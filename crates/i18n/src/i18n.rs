//! Translations for Asylum's own interface.
//!
//! Strings live in `assets/locales/<language>/<namespace>.json` as flat `"key": "text"` maps and
//! are looked up with [`t!`] as `"<namespace>.<key>"`. A missing translation falls back to
//! English, and a missing English string to the key itself, so a typo shows up on screen instead
//! of panicking. The tests check that every language has the same keys and placeholders, and that
//! every key used with [`t!`] exists.

use std::{
    collections::HashMap,
    fmt::Display,
    sync::{
        LazyLock,
        atomic::{AtomicU8, Ordering},
    },
};

use gpui::{App, SharedString};
use settings::{RegisterSetting, Settings, SettingsStore, UiLanguageContent};

util::fs_embed! {
    struct Locales,
    crate_relative = "../../assets/locales",
    root_relative = "assets/locales",
    include = ["**/*.json"],
}

/// Looks up a translated string by `"<namespace>.<key>"`.
///
/// Placeholders written as `{name}` in the translation are filled from `name = value` arguments:
///
/// ```ignore
/// t!("docker.containers.title")
/// t!("docker.containers.count", count = containers.len())
/// ```
#[macro_export]
macro_rules! t {
    ($key:literal $(,)?) => {
        $crate::translate($key)
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::translate_with(
            $key,
            &[$((stringify!($name), &$value as &dyn ::std::fmt::Display)),+],
        )
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Language {
    English = 0,
    PortugueseBrazil = 1,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::English, Language::PortugueseBrazil];

    /// The name of the folder under `assets/locales` holding this language's strings.
    pub fn code(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::PortugueseBrazil => "pt-BR",
        }
    }

    fn from_index(index: u8) -> Self {
        match index {
            1 => Language::PortugueseBrazil,
            _ => Language::English,
        }
    }

    fn from_setting(setting: UiLanguageContent) -> Self {
        match setting {
            UiLanguageContent::System => system_language(),
            UiLanguageContent::English => Language::English,
            UiLanguageContent::PortugueseBrazil => Language::PortugueseBrazil,
        }
    }
}

fn system_language() -> Language {
    match sys_locale::get_locale() {
        Some(locale) if locale.to_lowercase().starts_with("pt") => Language::PortugueseBrazil,
        _ => Language::English,
    }
}

// Read on every render without a `cx`, so it lives outside of GPUI's state.
static CURRENT_LANGUAGE: AtomicU8 = AtomicU8::new(Language::English as u8);

pub fn current_language() -> Language {
    Language::from_index(CURRENT_LANGUAGE.load(Ordering::Relaxed))
}

pub fn set_language(language: Language) {
    CURRENT_LANGUAGE.store(language as u8, Ordering::Relaxed);
}

type Table = HashMap<String, SharedString>;

static TABLES: LazyLock<[Table; 2]> = LazyLock::new(|| Language::ALL.map(load_table));

fn load_table(language: Language) -> Table {
    let mut table = Table::default();
    for (namespace, entries) in namespaces(language) {
        for (key, value) in entries {
            table.insert(format!("{namespace}.{key}"), value.into());
        }
    }
    table
}

fn namespaces(language: Language) -> Vec<(String, HashMap<String, String>)> {
    let prefix = format!("{}/", language.code());
    let mut namespaces = Vec::new();
    for path in Locales::iter() {
        let Some(namespace) = path
            .strip_prefix(&prefix)
            .and_then(|file_name| file_name.strip_suffix(".json"))
        else {
            continue;
        };
        let Some(file) = Locales::get(&path) else {
            continue;
        };
        match serde_json::from_slice::<HashMap<String, String>>(&file.data) {
            Ok(entries) => namespaces.push((namespace.to_string(), entries)),
            Err(error) => log::error!("invalid locale file assets/locales/{path}: {error}"),
        }
    }
    namespaces
}

