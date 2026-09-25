//! Connections the project already describes: URLs in `.env` files, a postgres service in
//! docker compose, and passwords from `~/.pgpass`. These are only suggestions; nothing connects
//! until the person picks one.

use crate::connection::{DEFAULT_PORT, UrlFields, parse_url};
use collections::HashMap;
use fs::Fs;
use std::path::{Path, PathBuf};

const ENV_FILES: [&str; 4] = [".env", ".env.local", ".env.development", ".env.development.local"];
const COMPOSE_FILES: [&str; 4] = [
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
];
const URL_VARIABLES: [&str; 4] = ["DATABASE_URL", "POSTGRES_URL", "POSTGRESQL_URL", "PG_URL"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Env { file: String, variable: String },
    Compose { file: String, service: String, image: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub source: Source,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub fields: UrlFields,
}

impl Suggestion {
    pub fn address(&self) -> String {
        format!("{}@{}:{}/{}", self.user, self.host, self.port, self.database)
    }

    fn from_fields(source: Source, fields: UrlFields) -> Option<Self> {
        let host = fields.host.clone()?;
        let user = fields.user.clone().unwrap_or_else(|| "postgres".to_owned());
        Some(Self {
            source,
            port: fields.port.unwrap_or(DEFAULT_PORT),
            database: fields.database.clone().unwrap_or_else(|| user.clone()),
            password: fields.password.clone(),
            host,
            user,
            fields,
        })
    }
}

pub async fn discover(fs: &dyn Fs, root: &Path) -> Vec<Suggestion> {
    let mut env = HashMap::default();
    let mut suggestions = Vec::new();
    for file in ENV_FILES {
        let path = root.join(file);
        let Ok(text) = fs.load(&path).await else {
            continue;
        };
        let variables = parse_env(&text);
        suggestions.extend(env_suggestions(file, &variables));
        env.extend(variables);
    }
    for file in COMPOSE_FILES {
        let path = root.join(file);
        let Ok(text) = fs.load(&path).await else {
            continue;
        };
        match compose_suggestions(file, &text, &env) {
            Ok(found) => suggestions.extend(found),
            Err(error) => log::info!("Banco: {file} não foi lido: {error:#}"),
        }
    }
    suggestions.dedup_by(|a, b| a.address() == b.address());
    suggestions
}

fn parse_env(text: &str) -> Vec<(String, String)> {
    dotenvy::from_read_iter(text.as_bytes())
        .filter_map(|item| item.ok())
        .collect()
}

fn env_suggestions(file: &str, variables: &[(String, String)]) -> Vec<Suggestion> {
    let lookup = |name: &str| {
        variables
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let mut suggestions = Vec::new();
    for (variable, value) in variables {
        let is_url_variable = URL_VARIABLES.contains(&variable.as_str())
            || variable.ends_with("_DATABASE_URL");
        let is_postgres_url = value.starts_with("postgres://") || value.starts_with("postgresql://");
        if !is_url_variable || !is_postgres_url {
            continue;
        }
        let Ok(fields) = parse_url(value) else {
            continue;
        };
        let source = Source::Env {
            file: file.to_owned(),
            variable: variable.clone(),
        };
        suggestions.extend(Suggestion::from_fields(source, fields));
    }
    if let Some(host) = lookup("PGHOST") {
        let fields = UrlFields {
            host: Some(host),
            port: lookup("PGPORT").and_then(|port| port.parse().ok()),
            database: lookup("PGDATABASE"),
            user: lookup("PGUSER"),
            password: lookup("PGPASSWORD"),
            ssl_mode: lookup("PGSSLMODE").and_then(|mode| crate::tls::SslMode::parse(&mode)),
        };
        let source = Source::Env {
            file: file.to_owned(),
            variable: "PGHOST".to_owned(),
        };
        suggestions.extend(Suggestion::from_fields(source, fields));
    }
    suggestions
}

/// Replaces `${NAME}` and `${NAME:-default}` the way compose does, from the project's `.env`.
fn interpolate(value: &str, env: &HashMap<String, String>) -> String {
    let mut result = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            result.push_str(&rest[start..]);
            return result;
        };
        let expression = &after[..end];
        let (name, default) = match expression.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (expression, None),
        };
        match env.get(name) {
            Some(value) if !value.is_empty() => result.push_str(value),
            _ => result.push_str(default.unwrap_or_default()),
        }
        rest = &after[end + 1..];
    }
    result.push_str(rest);
    result
}

