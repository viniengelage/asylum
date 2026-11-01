//! Keeps Asylum's translations of Zed's own interface alive across upstream merges.
//!
//! `assets/locales/catalog.json` maps every translated string that came from Zed to the file it
//! lives in and its original English text. After merging upstream, conflicting files can be taken
//! from upstream as-is and `apply` puts the `i18n::t!` calls back; `check` lists what upstream
//! changed, removed or added since the catalog's base commit.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
};

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
use regex::Regex;
use serde::{Deserialize, Serialize};

const CATALOG_PATH: &str = "assets/locales/catalog.json";
const LOCALES_DIR: &str = "assets/locales";

#[derive(Parser)]
pub struct I18nArgs {
    #[command(subcommand)]
    command: I18nCommand,
}

#[derive(Subcommand)]
enum I18nCommand {
    /// Replaces the UI strings of a crate with `i18n::t!` calls, adding them to the English
    /// strings and, when they came from Zed, to the catalog.
    Extract {
        /// The crate to extract, e.g. `editor`. Its name is also the namespace.
        crate_name: String,
        /// Lists what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Puts the catalog's `i18n::t!` calls back into files that were taken from upstream, e.g.
    /// after resolving a merge conflict with `git checkout --theirs`.
    Apply {
        /// Files to fix; all files in the catalog when empty.
        files: Vec<PathBuf>,
    },
    /// Compares the catalog with an upstream commit and the working tree.
    Check {
        /// The upstream commit to compare with.
        #[arg(long, default_value = "upstream/main")]
        upstream: String,
    },
    /// Records the merged upstream commit as the catalog's base and refreshes entries whose
    /// English text was updated to match upstream.
    Sync {
        #[arg(long, default_value = "upstream/main")]
        upstream: String,
    },
    /// Lists keys that have no translation in a language.
    Missing {
        #[arg(default_value = "pt-BR")]
        language: String,
    },
}

#[derive(Default, Serialize, Deserialize)]
struct Catalog {
    upstream_base: String,
    entries: BTreeMap<String, CatalogEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CatalogEntry {
    file: String,
    text: String,
}

pub fn run_i18n(args: I18nArgs) -> Result<()> {
    match args.command {
        I18nCommand::Extract {
            crate_name,
            dry_run,
        } => extract(&crate_name, dry_run),
        I18nCommand::Apply { files } => apply(files),
        I18nCommand::Check { upstream } => check(&upstream),
        I18nCommand::Sync { upstream } => sync(&upstream),
        I18nCommand::Missing { language } => missing(&language),
    }
}

/// A string literal passed to a UI API: the byte range of the whole literal (quotes included)
/// and its decoded text.
struct UiString {
    range: std::ops::Range<usize>,
    text: String,
    conversion: Conversion,
}

/// How the `SharedString` returned by `t!` must be adapted to the API's parameter type.
#[derive(Clone, Copy)]
enum Conversion {
    None,
    /// The API takes `&str`.
    Reference,
    /// The API takes `Cow<'static, str>` or `String`.
    Owned,
}

const LITERAL: &str = r#""((?:[^"\\]|\\.)*)""#;

static UI_CALLS: LazyLock<Vec<(Regex, Conversion)>> = LazyLock::new(|| {
    let prefixes: &[(&str, Conversion)] = &[
        (r"\bLabel::new\(\s*", Conversion::None),
        (r"\bHeadline::new\(\s*", Conversion::None),
        (r"\bListHeader::new\(\s*", Conversion::None),
        (r"\bListSubHeader::new\(\s*", Conversion::None),
        (
            r"\bButton::new\(\s*[^,()]+(?:\([^()]*\))?\s*,\s*",
            Conversion::None,
        ),
        (
            r"\bToast::new\(\s*[^,()]+(?:\([^()]*\))?\s*,\s*",
            Conversion::Owned,
        ),
        (
            r"\bTooltip::(?:text|simple|new|for_action|for_action_title|for_action_in|with_meta)\(\s*",
            Conversion::None,
        ),
        (r"\bMenuItem::(?:action|os_action)\(\s*", Conversion::None),
        (
            r"\.(?:entry|header|label|action|toggleable_entry|custom_entry|title|description|placeholder|tooltip_text|meta)\(\s*",
            Conversion::None,
        ),
        (r"\.child\(\s*", Conversion::None),
        (r"\bset_placeholder_text\(\s*", Conversion::Reference),
        (r"\bPickerAction::(?:header|button)\(\s*", Conversion::None),
    ];
    prefixes
        .iter()
        .map(|(prefix, conversion)| {
            let regex = Regex::new(&format!("{prefix}{LITERAL}"))
                .expect("UI call patterns are valid regexes");
            (regex, *conversion)
        })
        .collect()
});

// `"Text".into()` and friends are mostly UI text in Zed, but so are ids and keys; requiring a
// phrase (a space or trailing punctuation) keeps those out.
static CONVERTED_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"{LITERAL}\s*\.(?:into|to_string|to_owned)\(\)"))
        .expect("converted literal pattern is a valid regex")
});

