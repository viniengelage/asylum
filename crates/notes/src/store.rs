use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use collections::HashMap;
use fs::{Fs, RemoveOptions};
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

const NOTES_FOLDER_NAME: &str = "notes";
pub const PIN_SLOTS: u8 = 9;

pub fn global_notes_dir() -> PathBuf {
    paths::config_dir().join(NOTES_FOLDER_NAME)
}

pub fn project_notes_dir(worktree_root: &Path) -> PathBuf {
    worktree_root
        .join(paths::local_settings_folder_name())
        .join(NOTES_FOLDER_NAME)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    #[default]
    Note,
    /// A prompt parked from the agent's composer, to be resumed later.
    Draft,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NoteScope {
    Global,
    Project(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub path: PathBuf,
    pub scope: NoteScope,
    pub title: SharedString,
    pub kind: NoteKind,
    pub pin: Option<u8>,
    pub tags: Vec<String>,
    pub thread: Option<String>,
    pub body: String,
    pub modified: SystemTime,
    /// Set when the frontmatter couldn't be read; the file is shown with its name.
    pub invalid: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Frontmatter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default)]
    kind: NoteKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pin: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread: Option<String>,
}

fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let content = content.trim_start_matches('\u{feff}');
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (None, content);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, content)
}

pub fn title_from_text(text: &str) -> String {
    let line = text
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut title: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        title.push('…');
    }
    title
}

pub fn parse_note(path: &Path, text: &str, scope: NoteScope, modified: SystemTime) -> Note {
    let (frontmatter, body) = split_frontmatter(text);
    let (frontmatter, invalid) = match frontmatter {
        Some(yaml) if !yaml.trim().is_empty() => {
            match serde_yaml_ng::from_str::<Frontmatter>(yaml) {
                Ok(frontmatter) => (frontmatter, false),
                Err(_) => (Frontmatter::default(), true),
            }
        }
        _ => (Frontmatter::default(), false),
    };
    let body = if invalid { text } else { body };
    let body = body.trim_start_matches(['\n', '\r']).to_string();
    let title = frontmatter
        .title
        .filter(|title| !title.trim().is_empty() && !invalid)
        .unwrap_or_else(|| {
            let from_body = title_from_text(&body);
            if from_body.is_empty() || invalid {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                from_body
            }
        });
    Note {
        path: path.to_path_buf(),
        scope,
        title: title.into(),
        kind: frontmatter.kind,
        pin: frontmatter.pin.filter(|pin| (1..=PIN_SLOTS).contains(pin)),
        tags: frontmatter.tags,
        thread: frontmatter.thread,
        body,
        modified,
        invalid,
    }
}