fn yaml_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(text) => Some(text.clone()),
        serde_yaml::Value::Number(number) => Some(number.to_string()),
        serde_yaml::Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn compose_suggestions(
    file: &str,
    text: &str,
    env: &HashMap<String, String>,
) -> anyhow::Result<Vec<Suggestion>> {
    let document: serde_yaml::Value = serde_yaml::from_str(text)?;
    let Some(services) = document.get("services").and_then(|services| services.as_mapping())
    else {
        return Ok(Vec::new());
    };
    let mut suggestions = Vec::new();
    for (name, service) in services {
        let (Some(name), Some(image)) = (
            name.as_str(),
            service.get("image").and_then(yaml_string),
        ) else {
            continue;
        };
        let image = interpolate(&image, env);
        let repository = image.rsplit('/').next().unwrap_or(&image);
        if !repository.starts_with("postgres") && !repository.starts_with("postgis") {
            continue;
        }
        let environment = compose_environment(service, env);
        let user = environment
            .get("POSTGRES_USER")
            .cloned()
            .unwrap_or_else(|| "postgres".to_owned());
        let fields = UrlFields {
            host: Some("localhost".to_owned()),
            port: service.get("ports").and_then(|ports| published_port(ports, env)),
            database: Some(
                environment
                    .get("POSTGRES_DB")
                    .cloned()
                    .unwrap_or_else(|| user.clone()),
            ),
            user: Some(user),
            password: environment.get("POSTGRES_PASSWORD").cloned(),
            ssl_mode: Some(crate::tls::SslMode::Disable),
        };
        // A service that publishes no port isn't reachable from the host.
        if fields.port.is_none() {
            continue;
        }
        let source = Source::Compose {
            file: file.to_owned(),
            service: name.to_owned(),
            image: image.clone(),
        };
        suggestions.extend(Suggestion::from_fields(source, fields));
    }
    Ok(suggestions)
}

fn compose_environment(
    service: &serde_yaml::Value,
    env: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut result = HashMap::default();
    match service.get("environment") {
        Some(serde_yaml::Value::Mapping(mapping)) => {
            for (key, value) in mapping {
                if let (Some(key), Some(value)) = (key.as_str(), yaml_string(value)) {
                    result.insert(key.to_owned(), interpolate(&value, env));
                }
            }
        }
        Some(serde_yaml::Value::Sequence(items)) => {
            for item in items.iter().filter_map(|item| item.as_str()) {
                if let Some((key, value)) = item.split_once('=') {
                    result.insert(key.to_owned(), interpolate(value, env));
                }
            }
        }
        _ => {}
    }
    result
}

/// The host port that maps to the container's 5432, in the short (`"5433:5432"`,
/// `"127.0.0.1:5433:5432"`) or long (`{target: 5432, published: 5433}`) syntax.
fn published_port(ports: &serde_yaml::Value, env: &HashMap<String, String>) -> Option<u16> {
    for port in ports.as_sequence()? {
        if let Some(short) = yaml_string(port) {
            let short = interpolate(&short, env);
            let short = short.split('/').next().unwrap_or(&short);
            let parts = short.rsplit(':').collect::<Vec<_>>();
            match parts.as_slice() {
                [container, host, ..] if *container == "5432" => {
                    if let Ok(host) = host.parse() {
                        return Some(host);
                    }
                }
                [container] if *container == "5432" => return Some(DEFAULT_PORT),
                _ => {}
            }
        } else if port.get("target").and_then(yaml_string).as_deref() == Some("5432") {
            let published = port
                .get("published")
                .and_then(yaml_string)
                .and_then(|published| interpolate(&published, env).parse().ok());
            if let Some(published) = published {
                return Some(published);
            }
        }
    }
    None
}