fn is_phrase(text: &str) -> bool {
    text.trim().contains(' ') || text.ends_with('…') || text.ends_with("...") || text.ends_with('.')
}

static TEST_MODULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#\[cfg\(test\)\]\s*mod\s+\w+\s*\{").expect("valid regex"));

fn find_ui_strings(source: &str) -> Vec<UiString> {
    // Test modules sit at the end of a file by convention; their strings are fixtures.
    let end = TEST_MODULE
        .find(source)
        .map_or(source.len(), |found| found.start());
    let searched = &source[..end];
    let comment_ranges = line_comment_ranges(searched);
    let mut found: BTreeMap<usize, UiString> = BTreeMap::new();
    let patterns = UI_CALLS
        .iter()
        .map(|(regex, conversion)| (regex, *conversion, false))
        .chain([(&*CONVERTED_LITERAL, Conversion::None, true)]);
    for (regex, conversion, needs_phrase) in patterns {
        for captures in regex.captures_iter(searched) {
            let (Some(whole), Some(content)) = (captures.get(0), captures.get(1)) else {
                continue;
            };
            let literal_start = content.start() - 1;
            if comment_ranges
                .iter()
                .any(|range| range.contains(&whole.start()))
            {
                continue;
            }
            let Some(text) = unescape(content.as_str()) else {
                continue;
            };
            if !looks_like_ui_text(&text) || (needs_phrase && !is_phrase(&text)) {
                continue;
            }
            found.entry(literal_start).or_insert(UiString {
                range: literal_start..content.end() + 1,
                text,
                conversion,
            });
        }
    }
    found.into_values().collect()
}

fn line_comment_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        if let Some(position) = line.find("//") {
            // A `//` inside a string literal (e.g. a URL) is not a comment.
            let before = &line[..position];
            if before.matches('"').count() % 2 == 0 {
                ranges.push(offset + position..offset + line.len());
            }
        }
        offset += line.len();
    }
    ranges
}

fn unescape(content: &str) -> Option<String> {
    let mut text = String::with_capacity(content.len());
    let mut characters = content.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            text.push(character);
            continue;
        }
        match characters.next()? {
            'n' => text.push('\n'),
            't' => text.push('\t'),
            '\\' => text.push('\\'),
            '"' => text.push('"'),
            '\'' => text.push('\''),
            // Unicode escapes and line continuations are rare in UI text; leave them for a human.
            _ => return None,
        }
    }
    Some(text)
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 2);
    escaped.push('"');
    for character in text.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

fn looks_like_ui_text(text: &str) -> bool {
    let trimmed = text.trim();
    if !trimmed.chars().any(char::is_alphabetic) {
        return false;
    }
    if trimmed.contains("::") || trimmed.starts_with("http") || trimmed.contains('{') {
        return false;
    }
    // Identifiers such as `editor_mode` or `my-panel` are ids, not text.
    let is_identifier = trimmed
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
        && trimmed
            .chars()
            .next()
            .is_some_and(|first| first.is_lowercase());
    !is_identifier
}

fn replacement(key: &str, conversion: Conversion) -> String {
    let call = format!("i18n::t!(\"{key}\")");
    match conversion {
        Conversion::None => call,
        Conversion::Reference => format!("&{call}"),
        Conversion::Owned => format!("{call}.to_string()"),
    }
}

fn slug(text: &str) -> String {
    let words: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(6)
        .map(|word| word.to_lowercase())
        .collect();
    if words.is_empty() {
        "text".to_string()
    } else {
        words.join("_")
    }
}

