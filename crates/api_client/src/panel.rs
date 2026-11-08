use crate::{
    CopyRequestAsCurl, ExportPostmanCollection, ExportPostmanEnvironments, MatchSpec,
    ReimportPostman, ToggleFocus,
    collection::{self, Collection, LoginStatus},
    config::{
        self, Environment, HeaderEntry, LoginConfig, PostmanError, PostmanFile, PostmanImport,
        Variable,
    },
    request_view::{self, method_label},
    send,
    spec::{self, SchemeKind},
    vars::SECRET_PREFIX,
};
use anyhow::Context as _;
use collections::HashSet;
use editor::Editor;
use fs::Fs;
use gpui::{
    Action as _, AnyElement, AsyncApp, DismissEvent, Div, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, Pixels, PromptButton, PromptLevel, Stateful, Subscription,
    Task, WeakEntity, px, uniform_list,
};
use i18n::t;
use project::Project;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use ui::section_label;
use ui::{
    ContextMenu, ContextMenuEntry, EmptyState, KeyBinding, PopoverMenu, PopoverTriggerArea,
    StatusTone, SwitchField, ToggleState, Tooltip, prelude::*,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    ModalView, OpenOptions, Toast, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

/// Specs found in the project that aren't linked yet.
const MAX_CANDIDATES: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Start,
    Tree,
    Settings,
    Link,
}

struct Candidate {
    root: PathBuf,
    path: PathBuf,
    relative: String,
    summary: Option<Result<CandidateSummary, String>>,
}

#[derive(Clone)]
enum TreeRow {
    Folder {
        name: String,
        count: usize,
        expanded: bool,
    },
    Operation {
        key: String,
        method: String,
        title: String,
        deprecated: bool,
    },
    SavedHeader,
    Saved {
        id: String,
        name: String,
        method: String,
    },
}

struct SettingsForm {
    login_operation: Option<String>,
    refresh_operation: Option<String>,
    login_body: Entity<Editor>,
    token_path: Entity<InputField>,
    refresh_token_path: Entity<InputField>,
    expires_in_path: Entity<InputField>,
    refresh_body: Entity<InputField>,
    credentials: Vec<(String, Entity<InputField>)>,
    /// Secret name, what the field asks for, and the field.
    scheme_secrets: Vec<(String, String, Entity<InputField>)>,
    headers: Vec<(Entity<InputField>, Entity<InputField>, bool)>,
    environment_name: String,
    environment_variables: Vec<(Entity<InputField>, Entity<InputField>)>,
    new_environment: Entity<InputField>,
    renew_before_expiry: bool,
    retry_on_unauthorized: bool,
    remember_session: bool,
    dirty: bool,
}

