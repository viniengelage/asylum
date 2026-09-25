use crate::tls::{RustlsConnect, SslMode};
use anyhow::{Context as _, anyhow};
use futures::{StreamExt as _, pin_mut};
use std::fmt;
use std::future::Future;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_postgres::error::SqlState;
use tokio_postgres::{CancelToken, Client, SimpleQueryMessage};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What to dial: a `postgres://` URL or a libpq `key=value` string, with the `sslmode` pulled out
/// because tokio-postgres rejects `verify-ca` and `verify-full`.
#[derive(Clone, Debug)]
pub struct ConnectTarget {
    pub config: tokio_postgres::Config,
    pub ssl_mode: SslMode,
}

impl ConnectTarget {
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        let (rest, ssl_mode) = take_ssl_mode(input.trim());
        let ssl_mode = match ssl_mode {
            Some(value) => SslMode::parse(&value)
                .with_context(|| format!("sslmode desconhecido: {value}"))?,
            None => SslMode::default(),
        };
        let mut config =
            tokio_postgres::Config::from_str(&rest).context("conexão do Postgres inválida")?;
        config.ssl_mode(ssl_mode.negotiation());
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(DEFAULT_CONNECT_TIMEOUT);
        }
        Ok(Self { config, ssl_mode })
    }
}

/// Removes the `sslmode` parameter from a URL query string or a `key=value` list and returns
/// the rest untouched, so quoted values elsewhere keep their spaces.
fn take_ssl_mode(input: &str) -> (String, Option<String>) {
    if input.starts_with("postgres://") || input.starts_with("postgresql://") {
        let Some((base, query)) = input.split_once('?') else {
            return (input.to_owned(), None);
        };
        let mut ssl_mode = None;
        let kept = query
            .split('&')
            .filter(|pair| match pair.strip_prefix("sslmode=") {
                Some(value) => {
                    ssl_mode = Some(value.to_owned());
                    false
                }
                None => true,
            })
            .collect::<Vec<_>>();
        let rest = if kept.is_empty() {
            base.to_owned()
        } else {
            format!("{base}?{}", kept.join("&"))
        };
        return (rest, ssl_mode);
    }

    let mut search_from = 0;
    while let Some(offset) = input[search_from..].find("sslmode") {
        let start = search_from + offset;
        let at_word_start = input[..start]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        let after_key = input[start + "sslmode".len()..].trim_start();
        if at_word_start && let Some(value_and_rest) = after_key.strip_prefix('=') {
            let value_and_rest = value_and_rest.trim_start();
            let value_len = value_and_rest
                .find(char::is_whitespace)
                .unwrap_or(value_and_rest.len());
            let value = value_and_rest[..value_len].to_owned();
            let end = input.len() - (value_and_rest.len() - value_len);
            let rest = format!("{}{}", &input[..start], &input[end..]);
            return (rest.trim().to_owned(), Some(value));
        }
        search_from = start + "sslmode".len();
    }
    (input.to_owned(), None)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// `None` when the server wasn't asked, as for scripts with more than one statement.
    pub type_name: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    /// Every value as the server prints it; `None` is SQL NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// More rows existed than the limit allowed.
    pub truncated: bool,
    /// The count from the command tag: rows returned by a SELECT, touched by an UPDATE, …
    pub rows_affected: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct QueryOutcome {
    pub result_sets: Vec<ResultSet>,
    pub elapsed: Duration,
}

/// An error the server raised, with the fields the editor needs to point at it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerError {
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// 1-based character offset inside the statement that failed.
    pub position: Option<u32>,
}

impl fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ERROR {}: {}", self.code, self.message)
    }
}

impl std::error::Error for ServerError {}

fn server_error(error: tokio_postgres::Error) -> anyhow::Error {
    match error.as_db_error() {
        Some(db_error) => anyhow::Error::new(ServerError {
            code: db_error.code().code().to_owned(),
            message: db_error.message().to_owned(),
            detail: db_error.detail().map(str::to_owned),
            hint: db_error.hint().map(str::to_owned),
            position: match db_error.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(position)) => Some(*position),
                _ => None,
            },
        }),
        None => anyhow::Error::new(error),
    }
}

/// Runs the future on the shared tokio runtime: sockets, timers and the cancel request all need
/// it, while callers await from GPUI's executors.
async fn on_runtime<T: Send + 'static>(
    future: impl Future<Output = anyhow::Result<T>> + Send + 'static,
) -> anyhow::Result<T> {
    reqwest_client::runtime()
        .spawn(future)
        .await
        .map_err(|error| anyhow!("a tarefa do Postgres parou: {error}"))?
}