fn repository_root() -> Result<PathBuf> {
    let output = git(&["rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(output.trim()))
}

#[allow(
    clippy::disallowed_methods,
    reason = "This runs only in a CLI environment"
)]
fn git(arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(arguments)
        .output()
        .with_context(|| format!("running git {}", arguments.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn file_at(commit: &str, file: &str) -> Option<String> {
    git(&["show", &format!("{commit}:{file}")]).ok()
}

fn load_catalog(root: &Path) -> Result<Catalog> {
    let path = root.join(CATALOG_PATH);
    if !path.exists() {
        return Ok(Catalog::default());
    }
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("reading {CATALOG_PATH}"))?;
    serde_json::from_str(&contents).with_context(|| format!("parsing {CATALOG_PATH}"))
}

fn save_catalog(root: &Path, catalog: &Catalog) -> Result<()> {
    let contents = serde_json::to_string_pretty(catalog)? + "\n";
    std::fs::write(root.join(CATALOG_PATH), contents)
        .with_context(|| format!("writing {CATALOG_PATH}"))
}

type Strings = serde_json::Map<String, serde_json::Value>;

fn locale_path(root: &Path, language: &str, namespace: &str) -> PathBuf {
    root.join(LOCALES_DIR)
        .join(language)
        .join(format!("{namespace}.json"))
}

fn load_strings(root: &Path, language: &str, namespace: &str) -> Result<Strings> {
    let path = locale_path(root, language, namespace);
    if !path.exists() {
        return Ok(Strings::new());
    }
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("parsing {}", path.display()))
}

fn save_strings(root: &Path, language: &str, namespace: &str, strings: &Strings) -> Result<()> {
    let path = locale_path(root, language, namespace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(strings)? + "\n";
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))
}

fn rust_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("reading {}", directory.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|name| name != "tests" && name != "examples")
                {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|extension| extension == "rs")
                && !path.file_stem().is_some_and(|stem| {
                    let stem = stem.to_string_lossy();
                    stem == "tests" || stem.ends_with("_tests")
                })
            {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn merge_base(upstream: &str) -> Result<String> {
    Ok(git(&["merge-base", "HEAD", upstream])?.trim().to_string())
}

fn extract(crate_name: &str, dry_run: bool) -> Result<()> {
    let root = repository_root()?;
    let source_dir = root.join("crates").join(crate_name).join("src");
    if !source_dir.is_dir() {
        bail!("no crate at {}", source_dir.display());
    }
    let mut catalog = load_catalog(&root)?;
    if catalog.upstream_base.is_empty() {
        catalog.upstream_base = merge_base("upstream/main")?;
    }
    let mut english = load_strings(&root, "en", crate_name)?;
    let mut keys_by_text: HashMap<(String, String), String> = HashMap::new();
    for (key, value) in &english {
        if let Some(text) = value.as_str() {
            let area = key.split('.').next().unwrap_or_default().to_string();
            keys_by_text.insert((area, text.to_string()), key.clone());
        }
    }

    let mut total = 0;
    let mut from_upstream = 0;
    for path in rust_files(&source_dir)? {
        let file = relative(&root, &path);
        let source = std::fs::read_to_string(&path)?;
        let ui_strings = find_ui_strings(&source);
        if ui_strings.is_empty() {
            continue;
        }
        let upstream_source = file_at(&catalog.upstream_base, &file);
        let area = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| crate_name.to_string());
        let mut rewritten = source.clone();
        for ui_string in ui_strings.iter().rev() {
            let key = match keys_by_text.get(&(area.clone(), ui_string.text.clone())) {
                Some(key) => key.clone(),
                None => {
                    let base = format!("{area}.{}", slug(&ui_string.text));
                    let mut key = base.clone();
                    let mut suffix = 2;
                    while english.contains_key(&key) {
                        key = format!("{base}_{suffix}");
                        suffix += 1;
                    }
                    english.insert(
                        key.clone(),
                        serde_json::Value::String(ui_string.text.clone()),
                    );
                    keys_by_text.insert((area.clone(), ui_string.text.clone()), key.clone());
                    key
                }
            };
            let full_key = format!("{crate_name}.{key}");
            let literal = escape(&ui_string.text);
            if upstream_source
                .as_deref()
                .is_some_and(|upstream| upstream.contains(&literal))
            {
                catalog.entries.insert(
                    full_key.clone(),
                    CatalogEntry {
                        file: file.clone(),
                        text: ui_string.text.clone(),
                    },
                );
                from_upstream += 1;
            }
            if dry_run {
                println!("{file}: {literal} -> {full_key}");
            }
            rewritten.replace_range(
                ui_string.range.clone(),
                &replacement(&full_key, ui_string.conversion),
            );
            total += 1;
        }
        if !dry_run {
            std::fs::write(&path, rewritten)?;
        }
    }

    if !dry_run {
        save_strings(&root, "en", crate_name, &english)?;
        save_catalog(&root, &catalog)?;
    }
    println!(
        "{crate_name}: {total} strings ({from_upstream} from Zed, recorded in the catalog). \
         Next: add `i18n.workspace = true` to the crate if needed, fix any `&'static str` APIs that \
         `cargo check` reports, then translate with `cargo xtask i18n missing`."
    );
    Ok(())
}