fn lookup(key: &'static str) -> SharedString {
    let tables = &*TABLES;
    tables[current_language() as usize]
        .get(key)
        .or_else(|| tables[Language::English as usize].get(key))
        .cloned()
        .unwrap_or_else(|| SharedString::new_static(key))
}

#[doc(hidden)]
pub fn translate(key: &'static str) -> SharedString {
    lookup(key)
}

#[doc(hidden)]
pub fn translate_with(key: &'static str, arguments: &[(&str, &dyn Display)]) -> SharedString {
    let mut text = lookup(key).to_string();
    for (name, value) in arguments {
        text = text.replace(&format!("{{{name}}}"), &value.to_string());
    }
    text.into()
}

#[derive(RegisterSetting)]
pub struct UiLanguageSettings {
    pub ui_language: UiLanguageContent,
}

impl Settings for UiLanguageSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            ui_language: content.ui_language.unwrap_or_default(),
        }
    }
}

pub fn init(cx: &mut App) {
    apply_language_setting(cx);
    cx.observe_global::<SettingsStore>(apply_language_setting)
        .detach();
}

fn apply_language_setting(cx: &mut App) {
    let language = Language::from_setting(UiLanguageSettings::get_global(cx).ui_language);
    if language != current_language() {
        set_language(language);
        cx.refresh_windows();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use std::{collections::BTreeSet, path::Path};

    fn placeholders(text: &str) -> BTreeSet<String> {
        Regex::new(r"\{([a-z_][a-z0-9_]*)\}")
            .expect("valid regex")
            .captures_iter(text)
            .map(|captures| captures[1].to_string())
            .collect()
    }

    #[test]
    fn every_language_has_the_same_keys_and_placeholders() {
        let english: HashMap<String, HashMap<String, String>> =
            namespaces(Language::English).into_iter().collect();
        let mut problems = Vec::new();
        for language in Language::ALL {
            let other: HashMap<String, HashMap<String, String>> =
                namespaces(language).into_iter().collect();
            let all_namespaces: BTreeSet<&String> = english.keys().chain(other.keys()).collect();
            for namespace in all_namespaces {
                let empty = HashMap::new();
                let english_entries = english.get(namespace).unwrap_or(&empty);
                let entries = other.get(namespace).unwrap_or(&empty);
                let keys: BTreeSet<&String> = english_entries.keys().chain(entries.keys()).collect();
                for key in keys {
                    match (english_entries.get(key), entries.get(key)) {
                        (Some(english_text), Some(text)) => {
                            if placeholders(english_text) != placeholders(text) {
                                problems.push(format!(
                                    "{}: {namespace}.{key} has different placeholders than en",
                                    language.code()
                                ));
                            }
                        }
                        (None, _) => problems.push(format!("en: missing {namespace}.{key}")),
                        (_, None) => problems.push(format!(
                            "{}: missing {namespace}.{key}",
                            language.code()
                        )),
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn every_key_used_in_the_code_exists() {
        let english = load_table(Language::English);
        let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let usage = Regex::new(r#"\bt!\(\s*"([^"]+)""#).expect("valid regex");
        let mut missing = Vec::new();
        let mut directories = vec![crates_dir];
        while let Some(directory) = directories.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // This crate's own docs show `t!` with example keys.
                    let skipped = ["target", "i18n"];
                    if path
                        .file_name()
                        .is_some_and(|name| !skipped.iter().any(|skipped| name == *skipped))
                    {
                        directories.push(path);
                    }
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let Ok(source) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    for captures in usage.captures_iter(&source) {
                        if !english.contains_key(&captures[1]) {
                            missing.push(format!("{}: {}", path.display(), &captures[1]));
                        }
                    }
                }
            }
        }
        assert!(missing.is_empty(), "keys missing from assets/locales/en:\n{}", missing.join("\n"));
    }

    #[test]
    fn placeholders_are_filled() {
        set_language(Language::English);
        assert_eq!(
            translate_with("missing.{count} items", &[("count", &3)]).as_ref(),
            "missing.3 items"
        );
    }
}