/// One connection to the server. Statements sent through the same session share its
/// transaction state, so each SQL tab gets a session of its own.
pub struct Session {
    client: Arc<Client>,
    cancel_token: CancelToken,
    tls: RustlsConnect,
    target: ConnectTarget,
    pub server_version: String,
}

impl Session {
    pub async fn connect(target: &ConnectTarget) -> anyhow::Result<Self> {
        let tls = RustlsConnect::new(target.ssl_mode)?;
        let config = target.config.clone();
        let client = on_runtime({
            let tls = tls.clone();
            async move {
                let (client, connection) = config.connect(tls).await.map_err(server_error)?;
                // Dropping the JoinHandle detaches the task; it ends when the client is dropped
                // or the server closes the socket.
                drop(tokio::spawn(async move {
                    if let Err(error) = connection.await {
                        log::warn!("conexão com o Postgres encerrada: {error}");
                    }
                }));
                anyhow::Ok(client)
            }
        })
        .await?;
        let client = Arc::new(client);
        let cancel_token = client.cancel_token();
        let session = Self {
            client,
            cancel_token,
            tls,
            target: target.clone(),
            server_version: String::new(),
        };
        let server_version = session
            .query_text("show server_version")
            .await?
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next().flatten())
            .unwrap_or_default();
        Ok(Self {
            server_version,
            ..session
        })
    }

    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// A second connection with the same credentials whose transactions are all read-only, for
    /// views that run SQL the person typed (a table filter) without meaning to write.
    pub async fn connect_read_only(&self) -> anyhow::Result<Self> {
        let mut target = self.target.clone();
        let options = match target.config.get_options() {
            Some(options) if options.contains("default_transaction_read_only") => {
                options.to_owned()
            }
            Some(options) => format!("{options} -c default_transaction_read_only=on"),
            None => "-c default_transaction_read_only=on".to_owned(),
        };
        target.config.options(options);
        Self::connect(&target).await
    }

    /// Runs one statement or a whole script. Past `row_limit` rows in a result the rest of the
    /// query is cancelled on the server, which also aborts an open transaction and any
    /// statements after it in the script.
    pub async fn run(&self, sql: &str, row_limit: usize) -> anyhow::Result<QueryOutcome> {
        let client = self.client.clone();
        let cancel_token = self.cancel_token.clone();
        let tls = self.tls.clone();
        let sql = sql.to_owned();
        on_runtime(async move { run(client, cancel_token, tls, sql, row_limit).await }).await
    }

    /// Asks the server to stop whatever this session is running. It goes over a new socket, so a
    /// query that already finished is left alone.
    pub async fn cancel(&self) -> anyhow::Result<()> {
        let cancel_token = self.cancel_token.clone();
        let tls = self.tls.clone();
        on_runtime(async move {
            cancel_token
                .cancel_query(tls)
                .await
                .map_err(server_error)
        })
        .await
    }

    /// Rows of a query the client itself issues, without describing or limiting it.
    pub(crate) async fn query_text(&self, sql: &str) -> anyhow::Result<Vec<Vec<Option<String>>>> {
        let client = self.client.clone();
        let sql = sql.to_owned();
        on_runtime(async move {
            let messages = client.simple_query(&sql).await.map_err(server_error)?;
            Ok(messages
                .into_iter()
                .filter_map(|message| match message {
                    SimpleQueryMessage::Row(row) => Some(
                        (0..row.len())
                            .map(|index| row.try_get(index).ok().flatten().map(str::to_owned))
                            .collect(),
                    ),
                    _ => None,
                })
                .collect())
        })
        .await
    }
}

