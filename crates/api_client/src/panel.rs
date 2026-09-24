use crate::{
    ToggleFocus,
    collection::{self, Collection, LoginStatus},
    config::{Environment, HeaderEntry, LoginConfig, Variable},
    request_view::{self, method_label},
    send,
    spec::{self, SchemeKind},
    vars::SECRET_PREFIX,
};
use collections::HashSet;
use editor::Editor;
use fs::Fs;
use gpui::{
    Action as _, AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Pixels,
    Subscription, Task, WeakEntity, px, uniform_list,
};
use project::Project;
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use ui::{
    CommonAnimationExt as _, ContextMenu, Divider, Indicator, PopoverMenu, SwitchField,
    ToggleState, Tooltip, prelude::*,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    OpenOptions, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

/// Specs found in the project that aren't linked yet.
const MAX_CANDIDATES: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Tree,
    Settings,
    Link,
}

struct Candidate {
    root: PathBuf,
    path: PathBuf,
    relative: String,
    summary: Option<Result<String, String>>,
    operation_count: Option<usize>,
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
        path: String,
        deprecated: bool,
    },
    SavedHeader,
    Saved {
        id: String,
        name: String,
        operation_key: String,
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
    filter_input: Entity<InputField>,
    expanded: HashSet<String>,
    settings: Option<SettingsForm>,
    discover_task: Task<()>,
    collection_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl ApiPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let fs = workspace.app_state().fs.clone();
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, "Filtrar por rota, método ou tag…")
                .start_icon(IconName::MagnifyingGlass)
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
            filter_input,
            expanded: HashSet::default(),
            settings: None,
            discover_task: Task::ready(()),
            collection_subscriptions: Vec::new(),
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
                    .background_spawn(async move {
                        spec::load(&summary_path, &text, &|path: &Path| {
                            Ok(std::fs::read_to_string(path)?)
                        })
                        .map(|spec| {
                            let summary = format!(
                                "{} · {} {} · {} operações",
                                spec.format.label(&spec.version_string),
                                spec.title,
                                spec.api_version,
                                spec.operations.len()
                            );
                            (summary, spec.operations.len())
                        })
                        .map_err(|error| format!("{error:#}"))
                    })
                    .await;
                let operation_count = summary.as_ref().ok().map(|(_, count)| *count);
                found.push(Candidate {
                    root,
                    path,
                    relative,
                    summary: Some(summary.map(|(summary, _)| summary)),
                    operation_count,
                });
            }
            this.update(cx, |this, cx| {
                this.set_collections(files, cx);
                // The main spec is usually the biggest one; readable ones go before broken ones.
                found.sort_by_key(|candidate| {
                    std::cmp::Reverse(candidate.operation_count.unwrap_or(0))
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
            self.view = View::Link;
        } else if self.view == View::Link && self.link_task.is_none() {
            self.view = View::Tree;
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
                        let mut files: Vec<(PathBuf, PathBuf)> = this
                            .collections
                            .iter()
                            .map(|collection| {
                                let collection = collection.read(cx);
                                (collection.root.clone(), collection.file_path.clone())
                            })
                            .collect();
                        files.push((root, file.clone()));
                        this.set_collections(files, cx);
                        this.selected = this
                            .collections
                            .iter()
                            .position(|collection| collection.read(cx).file_path == file)
                            .unwrap_or(0);
                        this.view = View::Tree;
                    }
                    Err(error) => this.link_error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
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
        let Some(spec) = collection.spec.as_ref() else {
            return Vec::new();
        };
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
        for (tag, operations) in spec.folders() {
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
                        path: operation.path.clone(),
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
                    .operation(&saved.operation)
                    .map(|operation| operation.method.clone())
                    .unwrap_or_else(|| "?".to_string());
                rows.push(TreeRow::Saved {
                    id: saved.id.clone(),
                    name: saved.name.clone(),
                    operation_key: saved.operation.clone(),
                    method,
                });
            }
        }
        rows
    }

    fn render_row(&self, index: usize, row: TreeRow, cx: &mut Context<Self>) -> AnyElement {
        let base = h_flex()
            .id(("api-row", index))
            .w_full()
            .h(px(26.))
            .px_2()
            .gap_1p5()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().element_hover));
        match row {
            TreeRow::Folder {
                name,
                count,
                expanded,
            } => base
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
                    Icon::new(if expanded {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    })
                    .size(IconSize::Small)
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
                path,
                deprecated,
            } => base
                .pl(px(30.))
                .child(div().w(px(44.)).child(method_label(&method, cx)))
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .color(if deprecated {
                                Color::Muted
                            } else {
                                Color::Default
                            })
                            .when(deprecated, |label| label.strikethrough())
                            .truncate(),
                    ),
                )
                .child(
                    div().max_w(px(150.)).child(
                        Label::new(path)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                    ),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_operation(key.clone(), window, cx);
                }))
                .into_any_element(),
            TreeRow::SavedHeader => h_flex()
                .h(px(26.))
                .px_2()
                .gap_1p5()
                .child(
                    Icon::new(IconName::Star)
                        .size(IconSize::XSmall)
                        .color(Color::Warning),
                )
                .child(
                    Label::new("SALVAS POR MIM")
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Muted),
                )
                .into_any_element(),
            TreeRow::Saved {
                id,
                name,
                operation_key,
                method,
            } => {
                let delete_id = id.clone();
                base.pl(px(30.))
                    .child(div().w(px(44.)).child(method_label(&method, cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(name).size(LabelSize::Small).truncate()),
                    )
                    .child(
                        Label::new(operation_key)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .child(
                        IconButton::new(("api-saved-delete", index), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Apagar"))
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

    fn render_header(
        &self,
        collection: &Entity<Collection>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.weak_entity();
        let collection_entity = collection.clone();
        let collection = collection.read(cx);
        let version = collection
            .spec
            .as_ref()
            .map(|spec| spec.api_version.clone())
            .filter(|version| !version.is_empty());
        let synced = match (&collection.spec_error, collection.synced_at) {
            (Some(_), _) => ("erro ao ler o spec".to_string(), Color::Error),
            (None, Some(synced_at)) => (
                format!("sincronizado às {}", synced_at.format("%H:%M")),
                Color::Success,
            ),
            (None, None) => ("lendo o spec…".to_string(), Color::Muted),
        };
        let environments: Vec<String> = collection
            .environments()
            .iter()
            .map(|environment| environment.name.clone())
            .collect();
        let active = collection
            .active_environment()
            .map(|environment| environment.name.clone())
            .unwrap_or_else(|| "sem ambiente".to_string());
        let titles: Vec<(usize, String)> = self
            .collections
            .iter()
            .enumerate()
            .map(|(index, collection)| (index, collection.read(cx).title()))
            .collect();
        let has_many = titles.len() > 1;
        v_flex()
            .px_3()
            .pt_2p5()
            .gap_1()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        div().flex_1().min_w_0().child(
                            h_flex()
                                .gap_1p5()
                                .child(
                                    Label::new(collection.title())
                                        .size(LabelSize::Default)
                                        .weight(FontWeight::SEMIBOLD)
                                        .truncate(),
                                )
                                .when_some(version, |this, version| {
                                    this.child(
                                        Label::new(format!("v{version}"))
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .color(Color::Muted),
                                    )
                                }),
                        ),
                    )
                    .child(
                        PopoverMenu::new("api-environment")
                            .trigger(
                                Button::new("api-environment-button", active.clone())
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::Small)
                                    .start_icon(
                                        Icon::new(IconName::Server)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .end_icon(
                                        Icon::new(IconName::ChevronDown).size(IconSize::XSmall),
                                    ),
                            )
                            .anchor(gpui::Anchor::TopRight)
                            .menu(move |window, cx| {
                                let environments = environments.clone();
                                let active = active.clone();
                                let collection = collection_entity.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    menu = menu.header("Ambiente");
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
                    )
                    .child(
                        IconButton::new("api-settings", IconName::Sliders)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Login, headers e ambientes"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings(window, cx);
                            })),
                    )
                    .child(
                        PopoverMenu::new("api-collection-menu")
                            .trigger(
                                IconButton::new("api-collection-menu-button", IconName::Ellipsis)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted),
                            )
                            .anchor(gpui::Anchor::TopRight)
                            .menu(move |window, cx| {
                                let this = this.clone();
                                let titles = titles.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    if has_many {
                                        menu = menu.header("Coleções");
                                        for (index, title) in titles.iter() {
                                            let this = this.clone();
                                            let index = *index;
                                            menu = menu.entry(title.clone(), None, move |_, cx| {
                                                this.update(cx, |this, cx| {
                                                    this.selected = index;
                                                    this.view = View::Tree;
                                                    cx.notify();
                                                })
                                                .log_err();
                                            });
                                        }
                                        menu = menu.separator();
                                    }
                                    let open_spec = this.clone();
                                    let reload = this.clone();
                                    let link = this;
                                    menu.entry("Abrir o spec", None, move |window, cx| {
                                        open_spec
                                            .update(cx, |this, cx| this.open_spec(window, cx))
                                            .log_err();
                                    })
                                    .entry("Reler o spec", None, move |_, cx| {
                                        reload
                                            .update(cx, |this, cx| {
                                                if let Some(collection) = this.collection() {
                                                    collection.update(cx, |collection, cx| {
                                                        collection.reload_spec(cx)
                                                    });
                                                }
                                            })
                                            .log_err();
                                    })
                                    .entry(
                                        "Vincular outra API…",
                                        None,
                                        move |_, cx| {
                                            link.update(cx, |this, cx| {
                                                this.view = View::Link;
                                                this.discover(cx);
                                            })
                                            .log_err();
                                        },
                                    )
                                }))
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Indicator::dot().color(synced.1))
                    .child(
                        div().min_w_0().child(
                            Label::new(collection.file.spec.clone())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .child(
                        Label::new(format!("· {}", synced.0))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }

    fn render_auth_card(
        &self,
        collection: &Entity<Collection>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collection_entity = collection.clone();
        let collection = collection.read(cx);
        let spec = collection.spec.clone();
        let has_bearer = spec.as_ref().is_some_and(|spec| {
            spec.security_schemes
                .iter()
                .any(|scheme| matches!(scheme.kind, SchemeKind::Bearer { .. } | SchemeKind::OAuth2))
        });
        if !has_bearer && collection.file.auth.login.is_none() {
            return div().into_any_element();
        }
        let (icon, icon_color, title, detail): (IconName, Color, String, String) =
            match (&collection.login_status, collection.session.is_logged_in()) {
                (LoginStatus::LoggingIn, _) => (
                    IconName::LoadCircle,
                    Color::Muted,
                    "Entrando…".to_string(),
                    collection
                        .file
                        .auth
                        .login
                        .as_ref()
                        .map(|login| login.operation.clone())
                        .unwrap_or_default(),
                ),
                (LoginStatus::Failed(error), _) => (
                    IconName::Warning,
                    Color::Error,
                    "O login falhou".to_string(),
                    error.to_string(),
                ),
                (LoginStatus::Idle, true) => {
                    let who = collection
                        .login_label()
                        .map(|label| format!("Logado como {label}"))
                        .unwrap_or_else(|| "Logado".to_string());
                    let expiry = match collection.session.seconds_left() {
                        Some(seconds) if seconds <= 0 => "token expirado".to_string(),
                        Some(seconds) if seconds < 120 => format!("token expira em {seconds} s"),
                        Some(seconds) if seconds < 7200 => {
                            format!("token expira em {} min", seconds / 60)
                        }
                        Some(seconds) => format!("token expira em {} h", seconds / 3600),
                        None => "token sem validade conhecida".to_string(),
                    };
                    let scheme = collection
                        .file
                        .auth
                        .login
                        .as_ref()
                        .map(|login| login.scheme.clone())
                        .unwrap_or_default();
                    (
                        IconName::UserCheck,
                        Color::Success,
                        who,
                        format!("{scheme} · {expiry}"),
                    )
                }
                (LoginStatus::Idle, false) => match &collection.file.auth.login {
                    Some(login) => (
                        IconName::Person,
                        Color::Muted,
                        "Não logado".to_string(),
                        format!("login por {}", login.operation),
                    ),
                    None => (
                        IconName::Person,
                        Color::Muted,
                        "Login não configurado".to_string(),
                        "o spec tem bearer, mas nenhuma rota de login foi achada".to_string(),
                    ),
                },
            };
        let logged_in = collection.session.is_logged_in();
        let logging_in = collection.login_status == LoginStatus::LoggingIn;
        let has_login = collection.file.auth.login.is_some();
        let has_credentials = !collection.login_secret_names().is_empty()
            && collection
                .login_secret_names()
                .iter()
                .all(|name| collection.secret(name).is_some());
        h_flex()
            .mx_3()
            .mt_2()
            .p_2()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(if logging_in {
                Icon::new(icon)
                    .size(IconSize::Small)
                    .color(icon_color)
                    .with_rotate_animation(2)
                    .into_any_element()
            } else {
                Icon::new(icon)
                    .size(IconSize::Small)
                    .color(icon_color)
                    .into_any_element()
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM)
                            .truncate(),
                    )
                    .child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .when(has_login && !logging_in, |this| {
                if logged_in {
                    let renew = collection_entity.clone();
                    let logout = collection_entity.clone();
                    this.child(
                        Button::new("api-renew", "Renovar")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _, cx| {
                                renew.update(cx, |collection, cx| {
                                    collection.renew(cx).detach_and_log_err(cx)
                                });
                            }),
                    )
                    .child(
                        IconButton::new("api-logout", IconName::Exit)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Sair e apagar o token"))
                            .on_click(move |_, _, cx| {
                                logout.update(cx, |collection, cx| collection.logout(cx));
                            }),
                    )
                } else if has_credentials {
                    let login = collection_entity.clone();
                    this.child(
                        Button::new("api-login", "Entrar")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _, cx| {
                                login.update(cx, |collection, cx| collection.start_login(cx));
                            }),
                    )
                } else {
                    this.child(
                        Button::new("api-configure-login", "Configurar")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_settings(window, cx)),
                            ),
                    )
                }
            })
            .when(!has_login, |this| {
                this.child(
                    Button::new("api-configure-login", "Configurar")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_settings(window, cx)),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_tree_view(
        &self,
        collection: &Entity<Collection>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.tree_rows(cx);
        let spec_error = collection.read(cx).spec_error.clone();
        let file_error = collection.read(cx).file_error.clone();
        let spec = collection.read(cx).spec.clone();
        let summary = spec.as_ref().map(|spec| {
            format!(
                "{} operações · {} pastas · {} servers",
                spec.operations.len(),
                spec.folders().len(),
                spec.servers.len()
            )
        });
        let warnings = spec
            .as_ref()
            .map(|spec| spec.warnings.clone())
            .unwrap_or_default();
        let row_count = rows.len();
        v_flex()
            .size_full()
            .child(self.render_header(collection, cx))
            .child(div().px_3().pt_2().child(self.filter_input.clone()))
            .child(self.render_auth_card(collection, cx))
            .when_some(file_error.or(spec_error), |this, error| {
                this.child(
                    h_flex()
                        .mx_3()
                        .mt_2()
                        .p_2()
                        .gap_2()
                        .rounded_md()
                        .bg(Color::Error.color(cx).opacity(0.1))
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::Small)
                                .color(Color::Error),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(error)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error),
                            ),
                        ),
                )
            })
            .child(
                h_flex()
                    .px_3()
                    .pt_3()
                    .pb_1()
                    .gap_1p5()
                    .child(
                        Label::new("ROTAS")
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .when_some(spec.as_ref(), |this, spec| {
                        this.child(
                            Label::new(spec.operations.len().to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("api-expand-all", "Expandir")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(collection) = this.collection() else {
                                    return;
                                };
                                let tags: Vec<String> = collection
                                    .read(cx)
                                    .spec
                                    .as_ref()
                                    .map(|spec| {
                                        spec.folders().into_iter().map(|(tag, _)| tag).collect()
                                    })
                                    .unwrap_or_default();
                                if tags.iter().all(|tag| this.expanded.contains(tag)) {
                                    this.expanded.clear();
                                } else {
                                    this.expanded.extend(tags);
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div().flex_1().min_h_0().px_1().child(
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
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .when_some(summary, |this, summary| {
                        this.child(
                            Label::new(summary)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .when(!warnings.is_empty(), |this| {
                        let tooltip = warnings
                            .iter()
                            .take(8)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n");
                        this.child(
                            div()
                                .id("api-warnings")
                                .child(
                                    Label::new(if warnings.len() == 1 {
                                        "· 1 aviso".to_string()
                                    } else {
                                        format!("· {} avisos", warnings.len())
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Warning),
                                )
                                .tooltip(Tooltip::text(tooltip)),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("api-open-spec", "Abrir spec")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::XSmall)
                            .color(Color::Accent)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_spec(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_link_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let linking = self.link_task.is_some();
        let selected = self.selected_candidate;
        let can_link = self
            .candidates
            .get(selected)
            .is_some_and(|candidate| matches!(candidate.summary, Some(Ok(_))));
        v_flex()
            .size_full()
            .px_3()
            .pt_3()
            .gap_2()
            .when(!self.collections.is_empty(), |this| {
                this.child(
                    Button::new("api-link-back", "Rotas")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::ChevronLeft).size(IconSize::XSmall))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.view = View::Tree;
                            cx.notify();
                        })),
                )
            })
            .child(
                Label::new("Vincular uma API")
                    .size(LabelSize::Large)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new(
                    "O Asylum lê o spec do projeto e monta a coleção. O yml continua sendo a fonte.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                Label::new("ENCONTRADOS NO PROJETO")
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
            .when(!self.discovered, |this| {
                this.child(
                    Label::new("Procurando arquivos openapi/swagger…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(self.discovered && self.candidates.is_empty(), |this| {
                this.child(
                    Label::new(
                        "Nenhum arquivo com openapi ou swagger no nome (yml, yaml ou json) foi achado nas pastas do projeto.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .child(
                v_flex()
                    .id("api-candidates")
                    .gap_1()
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .children(self.candidates.iter().enumerate().map(|(index, candidate)| {
                        let active = index == selected;
                        let (detail, detail_color) = match &candidate.summary {
                            Some(Ok(summary)) => (summary.clone(), Color::Muted),
                            Some(Err(error)) => (error.clone(), Color::Error),
                            None => ("lendo…".to_string(), Color::Muted),
                        };
                        v_flex()
                            .id(("api-candidate", index))
                            .p_2()
                            .gap_0p5()
                            .rounded_md()
                            .border_1()
                            .cursor_pointer()
                            .border_color(if active {
                                cx.theme().colors().border_focused
                            } else {
                                cx.theme().colors().border
                            })
                            .when(active, |this| this.bg(cx.theme().colors().element_selected))
                            .child(
                                Label::new(candidate.relative.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx),
                            )
                            .child(
                                Label::new(detail)
                                    .size(LabelSize::XSmall)
                                    .color(detail_color)
                                    .truncate(),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_candidate = index;
                                cx.notify();
                            }))
                    })),
            )
            .child(
                Label::new(
                    "Cria .asylum/api/<nome>.json na raiz do projeto com ambientes, headers e requests salvas. Senhas e tokens ficam no keychain do perfil.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .when_some(self.link_error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                Button::new(
                    "api-link",
                    if linking { "Vinculando…" } else { "Vincular" },
                )
                .full_width()
                .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                .start_icon(Icon::new(IconName::Link).size(IconSize::Small))
                .disabled(!can_link || linking)
                .on_click(cx.listener(|this, _, _, cx| this.link(cx))),
            )
            .into_any_element()
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let mut scheme_values: Vec<(String, &'static str, String, bool, String)> = Vec::new();
        if let Some(spec) = snapshot.spec.as_ref() {
            for scheme in &spec.security_schemes {
                let parts: &[(&'static str, bool)] = match &scheme.kind {
                    SchemeKind::ApiKey { .. } => &[("chave", true)],
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
                    let label = match &scheme.kind {
                        SchemeKind::ApiKey { name: header, .. } => {
                            format!("Chave de {} ({header}) · keychain", scheme.name)
                        }
                        _ => format!("{part} de {} · keychain", scheme.name),
                    };
                    scheme_values.push((name, part, value, *masked, label));
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
                let input = self.input(&name, &value, masked, window, cx);
                (name, input)
            })
            .collect();
        let scheme_secrets = scheme_values
            .into_iter()
            .map(|(name, placeholder, value, masked, label)| {
                let input = self.input(placeholder, &value, masked, window, cx);
                (name, label, input)
            })
            .collect();
        let headers = header_values
            .iter()
            .map(|header| {
                (
                    self.input("nome", &header.name, false, window, cx),
                    self.input("valor", &header.value, false, window, cx),
                    header.enabled,
                )
            })
            .collect();
        let environment_variables = environment
            .variables
            .iter()
            .map(|variable| {
                (
                    self.input("nome", &variable.name, false, window, cx),
                    self.input("valor", &variable.value, false, window, cx),
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
                "$.expiresIn (vazio = exp do JWT)",
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
            new_environment: self.input("nome do ambiente novo", "", false, window, cx),
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
        let environments: Vec<String> = collection_read
            .environments()
            .iter()
            .map(|environment| environment.name.clone())
            .collect();
        let section = |text: &'static str| {
            Label::new(text)
                .size(LabelSize::XSmall)
                .weight(FontWeight::SEMIBOLD)
                .color(Color::Muted)
        };
        let field_label =
            |text: String| Label::new(text).size(LabelSize::XSmall).color(Color::Muted);

        let post_operations: Vec<(String, String)> = spec
            .as_ref()
            .map(|spec| {
                spec.operations
                    .iter()
                    .filter(|operation| operation.method == "POST")
                    .map(|operation| (operation.key.clone(), operation.title()))
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
            let label = current.clone().unwrap_or_else(|| "Nenhuma".to_string());
            PopoverMenu::new(id)
                .trigger(
                    Button::new(SharedString::from(format!("{id}-button")), label)
                        .full_width()
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
                )
                .menu(move |window, cx| {
                    let this = this.clone();
                    let operations = operations.clone();
                    let current = current.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        if allow_none {
                            let this = this.clone();
                            menu = menu.toggleable_entry(
                                "Nenhuma",
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
                        for (key, title) in operations.iter().take(120) {
                            let this = this.clone();
                            let chosen = key.clone();
                            menu = menu.toggleable_entry(
                                format!("{key} · {title}"),
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
                        (scheme.name.clone(), scheme.summary(), uses)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let toggles = [
            (
                "api-renew-before",
                "Renovar 5 min antes de expirar",
                form.renew_before_expiry,
                (|form: &mut SettingsForm| form.renew_before_expiry = !form.renew_before_expiry)
                    as fn(&mut SettingsForm),
            ),
            (
                "api-retry-401",
                "Ao receber 401, renovar e repetir a request",
                form.retry_on_unauthorized,
                |form: &mut SettingsForm| form.retry_on_unauthorized = !form.retry_on_unauthorized,
            ),
            (
                "api-remember",
                "Lembrar o login entre aberturas do Asylum",
                form.remember_session,
                |form: &mut SettingsForm| form.remember_session = !form.remember_session,
            ),
        ];

        let content = v_flex()
            .gap_2()
            .px_3()
            .pt_2()
            .pb_4()
            .child(
                h_flex()
                    .child(
                        Button::new("api-settings-back", "Rotas")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::ChevronLeft).size(IconSize::XSmall))
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
                Label::new("Login, headers e ambientes")
                    .size(LabelSize::Large)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new("Valem para todas as rotas da coleção. Credenciais ficam no keychain do perfil; o resto vai para o arquivo da coleção.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(section("ESQUEMAS DO SPEC"))
            .when(schemes.is_empty(), |this| {
                this.child(field_label("O spec não declara securitySchemes.".to_string()))
            })
            .children(schemes.into_iter().map(|(name, summary, uses)| {
                h_flex()
                    .p_2()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(Label::new(name).size(LabelSize::Small).buffer_font(cx))
                            .child(field_label(summary)),
                    )
                    .child(field_label(format!("{uses} rotas")))
            }))
            .children(form.scheme_secrets.iter().map(|(_, label, input)| {
                v_flex()
                    .gap_1()
                    .child(field_label(label.clone()))
                    .child(input.clone())
            }))
            .child(Divider::horizontal())
            .child(section("LOGIN"))
            .child(field_label("Request de login".to_string()))
            .child(operation_picker(
                "api-login-operation",
                form.login_operation.clone(),
                true,
                |form, operation| form.login_operation = operation,
                cx,
            ))
            .child(field_label("Body do login (use {{secret.nome}} para credenciais)".to_string()))
            .child(
                div()
                    .p_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(form.login_body.clone()),
            )
            .when(!form.credentials.is_empty(), |this| {
                this.child(field_label("Credenciais (keychain)".to_string()))
            })
            .children(form.credentials.iter().map(|(name, input)| {
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(90.))
                            .child(Label::new(name.clone()).size(LabelSize::XSmall).buffer_font(cx).color(Color::Muted)),
                    )
                    .child(div().flex_1().child(input.clone()))
            }))
            .child(field_label("Extrair da resposta".to_string()))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(form.token_path.clone()))
                    .child(field_label("→ {{token}}".to_string())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(form.refresh_token_path.clone()))
                    .child(field_label("→ {{refreshToken}}".to_string())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(form.expires_in_path.clone()))
                    .child(field_label("→ expira (s)".to_string())),
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
                        Label::new("Caminho não suportado: use $.a.b, $['a-b'] ou $.lista[0]")
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    )
                },
            )
            .child(field_label("Renovação".to_string()))
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
            .children(toggles.into_iter().map(|(id, label, state, flip)| {
                let this = cx.weak_entity();
                SwitchField::new(
                    id,
                    Some(label),
                    None,
                    if state { ToggleState::Selected } else { ToggleState::Unselected },
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
            }))
            .when_some(
                match &login_status {
                    LoginStatus::Failed(error) => Some(error.clone()),
                    _ => None,
                },
                |this, error| this.child(Label::new(error).size(LabelSize::XSmall).color(Color::Error)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(
                            "api-test-login",
                            if login_status == LoginStatus::LoggingIn {
                                "Entrando…"
                            } else {
                                "Salvar e testar login"
                            },
                        )
                        .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::PlayOutlined).size(IconSize::XSmall))
                        .disabled(form.login_operation.is_none() || login_status == LoginStatus::LoggingIn)
                        .on_click(cx.listener(|this, _, window, cx| this.save_settings(true, window, cx))),
                    )
                    .when(logged_in, |this| {
                        let collection = collection.clone();
                        this.child(
                            Button::new("api-settings-logout", "Sair e apagar token")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::Exit).size(IconSize::XSmall))
                                .on_click(move |_, _, cx| {
                                    collection.update(cx, |collection, cx| collection.logout(cx));
                                }),
                        )
                    }),
            )
            .child(Divider::horizontal())
            .child(section("HEADERS DA COLEÇÃO"))
            .children(form.headers.iter().enumerate().map(|(index, (name, value, enabled))| {
                h_flex()
                    .gap_1()
                    .child(
                        ui::Checkbox::new(("api-collection-header", index), (*enabled).into()).on_click(
                            cx.listener(move |this, _, _, cx| {
                                if let Some(form) = this.settings.as_mut()
                                    && let Some(header) = form.headers.get_mut(index)
                                {
                                    header.2 = !header.2;
                                    form.dirty = true;
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(div().w(px(130.)).child(name.clone()))
                    .child(div().flex_1().child(value.clone()))
                    .child(
                        IconButton::new(("api-collection-header-remove", index), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = this.settings.as_mut()
                                    && index < form.headers.len()
                                {
                                    form.headers.remove(index);
                                    form.dirty = true;
                                }
                                cx.notify();
                            })),
                    )
            }))
            .child(
                Button::new("api-collection-header-add", "Adicionar header")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .color(Color::Accent)
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall).color(Color::Accent))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let name = this.input("nome", "", false, window, cx);
                        let value = this.input("valor", "", false, window, cx);
                        if let Some(form) = this.settings.as_mut() {
                            form.headers.push((name, value, true));
                            form.dirty = true;
                        }
                        cx.notify();
                    })),
            )
            .child(field_label(
                "Authorization vem do login (bearer) e não precisa estar aqui. Rotas com security: [] não recebem credenciais."
                    .to_string(),
            ))
            .child(Divider::horizontal())
            .child(section("AMBIENTES"))
            .child(
                h_flex().gap_1().flex_wrap().children(environments.iter().enumerate().map(|(index, name)| {
                    let active = *name == form.environment_name;
                    let chosen = name.clone();
                    Button::new(("api-environment-tab", index), name.clone())
                        .style(if active { ButtonStyle::Filled } else { ButtonStyle::Subtle })
                        .toggle_state(active)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // Switching edits another environment; save what was typed first.
                            this.save_settings(false, window, cx);
                            if let Some(collection) = this.collection().cloned() {
                                collection.update(cx, |collection, cx| {
                                    collection.set_active_environment(chosen.clone(), cx)
                                });
                                this.settings = Some(this.build_settings_form(&collection, window, cx));
                            }
                            cx.notify();
                        }))
                })),
            )
            .child(field_label(format!(
                "Variáveis de {} ({{{{baseUrl}}}} é a URL base)",
                form.environment_name
            )))
            .children(form.environment_variables.iter().enumerate().map(|(index, (name, value))| {
                h_flex()
                    .gap_1()
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
            }))
            .child(
                Button::new("api-variable-add", "Adicionar variável")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .color(Color::Accent)
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall).color(Color::Accent))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let name = this.input("nome", "", false, window, cx);
                        let value = this.input("valor", "", false, window, cx);
                        if let Some(form) = this.settings.as_mut() {
                            form.environment_variables.push((name, value));
                            form.dirty = true;
                        }
                        cx.notify();
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(div().flex_1().child(form.new_environment.clone()))
                    .child(
                        Button::new("api-environment-add", "Criar ambiente")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
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
                                            if !file.environments.iter().any(|environment| environment.name == new_name) {
                                                file.environments.push(Environment {
                                                    name: new_name,
                                                    variables: vec![Variable {
                                                        name: "baseUrl".to_string(),
                                                        value: base_url,
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
                    .px_3()
                    .py_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new(if form.dirty {
                            "Alterações não salvas"
                        } else {
                            ""
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("api-settings-save", "Salvar")
                            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_settings(false, window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ApiPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match (self.view, self.collection().cloned()) {
            (View::Link, _) | (_, None) => {
                if !self.discovered && self.collections.is_empty() {
                    v_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .child(
                            Label::new("Procurando APIs no projeto…")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .into_any_element()
                } else {
                    self.render_link_view(cx)
                }
            }
            (View::Tree, Some(collection)) => self.render_tree_view(&collection, cx),
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