fn apply(files: Vec<PathBuf>) -> Result<()> {
    let root = repository_root()?;
    let catalog = load_catalog(&root)?;
    let mut entries_by_file: HashMap<String, Vec<(&String, &CatalogEntry)>> = HashMap::new();
    for (key, entry) in &catalog.entries {
        entries_by_file
            .entry(entry.file.clone())
            .or_default()
            .push((key, entry));
    }
    let files: BTreeSet<String> = if files.is_empty() {
        entries_by_file.keys().cloned().collect()
    } else {
        files
            .iter()
            .map(|file| {
                relative(
                    &root,
                    &std::fs::canonicalize(file).unwrap_or_else(|_| root.join(file)),
                )
            })
            .collect()
    };

    let mut total = 0;
    for file in files {
        let Some(entries) = entries_by_file.get(&file) else {
            continue;
        };
        let path = root.join(&file);
        let Ok(source) = std::fs::read_to_string(&path) else {
            println!("{file}: missing, skipped");
            continue;
        };
        let keys_by_text: HashMap<&str, &String> = entries
            .iter()
            .map(|(key, entry)| (entry.text.as_str(), *key))
            .collect();
        let mut rewritten = source.clone();
        let mut applied = 0;
        for ui_string in find_ui_strings(&source).iter().rev() {
            if let Some(key) = keys_by_text.get(ui_string.text.as_str()) {
                rewritten.replace_range(
                    ui_string.range.clone(),
                    &replacement(key, ui_string.conversion),
                );
                applied += 1;
            }
        }
        if applied > 0 {
            std::fs::write(&path, rewritten)?;
            println!("{file}: {applied} strings translated again");
            total += applied;
        }
    }
    println!("{total} strings translated again");
    Ok(())
}