async fn run(
    client: Arc<Client>,
    cancel_token: CancelToken,
    tls: RustlsConnect,
    sql: String,
    row_limit: usize,
) -> anyhow::Result<QueryOutcome> {
    let started = Instant::now();

    // The simple protocol returns values as text but only names the columns; describing the
    // statement first gets their types. The server refuses to describe a script with several
    // statements (42601), in which case the headers go without types.
    let described_columns = match client.prepare(&sql).await {
        Ok(statement) => Some(
            statement
                .columns()
                .iter()
                .map(|column| Column {
                    name: column.name().to_owned(),
                    type_name: Some(column.type_().name().to_owned()),
                })
                .collect::<Vec<_>>(),
        ),
        Err(error) if error.code() == Some(&SqlState::SYNTAX_ERROR) => None,
        Err(error) => return Err(server_error(error)),
    };

    let stream = client
        .simple_query_raw(&sql)
        .await
        .map_err(server_error)?;
    pin_mut!(stream);

    let mut result_sets = Vec::new();
    let mut current: Option<ResultSet> = None;
    let mut limit_cancel = None;
    while let Some(message) = stream.next().await {
        match message {
            Ok(SimpleQueryMessage::RowDescription(columns)) => {
                current = Some(ResultSet {
                    columns: columns
                        .iter()
                        .map(|column| Column {
                            name: column.name().to_owned(),
                            type_name: None,
                        })
                        .collect(),
                    ..ResultSet::default()
                });
            }
            Ok(SimpleQueryMessage::Row(row)) => {
                let Some(result_set) = current.as_mut() else {
                    continue;
                };
                if result_set.rows.len() < row_limit {
                    result_set.rows.push(
                        (0..row.len())
                            .map(|index| row.try_get(index).ok().flatten().map(str::to_owned))
                            .collect(),
                    );
                } else if !result_set.truncated {
                    result_set.truncated = true;
                    limit_cancel = Some(tokio::spawn({
                        let cancel_token = cancel_token.clone();
                        let tls = tls.clone();
                        async move { cancel_token.cancel_query(tls).await }
                    }));
                }
            }
            Ok(SimpleQueryMessage::CommandComplete(count)) => {
                let mut result_set = current.take().unwrap_or_default();
                result_set.rows_affected = Some(count);
                result_sets.push(result_set);
            }
            Ok(_) => {}
            Err(error)
                if limit_cancel.is_some() && error.code() == Some(&SqlState::QUERY_CANCELED) =>
            {
                result_sets.extend(current.take());
                break;
            }
            Err(error) => return Err(server_error(error)),
        }
    }

    // Wait for the cancel request to be delivered before the session can send anything else,
    // or it could land on the next query instead.
    if let Some(limit_cancel) = limit_cancel {
        match limit_cancel.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::warn!("não deu para cancelar o resto da query: {error}"),
            Err(error) => log::warn!("não deu para cancelar o resto da query: {error}"),
        }
    }

    if let (Some(described_columns), [result_set]) =
        (described_columns, result_sets.as_mut_slice())
        && described_columns.len() == result_set.columns.len()
    {
        result_set.columns = described_columns;
    }

    Ok(QueryOutcome {
        result_sets,
        elapsed: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_mode_comes_out_of_a_url() {
        let (rest, mode) =
            take_ssl_mode("postgres://app@localhost:5432/trix?sslmode=verify-full&application_name=x");
        assert_eq!(rest, "postgres://app@localhost:5432/trix?application_name=x");
        assert_eq!(mode.as_deref(), Some("verify-full"));

        let (rest, mode) = take_ssl_mode("postgresql://localhost/db?sslmode=require");
        assert_eq!(rest, "postgresql://localhost/db");
        assert_eq!(mode.as_deref(), Some("require"));

        let (rest, mode) = take_ssl_mode("postgres://localhost/db");
        assert_eq!(rest, "postgres://localhost/db");
        assert_eq!(mode, None);
    }

    #[test]
    fn ssl_mode_comes_out_of_key_value_pairs() {
        let (rest, mode) =
            take_ssl_mode("host=db password='a b' sslmode = verify-ca dbname=trix");
        assert_eq!(rest, "host=db password='a b'  dbname=trix");
        assert_eq!(mode.as_deref(), Some("verify-ca"));

        let (rest, mode) = take_ssl_mode("host=db nosslmode=x");
        assert_eq!(rest, "host=db nosslmode=x");
        assert_eq!(mode, None);
    }

    #[test]
    fn target_defaults_to_prefer_and_keeps_verify_modes() {
        let target = ConnectTarget::parse("postgres://app@localhost/trix").unwrap();
        assert_eq!(target.ssl_mode, SslMode::Prefer);
        assert_eq!(target.config.get_dbname(), Some("trix"));

        let target =
            ConnectTarget::parse("postgres://app@localhost/trix?sslmode=verify-full").unwrap();
        assert_eq!(target.ssl_mode, SslMode::VerifyFull);
        assert_eq!(
            target.config.get_ssl_mode(),
            tokio_postgres::config::SslMode::Require
        );

        assert!(ConnectTarget::parse("postgres://localhost/db?sslmode=sometimes").is_err());
    }
}