pub fn pgpass_path() -> PathBuf {
    util::paths::home_dir().join(".pgpass")
}

/// A password from a `.pgpass` file (`host:port:database:user:password`, `*` matches anything,
/// `\:` and `\\` escape), for the first line that matches.
pub fn pgpass_password(
    text: &str,
    host: &str,
    port: u16,
    database: &str,
    user: &str,
) -> Option<String> {
    let port = port.to_string();
    for line in text.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let fields = split_pgpass_line(line);
        let [line_host, line_port, line_database, line_user, password] = fields.as_slice() else {
            continue;
        };
        let matches = |field: &str, value: &str| field == "*" || field == value;
        if matches(line_host, host)
            && matches(line_port, &port)
            && matches(line_database, database)
            && matches(line_user, user)
        {
            return Some(password.clone());
        }
    }
    None
}

fn split_pgpass_line(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => {
                if let (Some(next), Some(field)) = (characters.next(), fields.last_mut()) {
                    field.push(next);
                }
            }
            ':' if fields.len() < 5 => fields.push(String::new()),
            _ => {
                if let Some(field) = fields.last_mut() {
                    field.push(character);
                }
            }
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_urls_and_pg_variables() {
        let variables = parse_env(
            "# comment\nDATABASE_URL=postgres://app:pw@localhost:5432/trix_core\n\
             REDIS_URL=redis://localhost\nANALYTICS_DATABASE_URL=\"postgresql://ro@analytics:6543/events\"\n\
             PGHOST=127.0.0.1\nPGUSER=me\n",
        );
        let suggestions = env_suggestions(".env", &variables);
        let addresses = suggestions
            .iter()
            .map(Suggestion::address)
            .collect::<Vec<_>>();
        assert_eq!(
            addresses,
            [
                "app@localhost:5432/trix_core",
                "ro@analytics:6543/events",
                "me@127.0.0.1:5432/me",
            ]
        );
        assert_eq!(suggestions[0].password.as_deref(), Some("pw"));
        assert_eq!(
            suggestions[0].source,
            Source::Env {
                file: ".env".into(),
                variable: "DATABASE_URL".into()
            }
        );
    }

    #[test]
    fn compose_service_with_short_and_long_ports() {
        let mut env = HashMap::default();
        env.insert("DB_PORT".to_owned(), "5433".to_owned());
        let compose = r#"
services:
  api:
    image: node:20
    ports: ["3000:3000"]
  db:
    image: postgres:16-alpine
    environment:
      POSTGRES_USER: app
      POSTGRES_PASSWORD: ${DB_PASSWORD:-local}
      POSTGRES_DB: trix_test
    ports:
      - "127.0.0.1:${DB_PORT}:5432"
  gis:
    image: postgis/postgis:16-3.4
    environment:
      - POSTGRES_PASSWORD=gis
    ports:
      - target: 5432
        published: 6432
  hidden:
    image: postgres:15
"#;
        let suggestions = compose_suggestions("docker-compose.yml", compose, &env).unwrap();
        let addresses = suggestions
            .iter()
            .map(Suggestion::address)
            .collect::<Vec<_>>();
        assert_eq!(
            addresses,
            [
                "app@localhost:5433/trix_test",
                "postgres@localhost:6432/postgres"
            ]
        );
        assert_eq!(suggestions[0].password.as_deref(), Some("local"));
        assert_eq!(suggestions[1].password.as_deref(), Some("gis"));
    }

    #[test]
    fn pgpass_matches_with_wildcards_and_escapes() {
        let pgpass = "# staging\nstaging-db:5432:*:app:p\\:w\\\\1\n*:*:*:postgres:any\n";
        assert_eq!(
            pgpass_password(pgpass, "staging-db", 5432, "trix", "app").as_deref(),
            Some("p:w\\1")
        );
        assert_eq!(
            pgpass_password(pgpass, "localhost", 5433, "x", "postgres").as_deref(),
            Some("any")
        );
        assert_eq!(pgpass_password(pgpass, "localhost", 5432, "x", "app"), None);
    }
}