pub struct ApiPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    collections: Vec<Entity<Collection>>,
    selected: usize,
    discovered: bool,
    candidates: Vec<Candidate>,
    selected_candidate: usize,
    link_task: Option<Task<()>>,
    link_error: Option<SharedString>,
    view: View,
    /// The operation (and saved request) of the active tab, marked in the tree.
    active_request: Option<(String, Option<String>)>,
    filter_input: Entity<InputField>,
    expanded: HashSet<String>,
    settings: Option<SettingsForm>,
    discover_task: Task<()>,
    collection_subscriptions: Vec<Subscription>,
    /// Re-renders the footer's "synced N min ago".
    _sync_clock: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl ApiPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let fs = workspace.app_state().fs.clone();
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, &t!("api_client.panel.filter_placeholder"))
                .start_icon(IconName::MagnifyingGlass)
                .end_hint("⌘F")
        });
        let mut subscriptions = Vec::new();
        {
            let editor = filter_input.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |_, cx| cx.notify()).log_err();
                    }
                }),
                window,
                cx,
            ));
        }
        subscriptions.push(cx.subscribe(&project, |this, _, event, cx| {
            if matches!(
                event,
                project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_)
            ) {
                this.discover(cx);
            }
        }));
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(
                cx.subscribe(&workspace_entity, |this, workspace, event, cx| {
                    if matches!(event, workspace::Event::ActiveItemChanged) {
                        let active_request = workspace
                            .read(cx)
                            .active_item(cx)
                            .and_then(|item| item.downcast::<request_view::ApiRequestView>())
                            .map(|view| {
                                let view = view.read(cx);
                                (view.operation_key.clone(), view.saved_id.clone())
                            });
                        if active_request != this.active_request {
                            this.active_request = active_request;
                            cx.notify();
                        }
                    }
                }),
            );
        }
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            fs,
            focus_handle: cx.focus_handle(),
            collections: Vec::new(),
            selected: 0,
            discovered: false,
            candidates: Vec::new(),
            selected_candidate: 0,
            link_task: None,
            link_error: None,
            view: View::Tree,
            active_request: None,
            filter_input,
            expanded: HashSet::default(),
            settings: None,
            discover_task: Task::ready(()),
            collection_subscriptions: Vec::new(),
            _sync_clock: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(60))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }),
            _subscriptions: subscriptions,
        };
        this.discover(cx);
        this
    }

    fn roots(&self, cx: &App) -> Vec<PathBuf> {
        self.project
            .read(cx)
            .visible_worktrees(cx)
            .filter(|worktree| {
                worktree
                    .read(cx)
                    .root_entry()
                    .is_some_and(|entry| entry.is_dir())
            })
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect()
    }

    /// Finds linked collections and, for the link screen, spec files not linked yet.
    fn discover(&mut self, cx: &mut Context<Self>) {
        let roots = self.roots(cx);
        let mut candidates: Vec<(PathBuf, PathBuf, String)> = Vec::new();
        for worktree in self.project.read(cx).visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let root = worktree.abs_path().to_path_buf();
            for entry in worktree.files(false, 0) {
                if candidates.len() >= MAX_CANDIDATES {
                    break;
                }
                let Some(file_name) = entry.path.file_name() else {
                    continue;
                };
                if spec::looks_like_spec_file(file_name) {
                    let relative = entry.path.as_unix_str().to_string();
                    candidates.push((root.clone(), root.join(entry.path.as_std_path()), relative));
                }
            }
        }
        let fs = self.fs.clone();
        self.discover_task = cx.spawn(async move |this, cx| {
            let mut files = Vec::new();
            for root in &roots {
                for file in collection::discover(&fs, root).await {
                    files.push((root.clone(), file));
                }
            }
            let mut found = Vec::new();
            for (root, path, relative) in candidates {
                let Ok(text) = fs.load(&path).await else {
                    continue;
                };
                if !spec::declares_spec_version(&text) {
                    continue;
                }
                let summary_path = path.clone();
                let summary = cx
                    .background_spawn(async move { summarize(&summary_path, &text) })
                    .await;
                found.push(Candidate {
                    root,
                    path,
                    relative,
                    summary: Some(summary),
                });
            }
            this.update(cx, |this, cx| {
                this.set_collections(files, cx);
                // The main spec is usually the biggest one; readable ones go before broken ones.
                found.sort_by_key(|candidate| {
                    std::cmp::Reverse(
                        candidate
                            .summary
                            .as_ref()
                            .and_then(|summary| summary.as_ref().ok())
                            .map_or(0, |summary| summary.operation_count),
                    )
                });
                this.candidates = found;
                this.selected_candidate = 0;
                this.discovered = true;
                cx.notify();
            })
            .log_err();
        });
    }

    fn set_collections(&mut self, files: Vec<(PathBuf, PathBuf)>, cx: &mut Context<Self>) {
        let mut kept = Vec::new();
        for (root, file) in files {
            let existing = self
                .collections
                .iter()
                .find(|collection| collection.read(cx).file_path == file)
                .cloned();
            let collection = existing.unwrap_or_else(|| {
                let fs = self.fs.clone();
                cx.new(|cx| Collection::new(root, file, fs, cx))
            });
            kept.push(collection);
        }
        self.collections = kept;
        self.selected = self.selected.min(self.collections.len().saturating_sub(1));
        self.collection_subscriptions = self
            .collections
            .iter()
            .map(|collection| cx.observe(collection, |_, _, cx| cx.notify()))
            .collect();
        if self.collections.is_empty() {
            if self.view != View::Link {
                self.view = View::Start;
            }
        } else if matches!(self.view, View::Link | View::Start) && self.link_task.is_none() {
            self.view = View::Tree;
        }
    }

    pub(crate) fn is_loaded(&self, cx: &App) -> bool {
        self.discovered
            && self
                .collection()
                .is_some_and(|collection| collection.read(cx).spec.is_some())
    }

    pub(crate) fn show_link(&mut self, cx: &mut Context<Self>) {
        self.view = View::Link;
        self.discover(cx);
    }

    pub(crate) fn debug_open_operation(
        &mut self,
        key: String,
        send: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tag) = self.collection().and_then(|collection| {
            collection
                .read(cx)
                .spec
                .as_ref()
                .and_then(|spec| spec.operation(&key))
                .and_then(|operation| operation.tags.first().cloned())
        }) {
            self.expanded.insert(tag);
        }
        self.open_operation(key, window, cx);
        if send
            && let Some(view) = self.workspace.upgrade().and_then(|workspace| {
                workspace
                    .read(cx)
                    .active_item_as::<request_view::ApiRequestView>(cx)
            })
        {
            view.update(cx, |view, cx| view.send(window, cx));
        }
    }

    fn collection(&self) -> Option<&Entity<Collection>> {
        self.collections.get(self.selected)
    }

    fn link(&mut self, cx: &mut Context<Self>) {
        let Some(candidate) = self.candidates.get(self.selected_candidate) else {
            return;
        };
        let fs = self.fs.clone();
        let root = candidate.root.clone();
        let path = candidate.path.clone();
        self.link_error = None;
        self.link_task = Some(cx.spawn(async move |this, cx| {
            let result = collection::create(fs.clone(), root.clone(), path, cx).await;
            this.update(cx, |this, cx| {
                this.link_task = None;
                match result {
                    Ok(file) => {
                        this.adopt_collection(root, file, cx);
                    }
                    Err(error) => this.link_error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    /// Adds a collection file just written and selects it.
    fn adopt_collection(
        &mut self,
        root: PathBuf,
        file: PathBuf,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Collection>> {
        let mut files: Vec<(PathBuf, PathBuf)> = self
            .collections
            .iter()
            .map(|collection| {
                let collection = collection.read(cx);
                (collection.root.clone(), collection.file_path.clone())
            })
            .filter(|(_, existing)| *existing != file)
            .collect();
        files.push((root, file.clone()));
        self.set_collections(files, cx);
        let index = self
            .collections
            .iter()
            .position(|collection| collection.read(cx).file_path == file)?;
        self.selected = index;
        self.view = View::Tree;
        cx.notify();
        self.collections.get(index).cloned()
    }

    fn show_toast(workspace: &WeakEntity<Workspace>, message: String, cx: &mut AsyncApp) {
        workspace
            .update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<ApiPanel>(), message).autohide(),
                    cx,
                );
            })
            .log_err();
    }

    fn import_modal(&self, cx: &mut Context<Self>) -> PostmanImportModal {
        PostmanImportModal {
            panel: cx.weak_entity(),
            workspace: self.workspace.clone(),
            target: self.collection().cloned(),
            root: self.roots(cx).into_iter().next(),
            fs: self.fs.clone(),
            focus_handle: cx.focus_handle(),
            files: Vec::new(),
            match_spec: true,
            importing: None,
            error: None,
        }
    }

    fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let modal = self.import_modal(cx);
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |_, _| modal);
            })
            .log_err();
    }

    pub(crate) fn export_collection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.collection() else {
            return;
        };
        let collection = collection.read(cx);
        let text = collection.postman_collection_text();
        let suggested = format!(
            "{}.postman_collection.json",
            config::slug(&collection.title())
        );
        let path = cx.prompt_for_new_path(&collection.root, Some(&suggested));
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Some(path) = path.await?? else {
                return anyhow::Ok(());
            };
            let result = async { fs.atomic_write(path.clone(), text?).await }.await;
            let message = match result {
                Ok(()) => t!("api_client.panel.export_done", path = path.display()).to_string(),
                Err(error) => format!("{error:#}"),
            };
            Self::show_toast(&workspace, message, cx);
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn export_environments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.collection() else {
            return;
        };
        let files = collection.read(cx).postman_environment_texts();
        let directory = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("api_client.panel.export_environments_postman")),
        });
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Some(directory) = directory.await??.and_then(|paths| paths.into_iter().next())
            else {
                return anyhow::Ok(());
            };
            let result = async {
                for (name, text) in files? {
                    fs.atomic_write(directory.join(name), text).await?;
                }
                anyhow::Ok(())
            }
            .await;
            let message = match result {
                Ok(()) => {
                    t!("api_client.panel.export_done", path = directory.display()).to_string()
                }
                Err(error) => format!("{error:#}"),
            };
            Self::show_toast(&workspace, message, cx);
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn match_spec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.collection().cloned() else {
            return;
        };
        let Some(outcome) =
            collection.update(cx, |collection, cx| collection.match_saved_with_spec(cx))
        else {
            return;
        };
        let message = t!(
            "api_client.panel.import_match_spec_detail",
            matched = outcome.matched,
            unmatched = outcome.unmatched
        )
        .to_string();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            Self::show_toast(&workspace, message, cx);
        })
        .detach();
    }

    pub(crate) fn remove_collection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.collection().cloned() else {
            return;
        };
        let (title, file) = {
            let collection = collection.read(cx);
            (
                collection.title(),
                collection
                    .file_path
                    .strip_prefix(&collection.root)
                    .unwrap_or(&collection.file_path)
                    .display()
                    .to_string(),
            )
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &t!("api_client.panel.remove_collection_confirm", name = &title),
            Some(&t!(
                "api_client.panel.remove_collection_detail",
                path = &file
            )),
            &[
                PromptButton::new(t!("api_client.panel.remove_collection")),
                PromptButton::Cancel(t!("api_client.panel.import_cancel")),
            ],
            cx,
        );
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            if answer.await? != 0 {
                return anyhow::Ok(());
            }
            let removed = collection.update(cx, |collection, cx| collection.remove(cx));
            if let Err(error) = removed.await {
                Self::show_toast(&workspace, format!("{error:#}"), cx);
            }
            this.update(cx, |this, cx| this.discover(cx))?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn open_operation(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.collection().cloned() else {
            return;
        };
        request_view::open(self.workspace.clone(), collection, key, None, window, cx);
    }

    fn open_saved(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.collection().cloned() else {
            return;
        };
        let Some(saved) = collection
            .read(cx)
            .file
            .saved
            .iter()
            .find(|saved| saved.id == id)
            .cloned()
        else {
            return;
        };
        request_view::open(
            self.workspace.clone(),
            collection,
            saved.operation.clone(),
            Some((saved.id, saved.name, saved.draft)),
            window,
            cx,
        );
    }

    fn open_spec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self
            .collection()
            .map(|collection| collection.read(cx).spec_path())
        else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_abs_path(path, OpenOptions::default(), window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn tree_rows(&self, cx: &App) -> Vec<TreeRow> {
        let Some(collection) = self.collection() else {
            return Vec::new();
        };
        let collection = collection.read(cx);
        let spec = collection.spec.as_ref();
        if spec.is_none() && !collection.file.spec.is_empty() {
            return Vec::new();
        }
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let matches = |operation: &spec::Operation, tag: &str| -> bool {
            if filter.is_empty() {
                return true;
            }
            let haystack = format!(
                "{} {} {} {}",
                operation.method,
                operation.path,
                operation.title(),
                tag
            )
            .to_lowercase();
            filter
                .split_whitespace()
                .all(|word| haystack.contains(word))
        };
        let mut rows = Vec::new();
        for (tag, operations) in spec.map(|spec| spec.folders()).unwrap_or_default() {
            let visible: Vec<_> = operations
                .into_iter()
                .filter(|operation| matches(operation, &tag))
                .collect();
            if visible.is_empty() {
                continue;
            }
            let expanded = !filter.is_empty() || self.expanded.contains(&tag);
            rows.push(TreeRow::Folder {
                name: tag.clone(),
                count: visible.len(),
                expanded,
            });
            if expanded {
                for operation in visible {
                    rows.push(TreeRow::Operation {
                        key: operation.key.clone(),
                        method: operation.method.clone(),
                        title: operation.title(),
                        deprecated: operation.deprecated,
                    });
                }
            }
        }
        let saved: Vec<_> = collection
            .file
            .saved
            .iter()
            .filter(|saved| filter.is_empty() || saved.name.to_lowercase().contains(&filter))
            .collect();
        if !saved.is_empty() {
            rows.push(TreeRow::SavedHeader);
            for saved in saved {
                let method = spec
                    .and_then(|spec| spec.operation(&saved.operation))
                    .map(|operation| operation.method.clone())
                    .or_else(|| saved.method.clone())
                    .unwrap_or_else(|| "?".to_string());
                rows.push(TreeRow::Saved {
                    id: saved.id.clone(),
                    name: saved.name.clone(),
                    method,
                });
            }
        }
        rows
    }

    fn render_row(&self, index: usize, row: TreeRow, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let hover = colors.element_hover;
        let accent = colors.border_focused;
        let base = h_flex()
            .id(("api-row", index))
            .group("api-row")
            .w_full()
            .h(px(26.))
            .pr_2()
            .gap_1p5()
            .rounded(Radius::Md.rems())
            .cursor_pointer()
            .hover(move |style| style.bg(hover));
        let mark = move |row: Stateful<Div>, selected: bool| {
            row.when(selected, |row| row.bg(accent.opacity(0.16)))
        };
        match row {
            TreeRow::Folder {
                name,
                count,
                expanded,
            } => base
                .pl_1()
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(name.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM)
                            .truncate(),
                    ),
                )
                .child(
                    Label::new(count.to_string())
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.expanded.remove(&name) {
                        this.expanded.insert(name.clone());
                    }
                    cx.notify();
                }))
                .into_any_element(),
            TreeRow::Operation {
                key,
                method,
                title,
                deprecated,
            } => {
                let selected = self
                    .active_request
                    .as_ref()
                    .is_some_and(|(open, saved)| *open == key && saved.is_none());
                mark(base, selected)
                    .pl(px(30.))
                    .when(deprecated, |this| this.opacity(0.5))
                    .child(
                        div()
                            .flex_none()
                            .w(px(44.))
                            .child(method_label(&method, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(title).size(LabelSize::Small).truncate()),
                    )
                    .when(deprecated, |this| {
                        this.child(
                            Label::new(t!("api_client.panel.deprecated"))
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_operation(key.clone(), window, cx);
                    }))
                    .into_any_element()
            }
            TreeRow::SavedHeader => div()
                .id(("api-row", index))
                .w_full()
                .pt_3()
                .pb_1()
                .px_1()
                .child(ui::section_header(
                    t!("api_client.panel.saved_by_me_section"),
                    None,
                ))
                .into_any_element(),
            TreeRow::Saved { id, name, method } => {
                let delete_id = id.clone();
                let failing = self
                    .collection()
                    .map_or(0, |collection| collection.read(cx).failing_tests(&id));
                let selected = self
                    .active_request
                    .as_ref()
                    .is_some_and(|(_, saved)| saved.as_deref() == Some(id.as_str()));
                mark(base, selected)
                    .pl(px(30.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(44.))
                            .child(method_label(&method, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(name).size(LabelSize::Small).truncate()),
                    )
                    .when(failing > 0, |this| {
                        this.child(
                            h_flex()
                                .id(("api-saved-failing", index))
                                .flex_none()
                                .gap_1()
                                .child(StatusTone::Error.dot(cx))
                                .child(
                                    Label::new(failing.to_string())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(Color::Error),
                                )
                                .tooltip(Tooltip::text(if failing == 1 {
                                    t!("api_client.panel.failing_tests_tooltip.one")
                                } else {
                                    t!(
                                        "api_client.panel.failing_tests_tooltip.other",
                                        count = failing
                                    )
                                })),
                        )
                    })
                    .child(
                        IconButton::new(("api-saved-delete", index), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .visible_on_hover("api-row")
                            .tooltip(Tooltip::text(t!("api_client.panel.delete")))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(collection) = this.collection().cloned() {
                                    collection.update(cx, |collection, cx| {
                                        collection.delete_saved_request(&delete_id, cx)
                                    });
                                }
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_saved(&id, window, cx);
                    }))
                    .into_any_element()
            }
        }
    }

    fn render_collection_selector(
        &self,
        collection: &Entity<Collection>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.weak_entity();
        let collection = collection.read(cx);
        let title = collection.title();
        let spec_file = collection
            .spec_path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| collection.file.spec.clone());
        let subtitle = match collection
            .spec
            .as_ref()
            .map(|spec| spec.api_version.clone())
            .filter(|version| !version.is_empty())
        {
            Some(version) => format!("{spec_file} · v{version}"),
            None => spec_file,
        };
        let titles: Vec<(usize, String)> = self
            .collections
            .iter()
            .enumerate()
            .map(|(index, collection)| (index, collection.read(cx).title()))
            .collect();
        let selected = self.selected;
        let hover = cx.theme().colors().element_hover;
        let trigger = h_flex()
            .id("api-collection-selector")
            .w_full()
            .min_w_0()
            .h(px(40.))
            .px_1p5()
            .gap_2()
            .rounded(Radius::Md.rems())
            .hover(move |style| style.bg(hover))
            .child(ui::initial_tile(&title, cx).size(px(22.)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(title)
                            .size(LabelSize::Custom(rems_from_px(12.5_f32)))
                            .weight(FontWeight::SEMIBOLD)
                            .truncate(),
                    )
                    .child(
                        Label::new(subtitle)
                            .size(LabelSize::Custom(rems_from_px(10.5_f32)))
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                Icon::new(IconName::ChevronUpDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            );
        PopoverMenu::new("api-collection-menu")
            .full_width(true)
            .trigger(PopoverTriggerArea::new(trigger))
            .anchor(gpui::Anchor::TopLeft)
            .menu(move |window, cx| {
                let this = this.clone();
                let titles = titles.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header(t!("api_client.panel.collections_header"));
                    for (index, title) in titles.iter() {
                        let this = this.clone();
                        let index = *index;
                        menu = menu.toggleable_entry(
                            title.clone(),
                            index == selected,
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                this.update(cx, |this, cx| {
                                    this.selected = index;
                                    this.view = View::Tree;
                                    cx.notify();
                                })
                                .log_err();
                            },
                        );
                    }
                    let open_spec = this.clone();
                    let reload = this.clone();
                    let link = this;
                    menu.separator()
                        .entry(t!("api_client.panel.open_spec"), None, move |window, cx| {
                            open_spec
                                .update(cx, |this, cx| this.open_spec(window, cx))
                                .log_err();
                        })
                        .entry(t!("api_client.panel.reload_spec"), None, move |_, cx| {
                            reload.update(cx, |this, cx| this.reload_spec(cx)).log_err();
                        })
                        .entry(t!("api_client.panel.link_another"), None, move |_, cx| {
                            link.update(cx, |this, cx| this.show_link(cx)).log_err();
                        })
                }))
            })
    }

    fn reload_spec(&mut self, cx: &mut Context<Self>) {
        if let Some(collection) = self.collection() {
            collection.update(cx, |collection, cx| collection.reload_spec(cx));
        }
    }

    fn render_environment_row(
        &self,
        collection: &Entity<Collection>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let collection_entity = collection.clone();
        let environments: Vec<String> = collection
            .read(cx)
            .environments()
            .iter()
            .map(|environment| environment.name.clone())
            .collect();
        let active = collection
            .read(cx)
            .active_environment()
            .map(|environment| environment.name.clone())
            .unwrap_or_else(|| t!("api_client.panel.no_environment").to_string());
        h_flex()
            .w_full()
            .gap_2()
            .child(
                div().flex_1().min_w_0().child(
                    PopoverMenu::new("api-environment")
                        .full_width(true)
                        .trigger(PopoverTriggerArea::new(ui::select_field(
                            "api-environment-button",
                            active.clone(),
                            cx,
                        )))
                        .anchor(gpui::Anchor::TopLeft)
                        .menu(move |window, cx| {
                            let environments = environments.clone();
                            let active = active.clone();
                            let collection = collection_entity.clone();
                            Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                menu = menu.header(t!("api_client.panel.environment_header"));
                                for name in environments.iter() {
                                    let collection = collection.clone();
                                    let chosen = name.clone();
                                    menu = menu.toggleable_entry(
                                        name.clone(),
                                        *name == active,
                                        IconPosition::End,
                                        None,
                                        move |_, cx| {
                                            collection.update(cx, |collection, cx| {
                                                collection
                                                    .set_active_environment(chosen.clone(), cx)
                                            });
                                        },
                                    );
                                }
                                menu
                            }))
                        }),
                ),
            )
            .children(self.render_login_pill(collection, cx))
    }

    /// The session at a glance; the tooltip carries the long version and a click does the
    /// next step (log in, renew, or configure).
    fn render_login_pill(
        &self,
        collection: &Entity<Collection>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let collection_entity = collection.clone();
        let collection = collection.read(cx);
        let has_bearer = collection.spec.as_ref().is_some_and(|spec| {
            spec.security_schemes
                .iter()
                .any(|scheme| matches!(scheme.kind, SchemeKind::Bearer { .. } | SchemeKind::OAuth2))
        });
        let login = collection.file.auth.login.clone();
        if !has_bearer && login.is_none() {
            return None;
        }
        let logged_in = collection.session.is_logged_in();
        let logging_in = collection.login_status == LoginStatus::LoggingIn;
        let has_credentials = !collection.login_secret_names().is_empty()
            && collection
                .login_secret_names()
                .iter()
                .all(|name| collection.secret(name).is_some());
        let login_via = login.as_ref().map(|login| {
            t!("api_client.panel.login_via", operation = &login.operation).to_string()
        });
        let (chip, tooltip) = match (&collection.login_status, logged_in) {
            (LoginStatus::LoggingIn, _) => (
                ui::status_chip_spinning(
                    IconName::LoadCircle,
                    t!("api_client.panel.logging_in"),
                    Color::Muted,
                    cx,
                ),
                login_via.unwrap_or_default(),
            ),
            (LoginStatus::Failed(error), _) => (
                ui::status_chip(
                    Some(IconName::Warning),
                    t!("api_client.panel.login_failed"),
                    Color::Error,
                    cx,
                ),
                error.to_string(),
            ),
            (LoginStatus::Idle, true) => {
                let seconds_left = collection.session.seconds_left();
                let (label, color) = match seconds_left {
                    Some(seconds) if seconds <= 0 => (
                        t!("api_client.panel.login_pill_expired").to_string(),
                        Color::Warning,
                    ),
                    Some(seconds) if seconds < 3600 => (
                        t!(
                            "api_client.panel.login_pill_minutes",
                            minutes = (seconds / 60).max(1)
                        )
                        .to_string(),
                        Color::Success,
                    ),
                    Some(seconds) => (
                        t!("api_client.panel.login_pill_hours", hours = seconds / 3600).to_string(),
                        Color::Success,
                    ),
                    None => (t!("api_client.panel.logged_in").to_string(), Color::Success),
                };
                let who = collection
                    .login_label()
                    .map(|label| t!("api_client.panel.logged_in_as", label = label).to_string())
                    .unwrap_or_else(|| t!("api_client.panel.logged_in").to_string());
                let expiry = match seconds_left {
                    Some(seconds) if seconds <= 0 => {
                        t!("api_client.panel.token_expired").to_string()
                    }
                    Some(seconds) if seconds < 120 => {
                        t!("api_client.panel.token_expires_seconds", seconds = seconds).to_string()
                    }
                    Some(seconds) if seconds < 7200 => t!(
                        "api_client.panel.token_expires_minutes",
                        minutes = seconds / 60
                    )
                    .to_string(),
                    Some(seconds) => t!(
                        "api_client.panel.token_expires_hours",
                        hours = seconds / 3600
                    )
                    .to_string(),
                    None => t!("api_client.panel.token_no_expiry").to_string(),
                };
                (
                    ui::status_chip(Some(IconName::UserCheck), label, color, cx),
                    format!("{who} · {expiry}"),
                )
            }
            (LoginStatus::Idle, false) => match login_via {
                Some(login_via) if has_credentials => (
                    ui::status_chip(
                        Some(IconName::Person),
                        t!("api_client.panel.log_in"),
                        Color::Default,
                        cx,
                    ),
                    login_via,
                ),
                Some(login_via) => (
                    ui::status_chip(
                        Some(IconName::Person),
                        t!("api_client.panel.configure"),
                        Color::Muted,
                        cx,
                    ),
                    login_via,
                ),
                None => (
                    ui::status_chip(
                        Some(IconName::Person),
                        t!("api_client.panel.configure"),
                        Color::Muted,
                        cx,
                    ),
                    t!("api_client.panel.login_not_found").to_string(),
                ),
            },
        };
        let has_login = login.is_some();
        Some(
            div()
                .id("api-login-pill")
                .flex_none()
                .when(!logging_in, |this| this.cursor_pointer())
                .child(chip)
                .when(!tooltip.is_empty(), |this| {
                    this.tooltip(Tooltip::text(tooltip))
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    if logging_in {
                        return;
                    }
                    if has_login && logged_in {
                        collection_entity.update(cx, |collection, cx| {
                            collection.renew(cx).detach_and_log_err(cx)
                        });
                    } else if has_login && has_credentials {
                        collection_entity.update(cx, |collection, cx| collection.start_login(cx));
                    } else {
                        this.open_settings(window, cx);
                    }
                }))
                .into_any_element(),
        )
    }

    fn render_footer(&self, collection: &Entity<Collection>, cx: &mut Context<Self>) -> Div {
        let collection = collection.read(cx);
        let warnings = collection
            .spec
            .as_ref()
            .map(|spec| spec.warnings.clone())
            .unwrap_or_default();
        let (mut tone, mut text) = match (&collection.spec_error, collection.synced_at) {
            (Some(_), _) => (
                StatusTone::Error,
                t!("api_client.panel.sync_error").to_string(),
            ),
            (None, Some(synced_at)) => (StatusTone::Success, synced_text(synced_at)),
            (None, None) if collection.file.spec.is_empty() => (StatusTone::Success, String::new()),
            (None, None) => (
                StatusTone::Neutral,
                t!("api_client.panel.reading_spec").to_string(),
            ),
        };
        let has_spec = collection.spec.is_some();
        let has_request = self.active_request.is_some();
        if !warnings.is_empty() {
            if tone == StatusTone::Success {
                tone = StatusTone::Warning;
            }
            text.push(' ');
            text.push_str(&if warnings.len() == 1 {
                t!("api_client.panel.warnings.one")
            } else {
                t!("api_client.panel.warnings.other", count = warnings.len())
            });
        }
        let this = cx.weak_entity();
        let more_menu = PopoverMenu::new("api-footer-menu")
            .trigger_with_tooltip(
                IconButton::new("api-footer-menu-button", IconName::Ellipsis)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text(t!("api_client.panel.more_tooltip")),
            )
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let this = this.clone();
                let warnings = warnings.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    let settings = this.clone();
                    let open_spec = this.clone();
                    let reload = this.clone();
                    let remove = this.clone();
                    menu = menu
                        .sectioned()
                        .entry(
                            t!("api_client.panel.settings_tooltip"),
                            None,
                            move |window, cx| {
                                settings
                                    .update(cx, |this, cx| this.open_settings(window, cx))
                                    .log_err();
                            },
                        )
                        .entry(t!("api_client.panel.open_spec"), None, move |window, cx| {
                            open_spec
                                .update(cx, |this, cx| this.open_spec(window, cx))
                                .log_err();
                        })
                        .entry(t!("api_client.panel.reload_spec"), None, move |_, cx| {
                            reload.update(cx, |this, cx| this.reload_spec(cx)).log_err();
                        });
                    if !warnings.is_empty() {
                        menu = menu.separator();
                        for warning in warnings.iter().take(8) {
                            menu = menu.label(warning.clone());
                        }
                    }
                    menu.header(t!("api_client.panel.menu_export_section"))
                        .action(
                            t!("api_client.panel.export_collection_postman"),
                            ExportPostmanCollection.boxed_clone(),
                        )
                        .action(
                            t!("api_client.panel.export_environments_postman"),
                            ExportPostmanEnvironments.boxed_clone(),
                        )
                        .action_disabled_when(
                            !has_request,
                            t!("api_client.panel.export_request_curl"),
                            CopyRequestAsCurl.boxed_clone(),
                        )
                        .header(t!("api_client.panel.menu_collection_section"))
                        .action(
                            t!("api_client.panel.reimport_postman"),
                            ReimportPostman.boxed_clone(),
                        )
                        .action_disabled_when(
                            !has_spec,
                            t!("api_client.panel.match_spec"),
                            MatchSpec.boxed_clone(),
                        )
                        .item(
                            ContextMenuEntry::new(t!("api_client.panel.remove_collection"))
                                .icon(IconName::Trash)
                                .icon_position(IconPosition::Start)
                                .icon_color(Color::Error)
                                .handler(move |window, cx| {
                                    remove
                                        .update(cx, |this, cx| this.remove_collection(window, cx))
                                        .log_err();
                                }),
                        )
                }))
            });
        ui::navigator_footer(
            Some(tone),
            text,
            Some(
                h_flex()
                    .gap_1()
                    .child(
                        ui::capsule_button(
                            "api-import",
                            Some(IconName::Download),
                            t!("api_client.panel.import"),
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.open_import(window, cx))),
                    )
                    .child(more_menu)
                    .into_any_element(),
            ),
            cx,
        )
        .h(px(44.))
    }

    fn render_tree_view(
        &self,
        collection: &Entity<Collection>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.tree_rows(cx);
        let spec_error = collection.read(cx).spec_error.clone();
        let file_error = collection.read(cx).file_error.clone();
        let storage_error = collection.read(cx).storage_error.clone();
        let banner_error = file_error.or(spec_error).or(storage_error);
        let operation_count = collection
            .read(cx)
            .spec
            .as_ref()
            .map_or(0, |spec| spec.operations.len());
        let row_count = rows.len();
        v_flex()
            .size_full()
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .child(self.render_collection_selector(collection, cx)),
            )
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .child(self.render_environment_row(collection, cx)),
            )
            .child(div().px_3().pt_2().child(self.filter_input.clone()))
            .when_some(banner_error, |this, error| {
                this.child(div().px_3().pt_2().child(ui::error_callout(
                    t!("api_client.panel.collection_error"),
                    error,
                    cx,
                )))
            })
            .child(
                div().px_3().pt_3().pb_1().child(ui::section_header(
                    t!("api_client.panel.routes_count", count = operation_count),
                    Some(
                        IconButton::new("api-add-route", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text(t!("api_client.panel.open_spec")))
                            .on_click(cx.listener(|this, _, window, cx| this.open_spec(window, cx)))
                            .into_any_element(),
                    ),
                )),
            )
            .child(
                div().flex_1().min_h_0().px_2().child(
                    uniform_list(
                        "api-tree",
                        row_count,
                        cx.processor(move |this, range: Range<usize>, _window, cx| {
                            let rows = this.tree_rows(cx);
                            range
                                .filter_map(|index| {
                                    rows.get(index)
                                        .cloned()
                                        .map(|row| this.render_row(index, row, cx))
                                })
                                .collect()
                        }),
                    )
                    .size_full(),
                ),
            )
            .child(self.render_footer(collection, cx))
            .into_any_element()
    }

    fn render_start_view(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .id("api-start")
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .gap_2()
            .child(
                ui::choice_card(
                    "api-start-spec",
                    IconName::FileCode,
                    t!("api_client.panel.empty_use_spec_title"),
                    t!("api_client.panel.empty_use_spec_description"),
                    Some(t!("api_client.panel.recommended")),
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.show_link(cx))),
            )
            .child(
                ui::choice_card(
                    "api-start-postman",
                    IconName::Download,
                    t!("api_client.panel.empty_import_postman_title"),
                    t!("api_client.panel.empty_import_postman_description"),
                    None,
                    true,
                    cx,
                )
                .on_click(cx.listener(|this, _, window, cx| this.open_import(window, cx))),
            )
            // Blank collections have no backend yet.
            .child(ui::choice_card(
                "api-start-blank",
                IconName::Plus,
                t!("api_client.panel.empty_from_scratch_title"),
                t!("api_client.panel.empty_from_scratch_description"),
                None,
                false,
                cx,
            ))
            .child(
                div().pt_1().child(
                    Label::new(t!("api_client.panel.curl_hint"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .into_any_element()
    }

    /// Adds a spec from outside the folders the search looks at, chosen in the file picker.
    fn pick_other_spec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("api_client.panel.use_this_spec")),
        });
        let roots = self.roots(cx);
        let fs = self.fs.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Some(path) = paths.await??.and_then(|paths| paths.into_iter().next()) else {
                return anyhow::Ok(());
            };
            let text = fs.load(&path).await?;
            let summary_path = path.clone();
            let summary = cx
                .background_spawn(async move { summarize(&summary_path, &text) })
                .await;
            let root = roots
                .iter()
                .find(|root| path.starts_with(root))
                .cloned()
                .or_else(|| path.parent().map(Path::to_path_buf))
                .unwrap_or_default();
            let relative = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            this.update(cx, |this, cx| {
                this.candidates.retain(|candidate| candidate.path != path);
                this.candidates.insert(
                    0,
                    Candidate {
                        root,
                        path,
                        relative,
                        summary: Some(summary),
                    },
                );
                this.selected_candidate = 0;
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_link_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let linking = self.link_task.is_some();
        let selected = self.selected_candidate;
        let colors = cx.theme().colors();
        let chosen = self
            .candidates
            .get(selected)
            .and_then(|candidate| candidate.summary.as_ref())
            .and_then(|summary| summary.as_ref().ok())
            .cloned();
        let can_link = chosen.is_some();
        let note_row = |icon: IconName, title: SharedString, detail: SharedString| {
            h_flex()
                .w_full()
                .gap_2p5()
                .items_start()
                .child(
                    div()
                        .pt_0p5()
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted)),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(title)
                                .size(LabelSize::Small)
                                .weight(FontWeight::MEDIUM),
                        )
                        .child(
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
        };
        let link_label = match (&chosen, linking) {
            (_, true) => t!("api_client.panel.linking"),
            (Some(chosen), false) => t!("api_client.panel.link_named", title = &chosen.title),
            (None, false) => t!("api_client.panel.link"),
        };
        let link_button =
            ui::primary_button("api-link", Some(IconName::Link), link_label, None, cx)
                .w_full()
                .h(px(34.))
                .justify_center()
                .map(|this| {
                    if can_link && !linking {
                        this.on_click(cx.listener(|this, _, _, cx| this.link(cx)))
                            .into_any_element()
                    } else {
                        this.opacity(0.5).cursor_default().into_any_element()
                    }
                });
        let hover = colors.element_hover;
        v_flex()
            .size_full()
            .child(
                v_flex()
                    .id("api-link-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_3()
                    .pt_3()
                    .pb_3()
                    .gap_2p5()
                    .when(!self.collections.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .id("api-link-back")
                                .gap_1()
                                .cursor_pointer()
                                .child(
                                    Icon::new(IconName::ChevronLeft)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(t!("api_client.panel.routes"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.view = View::Tree;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                Label::new(t!("api_client.panel.link_title"))
                                    .size(LabelSize::Large)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(t!("api_client.panel.link_description"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(section_label(t!("api_client.panel.found_section")))
                    .when(!self.discovered, |this| {
                        this.child(
                            Label::new(t!("api_client.panel.searching_files"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(self.discovered && self.candidates.is_empty(), |this| {
                        this.child(
                            Label::new(t!("api_client.panel.no_files_found"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .children(
                        self.candidates
                            .iter()
                            .enumerate()
                            .map(|(index, candidate)| {
                                let active = index == selected;
                                let detail: AnyElement = match &candidate.summary {
                                    Some(Ok(summary)) => h_flex()
                                        .gap_2()
                                        .child(ui::status_chip(
                                            None,
                                            summary.format.clone(),
                                            Color::Warning,
                                            cx,
                                        ))
                                        .child(
                                            Label::new(summary.info.clone())
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted)
                                                .truncate(),
                                        )
                                        .into_any_element(),
                                    Some(Err(error)) => Label::new(error.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Error)
                                        .truncate()
                                        .into_any_element(),
                                    None => Label::new(t!("api_client.panel.reading"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .into_any_element(),
                                };
                                radio_card(("api-candidate", index), active, true, cx)
                                    .cursor_pointer()
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .gap_1()
                                            .child(
                                                Label::new(candidate.relative.clone())
                                                    .size(LabelSize::Small)
                                                    .buffer_font(cx)
                                                    .weight(FontWeight::SEMIBOLD)
                                                    .truncate(),
                                            )
                                            .child(detail),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.selected_candidate = index;
                                        cx.notify();
                                    }))
                            }),
                    )
                    .child(
                        h_flex()
                            .id("api-link-other")
                            .w_full()
                            .h(px(36.))
                            .px_3()
                            .gap_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(colors.border_selected)
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .child(
                                Icon::new(IconName::FolderSearch)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(t!("api_client.panel.other_spec_file"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pick_other_spec(window, cx)),
                            ),
                    )
                    .when_some(chosen, |this, chosen| {
                        let folders = match chosen.tags.as_slice() {
                            [] => String::new(),
                            tags if tags.len() <= 3 => tags.join(", "),
                            tags => format!("{}…", tags[..3].join(", ")),
                        };
                        let servers = if chosen.servers.is_empty() {
                            t!("api_client.panel.local_no_base_url").to_string()
                        } else {
                            chosen.servers.join(" · ")
                        };
                        this.child(div().h_2())
                            .child(section_label(t!("api_client.panel.plan_section")))
                            .child(plan_row(
                                IconName::Folder,
                                "tags",
                                t!("api_client.panel.plan_folders", count = chosen.tags.len()),
                                plan_detail(folders),
                                cx,
                            ))
                            .child(plan_row(
                                IconName::ArrowRightLeft,
                                "paths",
                                t!(
                                    "api_client.panel.plan_requests",
                                    count = chosen.operation_count
                                ),
                                gpui::Empty.into_any_element(),
                                cx,
                            ))
                            .child(plan_row(
                                IconName::Server,
                                "servers",
                                match chosen.servers.len() {
                                    0 | 1 => t!("api_client.panel.plan_environments.one"),
                                    count => t!(
                                        "api_client.panel.plan_environments.other",
                                        count = count
                                    ),
                                },
                                plan_detail(servers),
                                cx,
                            ))
                            .when(!chosen.schemes.is_empty(), |this| {
                                this.child(plan_row(
                                    IconName::Lock,
                                    "securitySchemes",
                                    t!("api_client.panel.plan_login"),
                                    plan_detail(chosen.schemes.join(", ")),
                                    cx,
                                ))
                            })
                            .when(chosen.schema_count > 0, |this| {
                                this.child(plan_row(
                                    IconName::Check,
                                    "components.schemas",
                                    t!("api_client.panel.plan_validation"),
                                    plan_detail(t!("api_client.panel.plan_validation_detail")),
                                    cx,
                                ))
                            })
                    })
                    .child(div().h_2())
                    .child(note_row(
                        IconName::RotateCw,
                        t!("api_client.panel.note_watch_title"),
                        t!("api_client.panel.note_watch_detail"),
                    ))
                    .child(note_row(
                        IconName::FileCode,
                        t!("api_client.panel.note_versioned_title"),
                        t!("api_client.panel.note_versioned_detail"),
                    ))
                    .child(keychain_note(t!("api_client.panel.keychain_note"), cx))
                    .when_some(self.link_error.clone(), |this, error| {
                        this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    }),
            )
            .child(div().px_3().pb_3().child(link_button))
            .into_any_element()
    }

    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.collection().cloned() else {
            return;
        };
        self.settings = Some(self.build_settings_form(&collection, window, cx));
        self.view = View::Settings;
        cx.notify();
    }

    fn input(
        &self,
        placeholder: &str,
        text: &str,
        masked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputField> {
        // `masked(false)` still shows the reveal toggle, so only secrets call it.
        let input = cx.new(|cx| {
            let input = InputField::new(window, cx, placeholder);
            if masked { input.masked(true) } else { input }
        });
        let editor = input.read(cx).editor().clone();
        editor.set_text(text, window, cx);
        input
    }

    fn build_settings_form(
        &self,
        collection: &Entity<Collection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SettingsForm {
        let snapshot = collection.read(cx);
        let login_config = snapshot.file.auth.login.clone();
        let login = login_config.clone().unwrap_or_default();
        let credential_values: Vec<(String, String)> = snapshot
            .login_secret_names()
            .into_iter()
            .map(|name| {
                let value = snapshot.secret(&name).unwrap_or("").to_string();
                (name, value)
            })
            .collect();
        let mut scheme_values: Vec<(String, SharedString, String, bool, String)> = Vec::new();
        if let Some(spec) = snapshot.spec.as_ref() {
            for scheme in &spec.security_schemes {
                let parts: &[(&'static str, bool)] = match &scheme.kind {
                    SchemeKind::ApiKey { .. } => &[("key", true)],
                    SchemeKind::Basic => &[("username", false), ("password", true)],
                    _ => &[],
                };
                for (part, masked) in parts {
                    let secret = match scheme.kind {
                        SchemeKind::Basic => send::basic_secret(&scheme.name, part),
                        _ => send::api_key_secret(&scheme.name),
                    };
                    let name = secret.trim_start_matches(SECRET_PREFIX).to_string();
                    let value = snapshot.secret(&name).unwrap_or("").to_string();
                    let (placeholder, label) = match &scheme.kind {
                        SchemeKind::ApiKey { name: header, .. } => (
                            t!("api_client.settings.api_key_placeholder"),
                            t!(
                                "api_client.settings.api_key_label",
                                scheme = &scheme.name,
                                header = header
                            )
                            .to_string(),
                        ),
                        _ => (
                            SharedString::from(*part),
                            t!(
                                "api_client.settings.basic_secret_label",
                                part = part,
                                scheme = &scheme.name
                            )
                            .to_string(),
                        ),
                    };
                    scheme_values.push((name, placeholder, value, *masked, label));
                }
            }
        }
        let header_values = snapshot.file.headers.clone();
        let environment = snapshot.active_environment().cloned().unwrap_or_default();
        let auth = snapshot.file.auth.clone();

        let login_body = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 10, window, cx);
            editor.set_text(login.body.clone(), window, cx);
            editor.set_placeholder_text("{ \"email\": \"{{secret.email}}\" }", window, cx);
            editor
        });
        let credentials = credential_values
            .into_iter()
            .map(|(name, value)| {
                let lower = name.to_ascii_lowercase();
                let masked =
                    lower.contains("pass") || lower.contains("senha") || lower.contains("secret");
                let input = cx.new(|cx| {
                    let input =
                        InputField::new(window, cx, &name).start_label(name.clone(), px(80.));
                    if masked { input.masked(true) } else { input }
                });
                input.read(cx).editor().clone().set_text(&value, window, cx);
                (name, input)
            })
            .collect();
        let scheme_secrets = scheme_values
            .into_iter()
            .map(|(name, placeholder, value, masked, label)| {
                let input = self.input(&placeholder, &value, masked, window, cx);
                (name, label, input)
            })
            .collect();
        let name_placeholder = t!("api_client.settings.name_placeholder");
        let value_placeholder = t!("api_client.settings.value_placeholder");
        let headers = header_values
            .iter()
            .map(|header| {
                (
                    self.input(&name_placeholder, &header.name, false, window, cx),
                    self.input(&value_placeholder, &header.value, false, window, cx),
                    header.enabled,
                )
            })
            .collect();
        let environment_variables = environment
            .variables
            .iter()
            .map(|variable| {
                (
                    self.input(&name_placeholder, &variable.name, false, window, cx),
                    self.input(&value_placeholder, &variable.value, false, window, cx),
                )
            })
            .collect();
        SettingsForm {
            login_operation: login_config.map(|login| login.operation),
            refresh_operation: login.refresh_operation.clone(),
            login_body,
            token_path: self.input("$.accessToken", &login.token_path, false, window, cx),
            refresh_token_path: self.input(
                "$.refreshToken",
                login.refresh_token_path.as_deref().unwrap_or(""),
                false,
                window,
                cx,
            ),
            expires_in_path: self.input(
                &t!("api_client.settings.expires_in_placeholder"),
                login.expires_in_path.as_deref().unwrap_or(""),
                false,
                window,
                cx,
            ),
            refresh_body: self.input(
                "{\"refreshToken\":\"{{refreshToken}}\"}",
                login.refresh_body.as_deref().unwrap_or(""),
                false,
                window,
                cx,
            ),
            credentials,
            scheme_secrets,
            headers,
            environment_name: environment.name,
            environment_variables,
            new_environment: self.input(
                &t!("api_client.settings.new_environment_placeholder"),
                "",
                false,
                window,
                cx,
            ),
            renew_before_expiry: auth.renew_before_expiry,
            retry_on_unauthorized: auth.retry_on_unauthorized,
            remember_session: auth.remember_session,
            dirty: false,
        }
    }

    /// Writes the form: settings to the collection file, credentials to the keychain.
    fn save_settings(&mut self, then_log_in: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection) = self.collection().cloned() else {
            return;
        };
        let Some(form) = self.settings.as_ref() else {
            return;
        };
        let text = |input: &Entity<InputField>| input.read(cx).text(cx).trim().to_string();
        let optional =
            |input: &Entity<InputField>| Some(text(input)).filter(|value| !value.is_empty());
        let scheme = collection
            .read(cx)
            .spec
            .as_ref()
            .and_then(|spec| {
                spec.security_schemes
                    .iter()
                    .find(|scheme| {
                        matches!(scheme.kind, SchemeKind::Bearer { .. } | SchemeKind::OAuth2)
                    })
                    .map(|scheme| scheme.name.clone())
            })
            .unwrap_or_default();
        let login = form.login_operation.clone().map(|operation| LoginConfig {
            operation,
            scheme,
            body: form.login_body.read(cx).text(cx),
            token_path: optional(&form.token_path).unwrap_or_else(|| "$.accessToken".to_string()),
            refresh_token_path: optional(&form.refresh_token_path),
            expires_in_path: optional(&form.expires_in_path),
            refresh_operation: form.refresh_operation.clone(),
            refresh_body: optional(&form.refresh_body),
        });
        let headers: Vec<HeaderEntry> = form
            .headers
            .iter()
            .map(|(name, value, enabled)| HeaderEntry {
                name: text(name),
                value: value.read(cx).text(cx),
                enabled: *enabled,
            })
            .filter(|header| !header.name.is_empty())
            .collect();
        let variables: Vec<Variable> = form
            .environment_variables
            .iter()
            .map(|(name, value)| Variable {
                name: text(name),
                value: value.read(cx).text(cx).trim().to_string(),
                secret: false,
            })
            .filter(|variable| !variable.name.is_empty())
            .collect();
        let environment_name = form.environment_name.clone();
        let secrets: Vec<(String, String)> = form
            .credentials
            .iter()
            .map(|(name, input)| (name.clone(), input.read(cx).text(cx)))
            .chain(
                form.scheme_secrets
                    .iter()
                    .map(|(name, _, input)| (name.clone(), input.read(cx).text(cx))),
            )
            .collect();
        let (renew, retry, remember) = (
            form.renew_before_expiry,
            form.retry_on_unauthorized,
            form.remember_session,
        );
        collection.update(cx, |collection, cx| {
            for (name, value) in secrets {
                if collection.secret(&name).unwrap_or("") != value {
                    collection.set_secret(&name, value, cx);
                }
            }
            // A secret variable typed in here goes to the keychain, never the shared file.
            let secret_variables: Vec<String> = collection
                .environments()
                .iter()
                .find(|environment| environment.name == environment_name)
                .map(|environment| {
                    environment
                        .variables
                        .iter()
                        .filter(|variable| variable.secret)
                        .map(|variable| variable.name.clone())
                        .collect()
                })
                .unwrap_or_default();
            let mut variables = variables;
            for variable in &mut variables {
                if secret_variables.contains(&variable.name) {
                    variable.secret = true;
                    let value = std::mem::take(&mut variable.value);
                    if !value.is_empty() {
                        collection.set_secret(&variable.name, value, cx);
                    }
                }
            }
            collection.update_file(
                |file| {
                    file.auth.login = login;
                    file.auth.renew_before_expiry = renew;
                    file.auth.retry_on_unauthorized = retry;
                    file.auth.remember_session = remember;
                    file.headers = headers;
                    match file
                        .environments
                        .iter_mut()
                        .find(|environment| environment.name == environment_name)
                    {
                        Some(environment) => environment.variables = variables,
                        None if !environment_name.is_empty() => {
                            file.environments.push(Environment {
                                name: environment_name,
                                variables,
                            })
                        }
                        None => {}
                    }
                },
                cx,
            );
            if then_log_in {
                collection.start_login(cx);
            }
        });
        // New placeholders in the login body need their credential fields.
        self.settings = Some(self.build_settings_form(&collection, window, cx));
        cx.notify();
    }

    fn render_settings_view(
        &self,
        collection: &Entity<Collection>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.settings.as_ref() else {
            return div().into_any_element();
        };
        let collection_read = collection.read(cx);
        let spec = collection_read.spec.clone();
        let login_status = collection_read.login_status.clone();
        let logged_in = collection_read.session.is_logged_in();
        let login_scheme = collection_read
            .file
            .auth
            .login
            .as_ref()
            .map(|login| login.scheme.clone());
        let environments: Vec<String> = collection_read
            .environments()
            .iter()
            .map(|environment| environment.name.clone())
            .collect();
        let colors = cx.theme().colors().clone();
        let accent = colors.border_focused;
        let field_label =
            |text: String| Label::new(text).size(LabelSize::XSmall).color(Color::Muted);
        let field_box = || {
            h_flex()
                .w_full()
                .h(px(32.))
                .px_2p5()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(colors.border_selected)
                .bg(colors.editor_background)
        };

        let post_operations: Vec<(String, String, String, String)> = spec
            .as_ref()
            .map(|spec| {
                spec.operations
                    .iter()
                    .filter(|operation| operation.method == "POST")
                    .map(|operation| {
                        let place = match operation.tags.first() {
                            Some(tag) => format!("{tag} › {}", operation.title()),
                            None => operation.title(),
                        };
                        (
                            operation.key.clone(),
                            operation.method.clone(),
                            operation.path.clone(),
                            place,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let operation_picker = |id: &'static str,
                                current: Option<String>,
                                allow_none: bool,
                                set: fn(&mut SettingsForm, Option<String>),
                                cx: &mut Context<Self>| {
            let this = cx.weak_entity();
            let operations = post_operations.clone();
            let chosen = current
                .as_ref()
                .and_then(|current| operations.iter().find(|(key, ..)| key == current).cloned());
            let hover = colors.element_hover;
            let trigger = field_box()
                .id(SharedString::from(format!("{id}-button")))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .map(|this| match &chosen {
                    Some((_, method, path, place)) => this
                        .child(method_label(method, cx))
                        .child(
                            Label::new(path.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .truncate(),
                        )
                        .child(div().flex_1())
                        .child(
                            div().min_w_0().child(
                                Label::new(place.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        ),
                    None => this
                        .child(
                            Label::new(
                                current
                                    .clone()
                                    .unwrap_or_else(|| t!("api_client.settings.none").to_string()),
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .child(div().flex_1()),
                })
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                );
            PopoverMenu::new(id)
                .full_width(true)
                .trigger(PopoverTriggerArea::new(trigger))
                .menu(move |window, cx| {
                    let this = this.clone();
                    let operations = operations.clone();
                    let current = current.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        if allow_none {
                            let this = this.clone();
                            menu = menu.toggleable_entry(
                                t!("api_client.settings.none"),
                                current.is_none(),
                                IconPosition::End,
                                None,
                                move |_, cx| {
                                    this.update(cx, |this, cx| {
                                        if let Some(form) = this.settings.as_mut() {
                                            set(form, None);
                                            form.dirty = true;
                                        }
                                        cx.notify();
                                    })
                                    .log_err();
                                },
                            );
                        }
                        for (key, _, _, place) in operations.iter().take(120) {
                            let this = this.clone();
                            let chosen = key.clone();
                            menu = menu.toggleable_entry(
                                format!("{key} · {place}"),
                                current.as_deref() == Some(key.as_str()),
                                IconPosition::End,
                                None,
                                move |_, cx| {
                                    this.update(cx, |this, cx| {
                                        if let Some(form) = this.settings.as_mut() {
                                            set(form, Some(chosen.clone()));
                                            form.dirty = true;
                                        }
                                        cx.notify();
                                    })
                                    .log_err();
                                },
                            );
                        }
                        menu
                    }))
                })
        };

        let schemes = spec
            .as_ref()
            .map(|spec| {
                spec.security_schemes
                    .iter()
                    .map(|scheme| {
                        let uses = spec
                            .operations
                            .iter()
                            .filter(|operation| {
                                spec.schemes_for(operation)
                                    .iter()
                                    .any(|used| used.name == scheme.name)
                            })
                            .count();
                        let is_login = match &login_scheme {
                            Some(login_scheme) => *login_scheme == scheme.name,
                            None => false,
                        };
                        (scheme.name.clone(), scheme.summary(), uses, is_login)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let has_bearer_login = form.login_operation.is_some()
            && spec.as_ref().is_some_and(|spec| {
                spec.security_schemes.iter().any(|scheme| {
                    matches!(scheme.kind, SchemeKind::Bearer { .. } | SchemeKind::OAuth2)
                })
            });

        let toggles = [
            (
                "api-renew-before",
                t!("api_client.settings.renew_before_expiry"),
                form.renew_before_expiry,
                (|form: &mut SettingsForm| form.renew_before_expiry = !form.renew_before_expiry)
                    as fn(&mut SettingsForm),
            ),
            (
                "api-retry-401",
                t!("api_client.settings.retry_on_unauthorized"),
                form.retry_on_unauthorized,
                |form: &mut SettingsForm| form.retry_on_unauthorized = !form.retry_on_unauthorized,
            ),
            (
                "api-remember",
                t!("api_client.settings.remember_session"),
                form.remember_session,
                |form: &mut SettingsForm| form.remember_session = !form.remember_session,
            ),
        ];
        let logging_in = login_status == LoginStatus::LoggingIn;
        let can_test = form.login_operation.is_some() && !logging_in;
        let test_button = ui::primary_button(
            "api-test-login",
            Some(IconName::PlayFilled),
            if logging_in {
                t!("api_client.panel.logging_in")
            } else {
                t!("api_client.settings.test_login")
            },
            None,
            cx,
        )
        .flex_1()
        .h(px(32.))
        .justify_center()
        .tooltip(Tooltip::text(t!("api_client.settings.test_login_tooltip")))
        .map(|this| {
            if can_test {
                this.on_click(
                    cx.listener(|this, _, window, cx| this.save_settings(true, window, cx)),
                )
                .into_any_element()
            } else {
                this.opacity(0.5).cursor_default().into_any_element()
            }
        });
        let logout_button = ui::secondary_button(
            "api-settings-logout",
            Some((IconName::Exit, Color::Muted)),
            t!("api_client.settings.logout"),
            logged_in,
            cx,
        )
        .flex_1()
        .h(px(32.))
        .justify_center()
        .when(logged_in, |this| {
            let collection = collection.clone();
            this.on_click(move |_, _, cx| {
                collection.update(cx, |collection, cx| collection.logout(cx));
            })
        });
        let add_link = |id: &'static str, label: SharedString| {
            h_flex()
                .id(id)
                .gap_1p5()
                .cursor_pointer()
                .child(
                    Icon::new(IconName::Plus)
                        .size(IconSize::XSmall)
                        .color(Color::Accent),
                )
                .child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .color(Color::Accent),
                )
        };

        let content = v_flex()
            .gap_2p5()
            .px_3()
            .pt_3()
            .pb_4()
            .child(
                h_flex()
                    .child(
                        h_flex()
                            .id("api-settings-back")
                            .gap_1()
                            .cursor_pointer()
                            .child(
                                Icon::new(IconName::ChevronLeft)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(t!("api_client.panel.routes"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = View::Tree;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(collection_read.title())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        Label::new(t!("api_client.settings.title"))
                            .size(LabelSize::Large)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(t!("api_client.settings.subtitle"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(ui::section_header(
                t!("api_client.settings.schemes_section"),
                Some(
                    Label::new("components.securitySchemes")
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted)
                        .into_any_element(),
                ),
            ))
            .when(schemes.is_empty(), |this| {
                this.child(field_label(
                    t!("api_client.settings.no_schemes").to_string(),
                ))
            })
            .children(schemes.into_iter().map(|(name, summary, uses, is_login)| {
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2p5()
                    .rounded_lg()
                    .border_1()
                    .map(|this| {
                        if is_login {
                            this.bg(accent.opacity(0.18)).border_color(accent)
                        } else {
                            this.bg(colors.element_background)
                                .border_color(colors.border_selected)
                        }
                    })
                    .child(ui::radio_indicator(is_login, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(name)
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(field_label(summary)),
                    )
                    .child(field_label(
                        t!("api_client.settings.scheme_routes", count = uses).to_string(),
                    ))
            }))
            .children(form.scheme_secrets.iter().map(|(_, label, input)| {
                v_flex()
                    .gap_1()
                    .child(field_label(label.clone()))
                    .child(input.clone())
            }))
            .child(div().h_2())
            .child(section_label(t!("api_client.settings.login_section")))
            .child(field_label(
                t!("api_client.settings.login_request").to_string(),
            ))
            .child(operation_picker(
                "api-login-operation",
                form.login_operation.clone(),
                true,
                |form, operation| form.login_operation = operation,
                cx,
            ))
            .child(field_label(
                t!("api_client.settings.login_body").to_string(),
            ))
            .child(
                div()
                    .p_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border_selected)
                    .bg(colors.editor_background)
                    .child(form.login_body.clone()),
            )
            .when(!form.credentials.is_empty(), |this| {
                this.child(field_label(
                    t!("api_client.settings.credentials").to_string(),
                ))
                .child(
                    v_flex()
                        .gap_1p5()
                        .children(form.credentials.iter().map(|(_, input)| input.clone())),
                )
            })
            .child(field_label(
                t!("api_client.settings.extract_from_response").to_string(),
            ))
            .child(
                v_flex().gap_1p5().children(
                    [
                        (&form.token_path, SharedString::from("{{token}}")),
                        (
                            &form.refresh_token_path,
                            SharedString::from("{{refreshToken}}"),
                        ),
                        (
                            &form.expires_in_path,
                            t!("api_client.settings.expires_seconds"),
                        ),
                    ]
                    .into_iter()
                    .map(|(input, target)| {
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(input.clone()))
                            .child(
                                Icon::new(IconName::ArrowRight)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(variable_chip(target, cx))
                    }),
                ),
            )
            .when(
                !crate::config::login_paths_are_valid(&LoginConfig {
                    token_path: form.token_path.read(cx).text(cx),
                    refresh_token_path: Some(form.refresh_token_path.read(cx).text(cx))
                        .filter(|path| !path.trim().is_empty()),
                    expires_in_path: Some(form.expires_in_path.read(cx).text(cx))
                        .filter(|path| !path.trim().is_empty()),
                    ..LoginConfig::default()
                }),
                |this| {
                    this.child(
                        Label::new(t!("api_client.settings.unsupported_path"))
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    )
                },
            )
            .child(field_label(
                t!("api_client.settings.refresh_request").to_string(),
            ))
            .child(operation_picker(
                "api-refresh-operation",
                form.refresh_operation.clone(),
                true,
                |form, operation| form.refresh_operation = operation,
                cx,
            ))
            .when(form.refresh_operation.is_some(), |this| {
                this.child(form.refresh_body.clone())
            })
            .child(v_flex().gap_2().py_1().children(toggles.into_iter().map(
                |(id, label, state, flip)| {
                    let this = cx.weak_entity();
                    SwitchField::new(
                        id,
                        Some(label),
                        None,
                        if state {
                            ToggleState::Selected
                        } else {
                            ToggleState::Unselected
                        },
                        move |_, _, cx| {
                            this.update(cx, |this, cx| {
                                if let Some(form) = this.settings.as_mut() {
                                    flip(form);
                                    form.dirty = true;
                                }
                                cx.notify();
                            })
                            .log_err();
                        },
                    )
                },
            )))
            .when_some(
                match &login_status {
                    LoginStatus::Failed(error) => Some(error.clone()),
                    _ => None,
                },
                |this, error| {
                    this.child(
                        Label::new(error)
                            .size(LabelSize::XSmall)
                            .color(Color::Error),
                    )
                },
            )
            .child(h_flex().gap_2().child(test_button).child(logout_button))
            .child(div().h_2())
            .child(section_label(t!("api_client.settings.headers_section")))
            .when(has_bearer_login, |this| {
                this.child(
                    field_box()
                        .id("api-authorization-header")
                        .child(
                            div().flex_none().w(px(130.)).child(
                                Label::new("Authorization")
                                    .size(LabelSize::Small)
                                    .buffer_font(cx),
                            ),
                        )
                        .child(
                            Label::new("Bearer {{token}}")
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(Color::Accent),
                        )
                        .child(div().flex_1())
                        .child(
                            Icon::new(IconName::Lock)
                                .size(IconSize::XSmall)
                                .color(Color::Success),
                        )
                        .tooltip(Tooltip::text(t!(
                            "api_client.settings.authorization_tooltip"
                        ))),
                )
            })
            .children(
                form.headers
                    .iter()
                    .enumerate()
                    .map(|(index, (name, value, enabled))| {
                        h_flex()
                            .gap_1p5()
                            .child(
                                ui::Checkbox::new(
                                    ("api-collection-header", index),
                                    (*enabled).into(),
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if let Some(form) = this.settings.as_mut()
                                            && let Some(header) = form.headers.get_mut(index)
                                        {
                                            header.2 = !header.2;
                                            form.dirty = true;
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(div().w(px(130.)).child(name.clone()))
                            .child(div().flex_1().child(value.clone()))
                            .child(
                                IconButton::new(
                                    ("api-collection-header-remove", index),
                                    IconName::Trash,
                                )
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if let Some(form) = this.settings.as_mut()
                                            && index < form.headers.len()
                                        {
                                            form.headers.remove(index);
                                            form.dirty = true;
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                    }),
            )
            .child(
                add_link(
                    "api-collection-header-add",
                    t!("api_client.settings.add_header"),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    let name = this.input(
                        &t!("api_client.settings.name_placeholder"),
                        "",
                        false,
                        window,
                        cx,
                    );
                    let value = this.input(
                        &t!("api_client.settings.value_placeholder"),
                        "",
                        false,
                        window,
                        cx,
                    );
                    if let Some(form) = this.settings.as_mut() {
                        form.headers.push((name, value, true));
                        form.dirty = true;
                    }
                    cx.notify();
                })),
            )
            .child(field_label(
                t!("api_client.settings.authorization_note").to_string(),
            ))
            .child(div().h_2())
            .child(section_label(t!(
                "api_client.settings.environments_section"
            )))
            .child(
                ui::segmented_control(cx).children(environments.iter().enumerate().map(
                    |(index, name)| {
                        let active = *name == form.environment_name;
                        let chosen = name.clone();
                        ui::segment_option(
                            ("api-environment-tab", index),
                            name.clone(),
                            active,
                            true,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                // Switching edits another environment; save what was typed first.
                                this.save_settings(false, window, cx);
                                if let Some(collection) = this.collection().cloned() {
                                    collection.update(cx, |collection, cx| {
                                        collection.set_active_environment(chosen.clone(), cx)
                                    });
                                    this.settings =
                                        Some(this.build_settings_form(&collection, window, cx));
                                }
                                cx.notify();
                            },
                        ))
                    },
                )),
            )
            .child(field_label(
                t!(
                    "api_client.settings.environment_variables",
                    environment = &form.environment_name
                )
                .to_string(),
            ))
            .children(form.environment_variables.iter().enumerate().map(
                |(index, (name, value))| {
                    h_flex()
                        .gap_1p5()
                        .child(div().w(px(110.)).child(name.clone()))
                        .child(div().flex_1().child(value.clone()))
                        .child(
                            IconButton::new(("api-variable-remove", index), IconName::Trash)
                                .icon_size(IconSize::XSmall)
                                .icon_color(Color::Muted)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(form) = this.settings.as_mut()
                                        && index < form.environment_variables.len()
                                    {
                                        form.environment_variables.remove(index);
                                        form.dirty = true;
                                    }
                                    cx.notify();
                                })),
                        )
                },
            ))
            .child(
                add_link("api-variable-add", t!("api_client.settings.add_variable")).on_click(
                    cx.listener(|this, _, window, cx| {
                        let name = this.input(
                            &t!("api_client.settings.name_placeholder"),
                            "",
                            false,
                            window,
                            cx,
                        );
                        let value = this.input(
                            &t!("api_client.settings.value_placeholder"),
                            "",
                            false,
                            window,
                            cx,
                        );
                        if let Some(form) = this.settings.as_mut() {
                            form.environment_variables.push((name, value));
                            form.dirty = true;
                        }
                        cx.notify();
                    }),
                ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(div().flex_1().child(form.new_environment.clone()))
                    .child(
                        ui::secondary_button(
                            "api-environment-add",
                            None,
                            t!("api_client.settings.create_environment"),
                            true,
                            cx,
                        )
                        .h(px(32.))
                        .on_click(cx.listener(|this, _, window, cx| {
                            let Some(form) = this.settings.as_ref() else {
                                return;
                            };
                            let name = form.new_environment.read(cx).text(cx).trim().to_string();
                            if name.is_empty() {
                                return;
                            }
                            this.save_settings(false, window, cx);
                            let Some(collection) = this.collection().cloned() else {
                                return;
                            };
                            collection.update(cx, |collection, cx| {
                                let base_url = collection
                                    .active_environment()
                                    .and_then(|environment| environment.get("baseUrl"))
                                    .unwrap_or("")
                                    .to_string();
                                let new_name = name.clone();
                                collection.update_file(
                                    |file| {
                                        if !file
                                            .environments
                                            .iter()
                                            .any(|environment| environment.name == new_name)
                                        {
                                            file.environments.push(Environment {
                                                name: new_name,
                                                variables: vec![Variable {
                                                    name: "baseUrl".to_string(),
                                                    value: base_url,
                                                    secret: false,
                                                }],
                                            });
                                        }
                                    },
                                    cx,
                                );
                                collection.set_active_environment(name, cx);
                            });
                            this.settings = Some(this.build_settings_form(&collection, window, cx));
                            cx.notify();
                        })),
                    ),
            );

        v_flex()
            .size_full()
            .child(
                div()
                    .id("api-settings-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(content),
            )
            .child(
                h_flex()
                    .h(px(44.))
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        Label::new(if form.dirty {
                            t!("api_client.settings.unsaved_changes")
                        } else {
                            SharedString::default()
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                    )
                    .child(div().flex_1())
                    .child(
                        ui::primary_button(
                            "api-settings-save",
                            None,
                            t!("api_client.settings.save"),
                            None,
                            cx,
                        )
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.save_settings(false, window, cx)
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }
}

pub(crate) fn open_import_modal(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    crate::open(workspace, window, cx);
    let Some(panel) = workspace.panel::<ApiPanel>(cx) else {
        return;
    };
    let modal = panel.update(cx, |panel, cx| panel.import_modal(cx));
    workspace.toggle_modal(window, cx, |_, _| modal);
}

pub(crate) fn copy_request_as_curl(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let request = workspace
        .active_item_as::<request_view::ApiRequestView>(cx)
        .map(|view| {
            let view = view.read(cx);
            (view.operation_key.clone(), view.saved_id.clone())
        });
    let collection = workspace
        .panel::<ApiPanel>(cx)
        .and_then(|panel| panel.read(cx).collection().cloned());
    let curl = match (request, collection) {
        (Some((operation, saved_id)), Some(collection)) => {
            let collection = collection.read(cx);
            let draft = saved_id
                .and_then(|id| {
                    collection
                        .file
                        .saved
                        .iter()
                        .find(|saved| saved.id == id)
                        .map(|saved| saved.draft.clone())
                })
                .unwrap_or_else(|| collection.draft(&operation));
            Some(
                collection
                    .prepare(&operation, &draft)
                    .map(|prepared| prepared.masked().curl()),
            )
        }
        _ => None,
    };
    let message = match curl {
        Some(Ok(curl)) => {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(curl));
            return;
        }
        Some(Err(error)) => format!("{error:#}"),
        None => t!("api_client.panel.export_no_request").to_string(),
    };
    workspace.show_toast(
        Toast::new(NotificationId::unique::<ApiPanel>(), message).autohide(),
        cx,
    );
}

struct PickedFile {
    path: PathBuf,
    name: String,
    /// `None` while reading; an error carries its title and message.
    state: Option<Result<PostmanFile, (SharedString, SharedString)>>,
}

pub struct PostmanImportModal {
    panel: WeakEntity<ApiPanel>,
    workspace: WeakEntity<Workspace>,
    /// The selected collection; without one, a new collection file is created.
    target: Option<Entity<Collection>>,
    root: Option<PathBuf>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    files: Vec<PickedFile>,
    match_spec: bool,
    importing: Option<Task<()>>,
    error: Option<SharedString>,
}

impl EventEmitter<DismissEvent> for PostmanImportModal {}

impl ModalView for PostmanImportModal {}

impl Focusable for PostmanImportModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PostmanImportModal {
    fn import(&self) -> PostmanImport {
        let mut import = PostmanImport::default();
        for file in &self.files {
            if let Some(Ok(parsed)) = &file.state {
                import.add(parsed.clone());
            }
        }
        import
    }

    fn source_name(import: &PostmanImport) -> String {
        import
            .collections
            .first()
            .map(|collection| collection.name.clone())
            .or_else(|| {
                import
                    .environments
                    .first()
                    .map(|environment| environment.name.clone())
            })
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "postman".to_string())
    }

    fn target_spec(&self, cx: &App) -> Option<(Arc<spec::Spec>, String)> {
        let collection = self.target.as_ref()?.read(cx);
        let spec = collection.spec.clone()?;
        let file_name = collection
            .spec_path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| collection.file.spec.clone());
        Some((spec, file_name))
    }

    fn destination(&self, import: &PostmanImport, cx: &App) -> String {
        match &self.target {
            Some(collection) => {
                let collection = collection.read(cx);
                collection
                    .file_path
                    .strip_prefix(&collection.root)
                    .unwrap_or(&collection.file_path)
                    .display()
                    .to_string()
            }
            None => format!(
                "{}/{}.json",
                config::COLLECTIONS_DIR,
                config::slug(&Self::source_name(import))
            ),
        }
    }

    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(t!("api_client.panel.import_pick_files")),
        });
        let fs = self.fs.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Some(paths) = paths.await?? else {
                return anyhow::Ok(());
            };
            for path in paths {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                this.update(cx, |this, cx| {
                    this.files.retain(|file| file.path != path);
                    this.files.push(PickedFile {
                        path: path.clone(),
                        name: name.clone(),
                        state: None,
                    });
                    cx.notify();
                })?;
                let state = match fs.load(&path).await {
                    Ok(text) => cx
                        .background_spawn(async move { config::parse_postman(&text) })
                        .await
                        .map_err(|error| match error {
                            PostmanError::Json {
                                line,
                                column,
                                message,
                            } => (
                                t!("api_client.panel.import_invalid_json_title", file = &name),
                                t!(
                                    "api_client.panel.import_invalid_json_line",
                                    line = line,
                                    column = column,
                                    error = message
                                ),
                            ),
                            PostmanError::NotPostman => (
                                SharedString::from(name.clone()),
                                t!("api_client.panel.import_not_postman"),
                            ),
                        }),
                    Err(error) => Err((
                        SharedString::from(name.clone()),
                        t!(
                            "api_client.collection.error_postman_parse",
                            error = format!("{error:#}")
                        ),
                    )),
                };
                this.update(cx, |this, cx| {
                    if let Some(file) = this.files.iter_mut().find(|file| file.path == path) {
                        file.state = Some(state);
                    }
                    cx.notify();
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.importing.is_some() {
            return;
        }
        let import = self.import();
        if import.is_empty() {
            return;
        }
        let name = Self::source_name(&import);
        let done = t!(
            "api_client.panel.import_done",
            count = import.counts(None).requests,
            name = &name
        )
        .to_string();
        let match_spec = self.match_spec && self.target_spec(cx).is_some();
        if let Some(collection) = self.target.clone() {
            collection.update(cx, |collection, cx| {
                collection.apply_postman_import(import, match_spec, cx)
            });
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(NotificationId::unique::<ApiPanel>(), done).autohide(),
                        cx,
                    );
                })
                .log_err();
            cx.emit(DismissEvent);
            return;
        }
        let Some(root) = self.root.clone() else {
            self.error = Some(t!("api_client.panel.import_no_folder"));
            cx.notify();
            return;
        };
        let fs = self.fs.clone();
        let panel = self.panel.clone();
        let workspace = self.workspace.clone();
        self.error = None;
        self.importing = Some(cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let file = collection::create_for_import(fs, root.clone(), &name).await?;
                let collection = panel
                    .update(cx, |panel, cx| panel.adopt_collection(root, file, cx))?
                    .context("the new collection file was not listed")?;
                collection.update(cx, |collection, cx| {
                    collection.apply_postman_import(import, false, cx)
                });
                anyhow::Ok(())
            }
            .await;
            let succeeded = result.is_ok();
            this.update(cx, |this, cx| {
                this.importing = None;
                match result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
            if succeeded {
                ApiPanel::show_toast(&workspace, done, cx);
            }
        }));
        cx.notify();
    }

    fn render_file(&self, index: usize, file: &PickedFile, cx: &mut Context<Self>) -> AnyElement {
        let chip = match &file.state {
            None => ui::status_chip_spinning(
                IconName::LoadCircle,
                t!("api_client.panel.reading"),
                Color::Muted,
                cx,
            ),
            Some(Ok(PostmanFile::Collection(collection))) => ui::status_chip(
                None,
                t!(
                    "api_client.panel.import_kind_collection",
                    version = &collection.version
                ),
                Color::Accent,
                cx,
            ),
            Some(Ok(PostmanFile::Environment(_))) => ui::status_chip(
                None,
                t!("api_client.panel.import_kind_environment"),
                Color::Muted,
                cx,
            ),
            Some(Err(_)) => ui::status_chip(
                Some(IconName::Warning),
                t!("api_client.panel.import_invalid_tag"),
                Color::Error,
                cx,
            ),
        };
        v_flex()
            .w_full()
            .gap_1p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Icon::new(IconName::FileCode)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(file.name.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .truncate(),
                        ),
                    )
                    .child(chip)
                    .child(
                        IconButton::new(("api-import-remove-file", index), IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if index < this.files.len() {
                                    this.files.remove(index);
                                }
                                cx.notify();
                            })),
                    ),
            )
            .when_some(
                match &file.state {
                    Some(Err(error)) => Some(error.clone()),
                    _ => None,
                },
                |this, (title, message)| this.child(ui::error_callout(title, message, cx)),
            )
            .into_any_element()
    }

    fn render_plan(&self, import: &PostmanImport, cx: &App) -> Vec<AnyElement> {
        let spec = self
            .target_spec(cx)
            .filter(|_| self.match_spec)
            .map(|(spec, _)| spec);
        let counts = import.counts(spec.as_deref());
        let empty = || gpui::Empty.into_any_element();
        let mut rows = Vec::new();
        if counts.requests > 0 || counts.folders > 0 {
            rows.push(plan_row(
                IconName::Folder,
                "item[]",
                t!(
                    "api_client.panel.import_row_requests",
                    folders = counts.folders,
                    requests = counts.requests
                ),
                empty(),
                cx,
            ));
        }
        if counts.environments > 0 || counts.variables > 0 {
            rows.push(plan_row(
                IconName::Server,
                "values[]",
                t!(
                    "api_client.panel.import_row_environments",
                    environments = counts.environments,
                    variables = counts.variables
                ),
                empty(),
                cx,
            ));
        }
        if counts.secrets > 0 {
            rows.push(plan_row(
                IconName::Lock,
                "type: secret",
                t!(
                    "api_client.panel.import_row_secrets",
                    count = counts.secrets
                ),
                empty(),
                cx,
            ));
        }
        if counts.auth {
            rows.push(plan_row(
                IconName::UserCheck,
                "auth",
                t!("api_client.panel.import_row_auth"),
                empty(),
                cx,
            ));
        }
        if counts.captures > 0 {
            rows.push(plan_row(
                IconName::ArrowRightLeft,
                "pm.environment.set",
                t!(
                    "api_client.panel.import_row_captures",
                    count = counts.captures
                ),
                empty(),
                cx,
            ));
        }
        if counts.tests > 0 {
            rows.push(plan_row(
                IconName::Check,
                "pm.test",
                t!("api_client.panel.import_row_tests", count = counts.tests),
                empty(),
                cx,
            ));
        }
        if counts.unsupported > 0 {
            rows.push(plan_row(
                IconName::Warning,
                "pm.sendRequest",
                t!(
                    "api_client.panel.import_row_unsupported",
                    count = counts.unsupported
                ),
                ui::status_pill(
                    t!("api_client.panel.import_tag_disabled"),
                    StatusTone::Warning,
                    cx,
                )
                .into_any_element(),
                cx,
            ));
        }
        rows.into_iter()
            .map(IntoElement::into_any_element)
            .collect()
    }
}

impl Render for PostmanImportModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let import = self.import();
        let counts = import.counts(None);
        let reading = self.files.iter().any(|file| file.state.is_none());
        let can_import = !import.is_empty() && !reading && self.importing.is_none();
        let target_spec = self.target_spec(cx);
        let destination = t!(
            "api_client.panel.import_destination",
            path = self.destination(&import, cx)
        );
        let confirm_label = if counts.requests == 1 {
            t!("api_client.panel.import_confirm.one")
        } else {
            t!(
                "api_client.panel.import_confirm.other",
                count = counts.requests
            )
        };
        let focus_handle = self.focus_handle.clone();
        let files: Vec<AnyElement> = self
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| self.render_file(index, file, cx))
            .collect();
        let plan = self.render_plan(&import, cx);

        let source_files = radio_card("api-import-from-files", true, true, cx).child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(
                            Label::new(t!("api_client.panel.import_from_files"))
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            Label::new(t!("api_client.panel.import_from_files_description"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .children(files)
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            ui::secondary_button(
                                "api-import-pick",
                                Some((IconName::FolderSearch, Color::Muted)),
                                t!("api_client.panel.import_pick_files"),
                                true,
                                cx,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pick_files(window, cx)),
                            ),
                        )
                        .when(self.files.is_empty(), |this| {
                            this.child(
                                Label::new(t!("api_client.panel.import_no_files"))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                        }),
                ),
        );
        let source_workspace = radio_card("api-import-from-workspace", false, false, cx).child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    Label::new(t!("api_client.panel.import_from_workspace"))
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD),
                )
                .child(
                    Label::new(t!("api_client.panel.import_from_workspace_note"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        );

        let match_row = target_spec.map(|(spec, spec_name)| {
            let (matched, unmatched) = if self.match_spec {
                let matches = import.matches(&spec);
                let matched = matches.iter().filter(|found| found.is_some()).count();
                (matched, matches.len() - matched)
            } else {
                (0, counts.requests)
            };
            h_flex()
                .w_full()
                .gap_2p5()
                .child(
                    ui::toggle_switch("api-import-match", self.match_spec, true, cx).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.match_spec = !this.match_spec;
                            cx.notify();
                        }),
                    ),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(t!("api_client.panel.import_match_spec", spec = spec_name))
                                .size(LabelSize::Small)
                                .weight(FontWeight::MEDIUM),
                        )
                        .child(
                            Label::new(t!(
                                "api_client.panel.import_match_spec_detail",
                                matched = matched,
                                unmatched = unmatched
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        ),
                )
        });

        let confirm = ui::primary_button(
            "api-import-confirm",
            Some(IconName::Download),
            confirm_label,
            Some(KeyBinding::for_action_in(
                &menu::SecondaryConfirm,
                &focus_handle,
                cx,
            )),
            cx,
        )
        .map(|this| {
            if can_import {
                this.on_click(cx.listener(|this, _, window, cx| this.confirm(window, cx)))
            } else {
                this.opacity(0.5).cursor_default()
            }
        });

        ui::modal_surface(cx)
            .key_context("ApiImportModal")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| this.confirm(window, cx)))
            .on_action(
                cx.listener(|this, _: &menu::SecondaryConfirm, window, cx| {
                    this.confirm(window, cx)
                }),
            )
            .w(px(720.))
            .child(
                h_flex()
                    .w_full()
                    .px_6()
                    .pt_5()
                    .pb_4()
                    .gap_3()
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(t!("api_client.panel.import_title"))
                                .size(LabelSize::Large)
                                .weight(FontWeight::SEMIBOLD),
                        ),
                    )
                    .child(
                        ui::segmented_control(cx)
                            .w(px(260.))
                            .child(ui::segment_option(
                                "api-import-postman",
                                t!("api_client.panel.import_source_postman"),
                                true,
                                true,
                                cx,
                            ))
                            .child(
                                ui::segment_option(
                                    "api-import-openapi",
                                    t!("api_client.panel.import_source_openapi"),
                                    false,
                                    true,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.panel
                                            .update(cx, |panel, cx| panel.show_link(cx))
                                            .log_err();
                                        cx.emit(DismissEvent);
                                    },
                                )),
                            )
                            .child(ui::segment_option(
                                "api-import-curl",
                                t!("api_client.panel.import_source_curl"),
                                false,
                                false,
                                cx,
                            )),
                    ),
            )
            .child(
                v_flex()
                    .id("api-import-body")
                    .w_full()
                    .max_h(px(520.))
                    .overflow_y_scroll()
                    .px_6()
                    .pb_5()
                    .gap_2p5()
                    .child(section_label(t!("api_client.panel.import_origin_section")))
                    .child(source_files)
                    .child(source_workspace)
                    .when(!plan.is_empty(), |this| {
                        this.child(div().h_1())
                            .child(section_label(t!("api_client.panel.import_plan_section")))
                            .child(ui::card(cx).p_3().gap_2p5().children(plan))
                    })
                    .when(counts.secrets > 0, |this| {
                        this.child(keychain_note(t!("api_client.panel.keychain_note"), cx))
                    })
                    .when_some(match_row.filter(|_| counts.requests > 0), |this, row| {
                        this.child(row)
                    })
                    .when_some(self.error.clone(), |this, error| {
                        this.child(ui::error_callout(
                            t!("api_client.panel.collection_error"),
                            error,
                            cx,
                        ))
                    }),
            )
            .child(
                ui::modal_footer(cx)
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(destination)
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .child(
                        ui::secondary_button(
                            "api-import-cancel",
                            None,
                            t!("api_client.panel.import_cancel"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(confirm),
            )
    }
}

/// What a step brings in: an icon tile, where it comes from and what it becomes, and a
/// detail on the right.
fn plan_row(
    icon: IconName,
    key: impl Into<SharedString>,
    value: impl Into<SharedString>,
    detail: AnyElement,
    cx: &App,
) -> Div {
    let colors = cx.theme().colors();
    h_flex()
        .w_full()
        .gap_2p5()
        .child(
            div()
                .flex()
                .flex_none()
                .size(px(30.))
                .items_center()
                .justify_center()
                .rounded(Radius::Md.rems())
                .bg(colors.element_background)
                .border_1()
                .border_color(colors.border_selected)
                .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    Label::new(key)
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted),
                )
                .child(
                    Label::new(value)
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM),
                ),
        )
        .child(div().flex_none().max_w(px(220.)).min_w_0().child(detail))
}

fn plan_detail(text: impl Into<SharedString>) -> AnyElement {
    Label::new(text)
        .size(LabelSize::XSmall)
        .color(Color::Muted)
        .truncate()
        .into_any_element()
}

/// A choice card with a radio; the chosen one gets the accent border and tint. The caller
/// adds the content and the click.
fn radio_card(id: impl Into<ElementId>, selected: bool, enabled: bool, cx: &App) -> Stateful<Div> {
    let accent = cx.theme().colors().border_focused;
    ui::card(cx)
        .id(id)
        .flex_row()
        .items_start()
        .px_3()
        .py_2p5()
        .gap_2p5()
        .when(selected, |this| {
            this.bg(accent.opacity(0.18)).border_color(accent)
        })
        .when(!enabled, |this| this.opacity(0.5))
        .child(div().pt_0p5().child(ui::radio_indicator(selected, cx)))
}

fn keychain_note(text: impl Into<SharedString>, cx: &App) -> Div {
    ui::card(cx)
        .flex_row()
        .items_start()
        .px_3()
        .py_2p5()
        .gap_2p5()
        .child(
            Icon::new(IconName::Lock)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted)),
        )
}

/// A `{{variable}}` set off in the accent, as the boards draw them outside editors.
pub(crate) fn variable_chip(name: impl Into<SharedString>, cx: &App) -> Div {
    h_flex()
        .flex_none()
        .h(px(20.))
        .px_1p5()
        .rounded_sm()
        .bg(cx.theme().colors().border_focused.opacity(0.16))
        .child(
            Label::new(name)
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .color(Color::Accent),
        )
}

fn synced_text(synced_at: chrono::DateTime<chrono::Local>) -> String {
    let minutes = (chrono::Local::now() - synced_at).num_minutes();
    if minutes < 1 {
        t!("api_client.panel.synced_just_now").to_string()
    } else if minutes < 60 {
        t!("api_client.panel.synced_minutes_ago", minutes = minutes).to_string()
    } else {
        t!("api_client.panel.synced_hours_ago", hours = minutes / 60).to_string()
    }
}

/// What the link screen shows about a spec before it's linked.
#[derive(Clone)]
struct CandidateSummary {
    title: String,
    format: String,
    info: String,
    operation_count: usize,
    tags: Vec<String>,
    servers: Vec<String>,
    schemes: Vec<String>,
    schema_count: usize,
}

fn summarize(path: &Path, text: &str) -> Result<CandidateSummary, String> {
    let spec = spec::load(path, text, &|path: &Path| {
        Ok(std::fs::read_to_string(path)?)
    })
    .map_err(|error| format!("{error:#}"))?;
    let schema_count = ["/components/schemas", "/definitions"]
        .iter()
        .find_map(|pointer| spec.document.pointer(pointer))
        .and_then(|schemas| schemas.as_object())
        .map_or(0, |schemas| schemas.len());
    Ok(CandidateSummary {
        title: spec.title.clone(),
        format: spec.format.label(&spec.version_string),
        info: t!(
            "api_client.panel.candidate_info",
            title = &spec.title,
            version = &spec.api_version,
            count = spec.operations.len()
        )
        .to_string(),
        operation_count: spec.operations.len(),
        tags: spec.folders().into_iter().map(|(tag, _)| tag).collect(),
        servers: spec
            .servers
            .iter()
            .map(|server| {
                server
                    .description
                    .clone()
                    .unwrap_or_else(|| server.url.clone())
            })
            .collect(),
        schemes: spec
            .security_schemes
            .iter()
            .map(|scheme| scheme.name.clone())
            .collect(),
        schema_count,
    })
}

impl Render for ApiPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match (self.view, self.collection().cloned()) {
            (_, None) if !self.discovered => v_flex()
                .size_full()
                .child(
                    EmptyState::new(IconName::LoadCircle, t!("api_client.panel.searching_apis"))
                        .spinning(true),
                )
                .into_any_element(),
            (View::Link, _) => self.render_link_view(cx),
            (_, None) => self.render_start_view(cx),
            (View::Tree | View::Start, Some(collection)) => self.render_tree_view(&collection, cx),
            (View::Settings, Some(collection)) => self.render_settings_view(&collection, cx),
        };
        v_flex()
            .key_context("ApiPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(content)
    }
}

impl Focusable for ApiPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ApiPanel {}

impl Panel for ApiPanel {
    fn enabled(&self, cx: &App) -> bool {
        task_agents::toolkit_enabled(task_agents::toolkit_ids::API, &self.project, cx)
    }

    fn persistent_name() -> &'static str {
        "ApiPanel"
    }

    fn panel_key() -> &'static str {
        "ApiPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(384.)
    }

    /// No icon: a panel icon also puts a button in the status bar's panels group, and the
    /// toolkit button is already the way in.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ArrowRightLeft)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("API")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        11
    }
}