fn check(upstream: &str) -> Result<()> {
    let root = repository_root()?;
    let catalog = load_catalog(&root)?;
    if catalog.upstream_base.is_empty() {
        bail!("the catalog has no upstream base yet; run `cargo xtask i18n extract` first");
    }
    let upstream_commit = git(&["rev-parse", upstream])?.trim().to_string();

    let mut changed = Vec::new();
    let mut lost = Vec::new();
    let mut upstream_files: HashMap<String, Option<String>> = HashMap::new();
    for (key, entry) in &catalog.entries {
        let upstream_source = upstream_files
            .entry(entry.file.clone())
            .or_insert_with(|| file_at(&upstream_commit, &entry.file));
        let literal = escape(&entry.text);
        match upstream_source {
            None => changed.push(format!("{key}: {} no longer exists upstream", entry.file)),
            Some(source) if !source.contains(&literal) => changed.push(format!(
                "{key}: {literal} is gone from {} upstream (changed, moved or removed)",
                entry.file
            )),
            Some(_) => {}
        }

        let working = std::fs::read_to_string(root.join(&entry.file)).unwrap_or_default();
        if !working.contains(&format!("\"{key}\"")) {
            if working.contains(&literal) {
                lost.push(format!(
                    "{key}: {} has the English text again; run `apply`",
                    entry.file
                ));
            } else {
                lost.push(format!("{key}: not used in {} anymore", entry.file));
            }
        }
    }

    let catalog_texts: HashSet<(&str, &str)> = catalog
        .entries
        .values()
        .map(|entry| (entry.file.as_str(), entry.text.as_str()))
        .collect();
    let mut new_strings = Vec::new();
    let changed_files = git(&[
        "diff",
        "--name-only",
        &catalog.upstream_base,
        &upstream_commit,
        "--",
        "crates/*.rs",
    ])?;
    let translated_crates: HashSet<String> = catalog
        .entries
        .values()
        .filter_map(|entry| entry.file.split('/').nth(1).map(str::to_string))
        .collect();
    for file in changed_files.lines() {
        let crate_name = file.split('/').nth(1).unwrap_or_default();
        if !translated_crates.contains(crate_name) {
            continue;
        }
        let Some(source) = file_at(&upstream_commit, file) else {
            continue;
        };
        let base_source = file_at(&catalog.upstream_base, file).unwrap_or_default();
        for ui_string in find_ui_strings(&source) {
            let literal = escape(&ui_string.text);
            if !base_source.contains(&literal)
                && !catalog_texts.contains(&(file, ui_string.text.as_str()))
            {
                new_strings.push(format!("{file}: {literal}"));
            }
        }
    }

    print_section(
        "Changed upstream (review the English and pt-BR text, then `sync`)",
        &changed,
    );
    print_section("Lost in the working tree", &lost);
    print_section(
        "New upstream strings in translated crates (run `extract` on their crate)",
        &new_strings,
    );
    if changed.is_empty() && lost.is_empty() && new_strings.is_empty() {
        println!("The catalog is up to date with {upstream}.");
    }
    Ok(())
}

fn print_section(title: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    println!("{title} ({}):", lines.len());
    for line in lines {
        println!("  {line}");
    }
    println!();
}

fn sync(upstream: &str) -> Result<()> {
    let root = repository_root()?;
    let mut catalog = load_catalog(&root)?;
    let upstream_commit = git(&["rev-parse", upstream])?.trim().to_string();
    let mut english_by_namespace: HashMap<String, Strings> = HashMap::new();
    let mut still_stale = Vec::new();
    for (key, entry) in catalog.entries.iter_mut() {
        let Some(source) = file_at(&upstream_commit, &entry.file) else {
            still_stale.push(key.clone());
            continue;
        };
        if source.contains(&escape(&entry.text)) {
            continue;
        }
        let Some((namespace, local_key)) = key.split_once('.') else {
            continue;
        };
        if !english_by_namespace.contains_key(namespace) {
            english_by_namespace
                .insert(namespace.to_string(), load_strings(&root, "en", namespace)?);
        }
        let english_text = english_by_namespace
            .get(namespace)
            .and_then(|strings| strings.get(local_key))
            .and_then(|value| value.as_str());
        match english_text {
            Some(text) if source.contains(&escape(text)) => entry.text = text.to_string(),
            _ => still_stale.push(key.clone()),
        }
    }
    catalog.upstream_base = upstream_commit;
    save_catalog(&root, &catalog)?;
    if still_stale.is_empty() {
        println!("Catalog synced with {upstream}.");
    } else {
        println!(
            "Catalog base moved to {upstream}; {} entries still don't match upstream:",
            still_stale.len()
        );
        for key in still_stale {
            println!("  {key}");
        }
    }
    Ok(())
}

fn missing(language: &str) -> Result<()> {
    let root = repository_root()?;
    let english_dir = root.join(LOCALES_DIR).join("en");
    let mut total = 0;
    let mut namespaces: Vec<PathBuf> = std::fs::read_dir(&english_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    namespaces.sort();
    for path in namespaces {
        let Some(namespace) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            continue;
        };
        let english = load_strings(&root, "en", &namespace)?;
        let translated = load_strings(&root, language, &namespace)?;
        let missing: Vec<&String> = english
            .keys()
            .filter(|key| !translated.contains_key(*key))
            .collect();
        if missing.is_empty() {
            continue;
        }
        println!("{namespace} ({}):", missing.len());
        for key in &missing {
            println!(
                "  {key}: {}",
                english
                    .get(*key)
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
            );
        }
        total += missing.len();
    }
    println!("{total} strings missing in {language}");
    Ok(())
}