pub fn serialize_note(
    title: &str,
    kind: NoteKind,
    pin: Option<u8>,
    tags: &[String],
    thread: Option<&str>,
    body: &str,
) -> Result<String> {
    let frontmatter = Frontmatter {
        title: Some(title.to_string()),
        kind,
        pin,
        tags: tags.to_vec(),
        thread: thread.map(str::to_string),
    };
    let yaml = serde_yaml_ng::to_string(&frontmatter)?;
    let mut text = format!("---\n{yaml}---\n\n{body}");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn slug(title: &str) -> String {
    let mut slug = String::new();
    for character in title.chars() {
        let character = match character {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            other => other.to_ascii_lowercase(),
        };
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "nota".to_string()
    } else {
        slug
    }
}

pub fn first_free_slot(notes: &[Note]) -> Option<u8> {
    (1..=PIN_SLOTS).find(|slot| notes.iter().all(|note| note.pin != Some(*slot)))
}

pub fn sort_notes(notes: &mut [Note]) {
    notes.sort_by(|a, b| match (a.pin, b.pin) {
        (Some(a_pin), Some(b_pin)) => a_pin.cmp(&b_pin),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.modified.cmp(&a.modified),
    });
}

pub struct NewNote {
    pub title: String,
    pub body: String,
    pub kind: NoteKind,
    pub scope: NoteScope,
    pub pin: Option<u8>,
    pub thread: Option<String>,
}

struct GlobalNotesStore(Entity<NotesStore>);

impl Global for GlobalNotesStore {}

pub struct NotesStore {
    fs: Arc<dyn Fs>,
    notes: HashMap<NoteScope, Vec<Note>>,
    watches: HashMap<NoteScope, Task<()>>,
}

impl NotesStore {
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalNotesStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalNotesStore>() {
            return store.0.clone();
        }
        let fs = <dyn Fs>::global(cx);
        let store = cx.new(|cx| Self::new(fs, cx));
        cx.set_global(GlobalNotesStore(store.clone()));
        store
    }

    fn new(fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            fs,
            notes: HashMap::default(),
            watches: HashMap::default(),
        };
        this.watch(NoteScope::Global, cx);
        this
    }

    fn dir(scope: &NoteScope) -> PathBuf {
        match scope {
            NoteScope::Global => global_notes_dir(),
            NoteScope::Project(root) => project_notes_dir(root),
        }
    }

    pub fn watch_project(&mut self, worktree_root: PathBuf, cx: &mut Context<Self>) {
        self.watch(NoteScope::Project(worktree_root), cx);
    }

    fn watch(&mut self, scope: NoteScope, cx: &mut Context<Self>) {
        if self.watches.contains_key(&scope) {
            return;
        }
        let fs = self.fs.clone();
        let dir = Self::dir(&scope);
        let task_scope = scope.clone();
        let task = cx.spawn(async move |this, cx| {
            let scope = task_scope;
            // A watch on a folder that doesn't exist yet never fires, so the user's is made
            // up front; a project's is left alone until it gets its first note.
            if scope == NoteScope::Global {
                fs.create_dir(&dir).await.log_err();
            }
            let notes = load_dir(fs.as_ref(), &dir, &scope).await;
            if this
                .update(cx, |this, cx| this.set_notes(scope.clone(), notes, cx))
                .is_err()
            {
                return;
            }
            let (mut events, _watcher) = fs.watch(&dir, Duration::from_millis(200)).await;
            while events.next().await.is_some() {
                let notes = load_dir(fs.as_ref(), &dir, &scope).await;
                if this
                    .update(cx, |this, cx| this.set_notes(scope.clone(), notes, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.watches.insert(scope, task);
    }

    fn set_notes(&mut self, scope: NoteScope, notes: Vec<Note>, cx: &mut Context<Self>) {
        self.notes.insert(scope, notes);
        cx.notify();
    }

    fn reload(&self, scope: NoteScope, cx: &mut Context<Self>) -> Task<()> {
        let fs = self.fs.clone();
        let dir = Self::dir(&scope);
        cx.spawn(async move |this, cx| {
            let notes = load_dir(fs.as_ref(), &dir, &scope).await;
            this.update(cx, |this, cx| this.set_notes(scope, notes, cx))
                .log_err();
        })
    }

    pub fn visible_notes(&self, worktree_roots: &[PathBuf]) -> Vec<Note> {
        let mut notes: Vec<Note> = self
            .notes
            .iter()
            .filter(|(scope, _)| match scope {
                NoteScope::Global => true,
                NoteScope::Project(root) => worktree_roots.contains(root),
            })
            .flat_map(|(_, notes)| notes.iter().cloned())
            .collect();
        sort_notes(&mut notes);
        notes
    }

    pub fn pinned(&self, slot: u8, worktree_roots: &[PathBuf]) -> Option<Note> {
        self.visible_notes(worktree_roots)
            .into_iter()
            .find(|note| note.pin == Some(slot))
    }

    pub fn create(&mut self, new_note: NewNote, cx: &mut Context<Self>) -> Task<Result<PathBuf>> {
        let fs = self.fs.clone();
        let dir = Self::dir(&new_note.scope);
        let scope = new_note.scope.clone();
        let title = if new_note.title.trim().is_empty() {
            title_from_text(&new_note.body)
        } else {
            new_note.title.trim().to_string()
        };
        cx.spawn(async move |this, cx| {
            fs.create_dir(&dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))?;
            let base = slug(&title);
            let mut path = dir.join(format!("{base}.md"));
            let mut suffix = 2;
            while fs.is_file(&path).await {
                path = dir.join(format!("{base}-{suffix}.md"));
                suffix += 1;
            }
            let text = serialize_note(
                &title,
                new_note.kind,
                new_note.pin,
                &[],
                new_note.thread.as_deref(),
                &new_note.body,
            )?;
            fs.atomic_write(path.clone(), text)
                .await
                .with_context(|| format!("writing {}", path.display()))?;
            this.update(cx, |this, cx| {
                this.watch(scope.clone(), cx);
                this.reload(scope, cx)
            })?
            .await;
            Ok(path)
        })
    }

    pub fn set_pin(
        &mut self,
        note: &Note,
        pin: Option<u8>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let text = serialize_note(
            &note.title,
            note.kind,
            pin,
            &note.tags,
            note.thread.as_deref(),
            &note.body,
        );
        self.write(note, text, cx)
    }

    fn write(&self, note: &Note, text: Result<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        let fs = self.fs.clone();
        let path = note.path.clone();
        let scope = note.scope.clone();
        cx.spawn(async move |this, cx| {
            fs.atomic_write(path.clone(), text?)
                .await
                .with_context(|| format!("writing {}", path.display()))?;
            this.update(cx, |this, cx| this.reload(scope, cx))?.await;
            Ok(())
        })
    }

    pub fn delete(&mut self, note: &Note, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.delete_path(note.path.clone(), note.scope.clone(), cx)
    }

    pub fn delete_path(
        &mut self,
        path: PathBuf,
        scope: NoteScope,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            fs.remove_file(&path, RemoveOptions::default())
                .await
                .with_context(|| format!("removing {}", path.display()))?;
            this.update(cx, |this, cx| this.reload(scope, cx))?.await;
            Ok(())
        })
    }
}

async fn load_dir(fs: &dyn Fs, dir: &Path, scope: &NoteScope) -> Vec<Note> {
    let Ok(mut entries) = fs.read_dir(dir).await else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    while let Some(entry) = entries.next().await {
        let Some(path) = entry.log_err() else {
            continue;
        };
        if path.extension().is_none_or(|extension| extension != "md") {
            continue;
        }
        let Some(text) = fs.load(&path).await.log_err() else {
            continue;
        };
        let modified = fs
            .metadata(&path)
            .await
            .ok()
            .flatten()
            .map(|metadata| metadata.mtime.timestamp_for_user())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        notes.push(parse_note(&path, &text, scope.clone(), modified));
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(pin: Option<u8>, seconds: u64) -> Note {
        parse_note(
            Path::new("/n/a.md"),
            "corpo",
            NoteScope::Global,
            SystemTime::UNIX_EPOCH + Duration::from_secs(seconds),
        )
        .with_pin(pin)
    }

    impl Note {
        fn with_pin(mut self, pin: Option<u8>) -> Self {
            self.pin = pin;
            self
        }
    }

    #[test]
    fn test_note_round_trips_through_its_file() {
        let text = serialize_note(
            "Rodar testes do pix",
            NoteKind::Note,
            Some(2),
            &["pix".to_string()],
            None,
            "docker compose exec api pnpm test --filter pix",
        )
        .expect("serializes");
        let parsed = parse_note(
            Path::new("/n/rodar-testes.md"),
            &text,
            NoteScope::Global,
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(parsed.title.as_ref(), "Rodar testes do pix");
        assert_eq!(parsed.pin, Some(2));
        assert_eq!(parsed.tags, vec!["pix".to_string()]);
        assert_eq!(
            parsed.body.trim(),
            "docker compose exec api pnpm test --filter pix"
        );
        assert!(!parsed.invalid);
    }

    #[test]
    fn test_plain_markdown_takes_its_title_from_the_first_line() {
        let parsed = parse_note(
            Path::new("/n/decisoes.md"),
            "# Decisões do rebalanceamento\n\ncorte às 15h",
            NoteScope::Global,
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(parsed.title.as_ref(), "Decisões do rebalanceamento");
        assert_eq!(parsed.kind, NoteKind::Note);
    }

    #[test]
    fn test_broken_frontmatter_keeps_the_text_and_uses_the_file_name() {
        let parsed = parse_note(
            Path::new("/n/quebrada.md"),
            "---\ntitle: [sem fim\n---\nconteúdo",
            NoteScope::Global,
            SystemTime::UNIX_EPOCH,
        );
        assert!(parsed.invalid);
        assert_eq!(parsed.title.as_ref(), "quebrada");
        assert!(parsed.body.contains("conteúdo"));
    }

    #[test]
    fn test_pinned_notes_come_first_and_free_slots_fill_from_one() {
        let mut notes = vec![
            note(None, 10),
            note(Some(3), 1),
            note(Some(1), 2),
            note(None, 20),
        ];
        sort_notes(&mut notes);
        let order: Vec<_> = notes.iter().map(|note| (note.pin, note.modified)).collect();
        assert_eq!(order[0].0, Some(1));
        assert_eq!(order[1].0, Some(3));
        assert_eq!(order[2].1, SystemTime::UNIX_EPOCH + Duration::from_secs(20));
        assert_eq!(first_free_slot(&notes), Some(2));
        let full: Vec<_> = (1..=PIN_SLOTS).map(|slot| note(Some(slot), 0)).collect();
        assert_eq!(first_free_slot(&full), None);
    }

    #[test]
    fn test_slug_is_ascii_and_never_empty() {
        assert_eq!(
            slug("Decisões do rebalanceamento!"),
            "decisoes-do-rebalanceamento"
        );
        assert_eq!(slug("   "), "nota");
        assert_eq!(
            title_from_text("\n\n## Contexto do app\nmais"),
            "Contexto do app"
        );
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    #[gpui::test]
    async fn test_store_creates_pins_and_deletes_notes(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ ".zed": {} })).await;
        let store = cx.new(|cx| NotesStore::new(fs.clone(), cx));
        let project_root = PathBuf::from("/project");
        store.update(cx, |store, cx| {
            store.watch_project(project_root.clone(), cx)
        });
        cx.run_until_parked();

        let create = store.update(cx, |store, cx| {
            store.create(
                NewNote {
                    title: "Rodar testes do pix".to_string(),
                    body: "pnpm test --filter pix".to_string(),
                    kind: NoteKind::Note,
                    scope: NoteScope::Global,
                    pin: None,
                    thread: None,
                },
                cx,
            )
        });
        cx.run_until_parked();
        let global_path = create.await.expect("creates the global note");
        assert_eq!(
            global_path,
            global_notes_dir().join("rodar-testes-do-pix.md")
        );

        let create = store.update(cx, |store, cx| {
            store.create(
                NewNote {
                    title: String::new(),
                    body: "Expo 52 e rotas em src/app".to_string(),
                    kind: NoteKind::Note,
                    scope: NoteScope::Project(project_root.clone()),
                    pin: Some(1),
                    thread: None,
                },
                cx,
            )
        });
        cx.run_until_parked();
        create.await.expect("creates the project note");

        let notes = store.read_with(cx, |store, _| {
            store.visible_notes(std::slice::from_ref(&project_root))
        });
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].pin, Some(1));
        assert_eq!(notes[0].title.as_ref(), "Expo 52 e rotas em src/app");
        assert_eq!(
            store.read_with(cx, |store, _| store.visible_notes(&[]).len()),
            1
        );

        let global_note = notes[1].clone();
        let pin = store.update(cx, |store, cx| store.set_pin(&global_note, Some(2), cx));
        cx.run_until_parked();
        pin.await.expect("pins");
        let pinned = store.read_with(cx, |store, _| {
            store.pinned(2, std::slice::from_ref(&project_root))
        });
        assert_eq!(
            pinned.map(|note| note.body.trim().to_string()),
            Some("pnpm test --filter pix".to_string())
        );

        let delete = store.update(cx, |store, cx| store.delete(&global_note, cx));
        cx.run_until_parked();
        delete.await.expect("deletes");
        assert!(!fs.is_file(&global_path).await);
        assert_eq!(
            store.read_with(cx, |store, _| store
                .visible_notes(std::slice::from_ref(&project_root))
                .len()),
            1
        );
    }
}
