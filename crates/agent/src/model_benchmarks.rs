use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use serde::{Deserialize, Serialize};

pub const DEFAULT_MODEL_BENCHMARKS: &str = include_str!("../../../assets/settings/benchmarks.md");

const MODEL_BENCHMARKS_FILE_NAME: &str = "benchmarks.md";

const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

pub fn user_model_benchmarks_path() -> PathBuf {
    paths::config_dir().join(MODEL_BENCHMARKS_FILE_NAME)
}

pub fn project_model_benchmarks_path(worktree_root: &Path) -> PathBuf {
    worktree_root
        .join(paths::local_settings_folder_name())
        .join(MODEL_BENCHMARKS_FILE_NAME)
}

pub fn is_model_benchmarks_path(path: &Path) -> bool {
    if path == user_model_benchmarks_path() {
        return true;
    }
    path.file_name()
        .is_some_and(|name| name == MODEL_BENCHMARKS_FILE_NAME)
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == paths::local_settings_folder_name())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BenchmarkTier {
    #[serde(rename = "alto")]
    High,
    #[serde(rename = "médio")]
    Medium,
    #[serde(rename = "baixo")]
    Low,
}

impl BenchmarkTier {
    fn parse(text: &str) -> Option<Self> {
        match text.to_lowercase().as_str() {
            "alto" | "high" => Some(Self::High),
            "médio" | "medio" | "medium" => Some(Self::Medium),
            "baixo" | "low" => Some(Self::Low),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkRow {
    /// 1-based line of the row in the file.
    pub line: usize,
    pub id: String,
    /// `None` when the row holds for any effort of the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub tier: BenchmarkTier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_bench: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontiercode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub good_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avoid_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

impl BenchmarkRow {
    pub fn matches_model(&self, model_id: &str) -> bool {
        model_name(&self.id) == model_name(model_id)
    }

    fn matches_effort(&self, effort: Option<&str>) -> bool {
        match (&self.effort, effort) {
            (None, _) => true,
            (Some(row_effort), Some(effort)) => row_effort == effort,
            (Some(_), None) => false,
        }
    }
}

/// The model part of `provider/model`, so a row holds for the same model on any provider.
fn model_name(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id).trim()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BenchmarkWarningKind {
    MissingColumns(Vec<&'static str>),
    WrongColumnCount { expected: usize, found: usize },
    MissingId,
    InvalidTier(String),
    InvalidEffort(String),
    DuplicateRow { id: String, effort: Option<String> },
    NoTable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BenchmarkWarning {
    /// 1-based; `None` for problems with the file as a whole.
    pub line: Option<usize>,
    pub kind: BenchmarkWarningKind,
}

impl fmt::Display for BenchmarkWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(formatter, "line {line}: ")?;
        }
        match &self.kind {
            BenchmarkWarningKind::MissingColumns(columns) => write!(
                formatter,
                "the table header lacks the columns {}",
                columns.join(", ")
            ),
            BenchmarkWarningKind::WrongColumnCount { expected, found } => {
                write!(formatter, "expected {expected} cells, found {found}")
            }
            BenchmarkWarningKind::MissingId => write!(formatter, "the row has no id"),
            BenchmarkWarningKind::InvalidTier(tier) => {
                write!(formatter, "tier must be alto, médio or baixo, not {tier:?}")
            }
            BenchmarkWarningKind::InvalidEffort(effort) => write!(
                formatter,
                "effort must be one of {}, not {effort:?}",
                EFFORT_LEVELS.join(", ")
            ),
            BenchmarkWarningKind::DuplicateRow { id, effort } => write!(
                formatter,
                "{id} already has a row for effort {}",
                effort.as_deref().unwrap_or("(any)")
            ),
            BenchmarkWarningKind::NoTable => write!(formatter, "the file has no models table"),
        }
    }
}

pub fn warning_message(warning: &BenchmarkWarning) -> SharedString {
    let line = warning.line.unwrap_or_default();
    match &warning.kind {
        BenchmarkWarningKind::MissingColumns(columns) => i18n::t!(
            "agent.model_benchmarks.warning.missing_columns",
            columns = columns.join(", ")
        ),
        BenchmarkWarningKind::WrongColumnCount { expected, found } => i18n::t!(
            "agent.model_benchmarks.warning.column_count",
            line = line,
            expected = expected,
            found = found
        ),
        BenchmarkWarningKind::MissingId => {
            i18n::t!("agent.model_benchmarks.warning.missing_id", line = line)
        }
        BenchmarkWarningKind::InvalidTier(tier) => i18n::t!(
            "agent.model_benchmarks.warning.invalid_tier",
            line = line,
            tier = tier
        ),
        BenchmarkWarningKind::InvalidEffort(effort) => i18n::t!(
            "agent.model_benchmarks.warning.invalid_effort",
            line = line,
            effort = effort
        ),
        BenchmarkWarningKind::DuplicateRow { id, .. } => i18n::t!(
            "agent.model_benchmarks.warning.duplicate",
            line = line,
            id = id
        ),
        BenchmarkWarningKind::NoTable => i18n::t!("agent.model_benchmarks.warning.no_table"),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelBenchmarks {
    pub reviewed: Option<String>,
    pub rows: Vec<BenchmarkRow>,
    pub rules: String,
    pub warnings: Vec<BenchmarkWarning>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Column {
    Id,
    Effort,
    Tier,
    TerminalBench,
    FrontierCode,
    Price,
    Cost,
    GoodFor,
    AvoidFor,
    Source,
    Date,
}

impl Column {
    fn parse(header: &str) -> Option<Self> {
        let header = header.trim().to_lowercase();
        let column = match header.as_str() {
            "id" | "modelo" | "model" => Self::Id,
            "effort" | "esforço" | "esforco" | "raciocínio" => Self::Effort,
            "tier" => Self::Tier,
            "terminal_bench" | "terminal-bench" | "terminal bench" => Self::TerminalBench,
            "frontiercode" | "frontier_code" => Self::FrontierCode,
            "preço in/out" | "preco in/out" | "preço" | "preco" | "price" | "price in/out" => {
                Self::Price
            }
            "custo" | "cost" => Self::Cost,
            "bom para" | "good for" | "good_for" => Self::GoodFor,
            "evitar para" | "avoid for" | "avoid_for" => Self::AvoidFor,
            "fonte" | "source" => Self::Source,
            "data" | "date" => Self::Date,
            _ => return None,
        };
        Some(column)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Other,
    Models,
    Rules,
}

fn table_cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    let inner = line.strip_prefix('|')?;
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    Some(
        inner
            .split('|')
            .map(|cell| cell.trim().to_string())
            .collect(),
    )
}

fn is_separator_row(cells: &[String]) -> bool {
    cells.iter().all(|cell| {
        let cell = cell.trim_matches(':');
        !cell.is_empty() && cell.chars().all(|character| character == '-')
    })
}

fn cell_value(cell: &str) -> Option<String> {
    let cell = cell.trim();
    (!cell.is_empty() && cell != "—" && cell != "-").then(|| cell.to_string())
}

impl ModelBenchmarks {
    pub fn parse(text: &str) -> Self {
        let mut benchmarks = Self::default();
        let mut section = Section::Other;
        let mut header: Option<Vec<Option<Column>>> = None;
        let mut table_done = false;
        let mut seen = HashSet::new();
        let mut rules = Vec::new();

        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            let trimmed = line.trim();

            if let Some(heading) = trimmed.strip_prefix("## ") {
                section = match heading.trim().to_lowercase().as_str() {
                    "modelos" | "models" => Section::Models,
                    "regras" | "rules" => Section::Rules,
                    _ => Section::Other,
                };
                if header.is_some() {
                    table_done = true;
                }
                continue;
            }

            if benchmarks.reviewed.is_none()
                && let Some(date) = trimmed
                    .strip_prefix("revisado:")
                    .or_else(|| trimmed.strip_prefix("reviewed:"))
            {
                benchmarks.reviewed = Some(date.trim().to_string());
                continue;
            }

            if section == Section::Rules {
                rules.push(line);
                continue;
            }

            if table_done {
                continue;
            }
            let Some(cells) = table_cells(trimmed) else {
                if header.is_some() {
                    table_done = true;
                }
                continue;
            };

            let Some(columns) = header.as_ref() else {
                let columns: Vec<Option<Column>> =
                    cells.iter().map(|cell| Column::parse(cell)).collect();
                let missing: Vec<&'static str> = [(Column::Id, "id"), (Column::Tier, "tier")]
                    .into_iter()
                    .filter(|(column, _)| !columns.contains(&Some(*column)))
                    .map(|(_, name)| name)
                    .collect();
                if missing.is_empty() {
                    header = Some(columns);
                } else {
                    benchmarks.warnings.push(BenchmarkWarning {
                        line: Some(line_number),
                        kind: BenchmarkWarningKind::MissingColumns(missing),
                    });
                    table_done = true;
                }
                continue;
            };

            if is_separator_row(&cells) {
                continue;
            }
            if cells.len() != columns.len() {
                benchmarks.warnings.push(BenchmarkWarning {
                    line: Some(line_number),
                    kind: BenchmarkWarningKind::WrongColumnCount {
                        expected: columns.len(),
                        found: cells.len(),
                    },
                });
                continue;
            }

            let value = |wanted: Column| {
                columns
                    .iter()
                    .position(|column| *column == Some(wanted))
                    .and_then(|position| cells.get(position))
                    .and_then(|cell| cell_value(cell))
            };
            let warn = |kind| BenchmarkWarning {
                line: Some(line_number),
                kind,
            };

            let Some(id) = value(Column::Id) else {
                benchmarks
                    .warnings
                    .push(warn(BenchmarkWarningKind::MissingId));
                continue;
            };
            let tier_text = value(Column::Tier).unwrap_or_default();
            let Some(tier) = BenchmarkTier::parse(&tier_text) else {
                benchmarks
                    .warnings
                    .push(warn(BenchmarkWarningKind::InvalidTier(tier_text)));
                continue;
            };
            let effort = value(Column::Effort).map(|effort| effort.to_lowercase());
            if let Some(effort) = effort.as_ref()
                && !EFFORT_LEVELS.contains(&effort.as_str())
            {
                benchmarks
                    .warnings
                    .push(warn(BenchmarkWarningKind::InvalidEffort(effort.clone())));
                continue;
            }
            if !seen.insert((model_name(&id).to_string(), effort.clone())) {
                benchmarks
                    .warnings
                    .push(warn(BenchmarkWarningKind::DuplicateRow { id, effort }));
                continue;
            }

            benchmarks.rows.push(BenchmarkRow {
                line: line_number,
                id,
                effort,
                tier,
                terminal_bench: value(Column::TerminalBench),
                frontiercode: value(Column::FrontierCode),
                price: value(Column::Price),
                cost: value(Column::Cost),
                good_for: value(Column::GoodFor),
                avoid_for: value(Column::AvoidFor),
                source: value(Column::Source),
                date: value(Column::Date),
            });
        }

        if header.is_none()
            && !benchmarks
                .warnings
                .iter()
                .any(|warning| matches!(warning.kind, BenchmarkWarningKind::MissingColumns(_)))
        {
            benchmarks.warnings.push(BenchmarkWarning {
                line: None,
                kind: BenchmarkWarningKind::NoTable,
            });
        }
        benchmarks.rules = rules.join("\n").trim().to_string();
        benchmarks
    }

    pub fn rows_for_model<'a>(
        &'a self,
        model_id: &'a str,
    ) -> impl Iterator<Item = &'a BenchmarkRow> + 'a {
        self.rows
            .iter()
            .filter(move |row| row.matches_model(model_id))
    }

    pub fn has_model(&self, model_id: &str) -> bool {
        self.rows_for_model(model_id).next().is_some()
    }

    /// The row for a model at an effort level; a row without effort holds for any level.
    pub fn row_for(&self, model_id: &str, effort: Option<&str>) -> Option<&BenchmarkRow> {
        let mut rows = self.rows.iter().filter(|row| row.matches_model(model_id));
        rows.clone()
            .find(|row| row.effort.as_deref() == effort)
            .or_else(|| rows.find(|row| row.matches_effort(effort)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelBenchmarksSource {
    Default,
    User(PathBuf),
    Project(PathBuf),
}

impl ModelBenchmarksSource {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Default => None,
            Self::User(path) | Self::Project(path) => Some(path),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedModelBenchmarks {
    pub source: ModelBenchmarksSource,
    pub benchmarks: ModelBenchmarks,
    pub warnings: Vec<BenchmarkWarning>,
    /// Set when the file couldn't be read or had no valid rows, so the default is in use.
    pub fallback: Option<String>,
}

impl LoadedModelBenchmarks {
    pub fn default_benchmarks() -> Self {
        let benchmarks = ModelBenchmarks::parse(DEFAULT_MODEL_BENCHMARKS);
        Self {
            source: ModelBenchmarksSource::Default,
            warnings: benchmarks.warnings.clone(),
            benchmarks,
            fallback: None,
        }
    }

    fn from_text(source: ModelBenchmarksSource, text: &str) -> Self {
        let benchmarks = ModelBenchmarks::parse(text);
        if benchmarks.rows.is_empty() {
            let warnings = benchmarks.warnings;
            return Self {
                source,
                warnings,
                fallback: Some("the file has no valid rows".to_string()),
                ..Self::default_benchmarks()
            };
        }
        Self {
            source,
            warnings: benchmarks.warnings.clone(),
            benchmarks,
            fallback: None,
        }
    }

    fn unreadable(source: ModelBenchmarksSource, error: anyhow::Error) -> Self {
        Self {
            source,
            warnings: Vec::new(),
            fallback: Some(format!("{error:#}")),
            ..Self::default_benchmarks()
        }
    }
}

async fn load_file(fs: &dyn Fs, source: ModelBenchmarksSource) -> Option<LoadedModelBenchmarks> {
    let path = source.path()?;
    if !fs.is_file(path).await {
        return None;
    }
    Some(match fs.load(path).await {
        Ok(text) => LoadedModelBenchmarks::from_text(source, &text),
        Err(error) => LoadedModelBenchmarks::unreadable(source, error),
    })
}

/// A project's `.zed/benchmarks.md` wins over the user's, which wins over the default.
pub async fn load_model_benchmarks(
    fs: Arc<dyn Fs>,
    worktree_roots: Vec<PathBuf>,
) -> LoadedModelBenchmarks {
    for root in worktree_roots {
        let source = ModelBenchmarksSource::Project(project_model_benchmarks_path(&root));
        if let Some(loaded) = load_file(fs.as_ref(), source).await {
            return loaded;
        }
    }
    let source = ModelBenchmarksSource::User(user_model_benchmarks_path());
    load_file(fs.as_ref(), source)
        .await
        .unwrap_or_else(LoadedModelBenchmarks::default_benchmarks)
}

pub async fn ensure_user_model_benchmarks(fs: Arc<dyn Fs>) -> Result<PathBuf> {
    let path = user_model_benchmarks_path();
    if !fs.is_file(&path).await {
        if let Some(directory) = path.parent() {
            fs.create_dir(directory)
                .await
                .with_context(|| format!("creating {}", directory.display()))?;
        }
        fs.atomic_write(path.clone(), DEFAULT_MODEL_BENCHMARKS.to_string())
            .await
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

/// Puts the default back, keeping the replaced file next to it as `benchmarks.md.bak`.
pub async fn restore_default_model_benchmarks(fs: Arc<dyn Fs>, path: PathBuf) -> Result<()> {
    if fs.is_file(&path).await {
        let backup = path.with_extension("md.bak");
        let current = fs
            .load(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        fs.atomic_write(backup.clone(), current)
            .await
            .with_context(|| format!("writing {}", backup.display()))?;
    }
    fs.atomic_write(path.clone(), DEFAULT_MODEL_BENCHMARKS.to_string())
        .await
        .with_context(|| format!("writing {}", path.display()))
}

pub fn append_model_rows(text: &str, model_ids: &[String]) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let header = lines.iter().position(|line| {
        table_cells(line).is_some_and(|cells| {
            let columns: Vec<_> = cells.iter().map(|cell| Column::parse(cell)).collect();
            columns.contains(&Some(Column::Id)) && columns.contains(&Some(Column::Tier))
        })
    });
    let Some(header) = header else {
        return text.to_string();
    };
    let Some(columns) = lines.get(header).and_then(|line| table_cells(line)) else {
        return text.to_string();
    };
    let table_end = lines
        .iter()
        .enumerate()
        .skip(header + 1)
        .find(|(_, line)| table_cells(line).is_none())
        .map_or(lines.len(), |(index, _)| index);

    let new_rows = model_ids.iter().map(|model_id| {
        let cells: Vec<&str> = columns
            .iter()
            .map(|column| match Column::parse(column) {
                Some(Column::Id) => model_id.as_str(),
                _ => "",
            })
            .collect();
        format!("| {} |", cells.join(" | "))
    });

    let mut output: Vec<String> = lines[..table_end]
        .iter()
        .map(|line| line.to_string())
        .collect();
    output.extend(new_rows);
    output.extend(lines[table_end..].iter().map(|line| line.to_string()));
    let mut joined = output.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

struct GlobalModelBenchmarksStore(Entity<ModelBenchmarksStore>);

impl Global for GlobalModelBenchmarksStore {}

pub struct ModelBenchmarksStore {
    loaded: LoadedModelBenchmarks,
    _watch: Task<()>,
}

impl ModelBenchmarksStore {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalModelBenchmarksStore>() {
            return store.0.clone();
        }
        let fs = <dyn Fs>::global(cx);
        let store = cx.new(|cx| Self::new(fs, cx));
        cx.set_global(GlobalModelBenchmarksStore(store.clone()));
        store
    }

    fn new(fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        let path = user_model_benchmarks_path();
        let (mut contents, watch) =
            settings::watch_config_file(cx.background_executor(), fs, path.clone());
        let task = cx.spawn(async move |this, cx| {
            let _watch = watch;
            while let Some(text) = contents.next().await {
                let loaded = if text.trim().is_empty() {
                    LoadedModelBenchmarks::default_benchmarks()
                } else {
                    LoadedModelBenchmarks::from_text(
                        ModelBenchmarksSource::User(path.clone()),
                        &text,
                    )
                };
                let updated = this.update(cx, |this, cx| {
                    this.loaded = loaded;
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        Self {
            loaded: LoadedModelBenchmarks::default_benchmarks(),
            _watch: task,
        }
    }

    pub fn loaded(&self) -> &LoadedModelBenchmarks {
        &self.loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_benchmarks_parse_without_warnings() {
        let benchmarks = ModelBenchmarks::parse(DEFAULT_MODEL_BENCHMARKS);
        assert_eq!(benchmarks.warnings, Vec::new());
        assert!(benchmarks.rows.len() >= 6);
        assert_eq!(benchmarks.reviewed.as_deref(), Some("2026-11-06"));
        assert!(benchmarks.rules.starts_with("- Escolha"));
        assert!(benchmarks.has_model("claude-haiku-5-5"));
    }

    #[test]
    fn test_rows_match_the_model_on_any_provider() {
        let benchmarks = ModelBenchmarks::parse(
            "## Modelos\n| id | effort | tier |\n|---|---|---|\n| anthropic/claude-sonnet-5-5 | high | médio |\n| anthropic/claude-opus-5-5 | | alto |\n",
        );
        assert_eq!(benchmarks.warnings, Vec::new());
        let row = benchmarks
            .row_for("claude-subscription/claude-sonnet-5-5", Some("high"))
            .expect("the row should match on another provider");
        assert_eq!(row.line, 4);
        assert_eq!(
            benchmarks.row_for("anthropic/claude-sonnet-5-5", Some("max")),
            None
        );
        let any_effort = benchmarks
            .row_for("anthropic/claude-opus-5-5", Some("max"))
            .expect("a row without effort holds for any level");
        assert_eq!(any_effort.tier, BenchmarkTier::High);
    }

    #[test]
    fn test_invalid_rows_are_left_out_with_their_line() {
        let benchmarks = ModelBenchmarks::parse(
            "revisado: 2026-11-06\n## Modelos\n| id | effort | tier |\n|---|---|---|\n| a/opus | xhigh | top |\n| a/haiku | turbo | baixo |\n| a/sonnet | high |\n| a/sonnet | high | médio |\n| a/sonnet | high | médio |\n| | | alto |\n\n## Regras\n- Seja barato.\n",
        );
        assert_eq!(benchmarks.rows.len(), 1);
        assert_eq!(benchmarks.rules, "- Seja barato.");
        let lines: Vec<_> = benchmarks
            .warnings
            .iter()
            .map(|warning| (warning.line, warning.kind.clone()))
            .collect();
        assert_eq!(
            lines,
            vec![
                (
                    Some(5),
                    BenchmarkWarningKind::InvalidTier("top".to_string())
                ),
                (
                    Some(6),
                    BenchmarkWarningKind::InvalidEffort("turbo".to_string())
                ),
                (
                    Some(7),
                    BenchmarkWarningKind::WrongColumnCount {
                        expected: 3,
                        found: 2
                    }
                ),
                (
                    Some(9),
                    BenchmarkWarningKind::DuplicateRow {
                        id: "a/sonnet".to_string(),
                        effort: Some("high".to_string())
                    }
                ),
                (Some(10), BenchmarkWarningKind::MissingId),
            ]
        );
    }

    #[test]
    fn test_file_without_table_is_reported() {
        let benchmarks = ModelBenchmarks::parse("# Benchmarks\n\nnada aqui\n");
        assert_eq!(
            benchmarks.warnings,
            vec![BenchmarkWarning {
                line: None,
                kind: BenchmarkWarningKind::NoTable
            }]
        );
        let loaded = LoadedModelBenchmarks::from_text(
            ModelBenchmarksSource::User(PathBuf::from("/c/benchmarks.md")),
            "# vazio",
        );
        assert!(loaded.fallback.is_some());
        assert!(!loaded.benchmarks.rows.is_empty());
    }

    #[test]
    fn test_append_model_rows_goes_after_the_table() {
        let text = "## Modelos\n| id | effort | tier |\n|---|---|---|\n| a/opus | xhigh | alto |\n\n## Regras\n- x\n";
        let appended = append_model_rows(text, &["openai/gpt-6-astra".to_string()]);
        assert_eq!(
            appended,
            "## Modelos\n| id | effort | tier |\n|---|---|---|\n| a/opus | xhigh | alto |\n| openai/gpt-6-astra |  |  |\n\n## Regras\n- x\n"
        );
        let benchmarks = ModelBenchmarks::parse(&appended);
        assert_eq!(
            benchmarks.warnings.first().map(|warning| warning.line),
            Some(Some(5))
        );
    }
}
