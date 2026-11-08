use crate::browser_import::{
    BrowserKind, BrowserLibrary, BrowserSource, CookieKey, FullDiskAccessRequired, ImportRecord,
    KeychainDenied,
};
use gpui::{
    App, Context, DismissEvent, EventEmitter, FocusHandle, Focusable, FontWeight, Render,
    SharedString, Task, Window,
};
use i18n::t;
use ui::{
    CommonAnimationExt, ContextMenu, PopoverMenu, PopoverTriggerArea, Radius, checkbox_indicator,
    modal_footer, modal_surface, prelude::*, primary_button, radio_indicator, secondary_button,
};
use workspace::{ModalView, Workspace};

/// How many logged-in sites the summary names before folding the rest into a count.
const SHOWN_SITES: usize = 4;
/// The Full Disk Access list in System Settings, the only place macOS lets the user hand
/// Safari's data to another app.
const FULL_DISK_ACCESS_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    workspace.toggle_modal(window, cx, ImportModal::new);
}

enum Stage {
    Detecting,
    Choosing,
    Importing { bookmarks_done: Option<usize> },
    Done(ImportOutcome),
}

struct ImportOutcome {
    source: String,
    bookmarks: usize,
    cookies: Result<usize, CookieFailure>,
    sites: Vec<String>,
}

enum CookieFailure {
    NotRequested,
    KeychainDenied,
    FullDiskAccess,
    Failed(String),
}

pub struct ImportModal {
    focus_handle: FocusHandle,
    sources: Vec<BrowserSource>,
    selected_source: usize,
    selected_profile: usize,
    import_bookmarks: bool,
    import_cookies: bool,
    safari_readable: bool,
    stage: Stage,
    _task: Option<Task<()>>,
}

impl EventEmitter<DismissEvent> for ImportModal {}

impl ModalView for ImportModal {}

impl Focusable for ImportModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ImportModal {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let detect = cx.background_spawn(async move {
            (
                crate::browser_import::detect_sources(),
                crate::browser_import::safari_readable(),
            )
        });
        let task = cx.spawn(async move |this, cx| {
            let (sources, safari_readable) = detect.await;
            this.update(cx, |this, cx| {
                this.sources = sources;
                this.safari_readable = safari_readable;
                this.stage = Stage::Choosing;
                cx.notify();
            })
            .ok();
        });
        Self {
            focus_handle,
            sources: Vec::new(),
            selected_source: 0,
            selected_profile: 0,
            import_bookmarks: true,
            import_cookies: true,
            safari_readable: true,
            stage: Stage::Detecting,
            _task: Some(task),
        }
    }

    fn source(&self) -> Option<&BrowserSource> {
        self.sources.get(self.selected_source)
    }

    fn needs_full_disk_access(&self) -> bool {
        !self.safari_readable
            && self
                .source()
                .is_some_and(|source| source.kind == BrowserKind::Safari)
    }

    fn select_source(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_source = index;
        self.selected_profile = 0;
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.source() else {
            return;
        };
        let Some(profile) = source.profiles.get(self.selected_profile) else {
            return;
        };
        if !self.import_bookmarks && !self.import_cookies {
            return;
        }
        let kind = source.kind;
        let label = if source.profiles.len() > 1 {
            format!("{} · {}", kind.short_name(), profile.name)
        } else {
            kind.short_name().to_string()
        };
        let directory = profile.directory.clone();
        let import_bookmarks = self.import_bookmarks;
        let import_cookies = self.import_cookies;
        self.stage = Stage::Importing {
            bookmarks_done: (!import_bookmarks).then_some(0),
        };
        cx.notify();

        self._task = Some(cx.spawn(async move |this, cx| {
            let bookmarks = if import_bookmarks {
                let directory = directory.clone();
                cx.background_spawn(async move {
                    crate::browser_import::read_bookmarks(kind, &directory)
                })
                .await
                .unwrap_or_else(|error| {
                    log::error!("importing bookmarks from {}: {error:#}", kind.name());
                    Vec::new()
                })
            } else {
                Vec::new()
            };
            let bookmark_count = bookmarks.len();
            this.update(cx, |this, cx| {
                this.stage = Stage::Importing {
                    bookmarks_done: Some(bookmark_count),
                };
                cx.notify();
            })
            .ok();

            let cookies = if import_cookies {
                cx.background_spawn(async move {
                    crate::browser_import::read_cookies(kind, &directory).await
                })
                .await
            } else {
                Ok(Vec::new())
            };

            this.update(cx, |this, cx| {
                let (cookie_result, keys, sites) = match cookies {
                    _ if !import_cookies => {
                        (Err(CookieFailure::NotRequested), Vec::new(), Vec::new())
                    }
                    Ok(cookies) => match crate::import_cookies_into_engine(&cookies, cx) {
                        Ok(accepted) => {
                            let keys = cookies
                                .iter()
                                .map(|cookie| CookieKey {
                                    url: cookie.url(),
                                    name: cookie.name.clone(),
                                })
                                .collect::<Vec<_>>();
                            (Ok(accepted), keys, logged_in_sites(&cookies))
                        }
                        Err(error) => (
                            Err(CookieFailure::Failed(format!("{error:#}"))),
                            Vec::new(),
                            Vec::new(),
                        ),
                    },
                    Err(error) if error.downcast_ref::<KeychainDenied>().is_some() => {
                        (Err(CookieFailure::KeychainDenied), Vec::new(), Vec::new())
                    }
                    Err(error) if error.downcast_ref::<FullDiskAccessRequired>().is_some() => {
                        (Err(CookieFailure::FullDiskAccess), Vec::new(), Vec::new())
                    }
                    Err(error) => {
                        log::error!("importing cookies from {}: {error:#}", kind.name());
                        (
                            Err(CookieFailure::Failed(format!("{error:#}"))),
                            Vec::new(),
                            Vec::new(),
                        )
                    }
                };
                let cookie_count = cookie_result.as_ref().map(|count| *count).unwrap_or(0);
                let record = ImportRecord {
                    source: label.clone(),
                    imported_at: unix_now(),
                    bookmarks: bookmark_count,
                    cookies: cookie_count,
                };
                let added = BrowserLibrary::update(cx, |library| {
                    library.record_import(bookmarks, keys, record)
                });
                this.stage = Stage::Done(ImportOutcome {
                    source: label,
                    bookmarks: added,
                    cookies: cookie_result,
                    sites,
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn back_to_choosing(&mut self, cx: &mut Context<Self>) {
        self.stage = Stage::Choosing;
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn render_header(&self, title: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .px(px(20.))
            .pt(px(18.))
            .pb(px(6.))
            .gap_2()
            .child(
                Label::new(title)
                    .weight(FontWeight::BOLD)
                    .size(LabelSize::Default),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("browser-import-close", IconName::Close)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
    }

    fn render_choosing(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.sources.is_empty() {
            return v_flex()
                .px(px(20.))
                .py(px(12.))
                .child(
                    ui::EmptyState::new(IconName::ToolWeb, t!("web_preview.import.none_title"))
                        .description(t!("web_preview.import.none_description")),
                )
                .into_any_element();
        }
        let colors = cx.theme().colors();
        let (selected_background, hover_background, note_background) = (
            colors.element_selected,
            colors.element_hover,
            colors.element_background,
        );
        let mut browsers = ui::grouped_block(cx).p_1();
        for (index, source) in self.sources.iter().enumerate() {
            let selected = index == self.selected_source;
            let detail = if source.profiles.len() > 1 {
                t!(
                    "web_preview.import.profiles",
                    count = source.profiles.len().to_string()
                )
            } else {
                SharedString::from(
                    source
                        .profiles
                        .first()
                        .map(|profile| profile.name.clone())
                        .unwrap_or_default(),
                )
            };
            browsers = browsers.child(
                h_flex()
                    .id(SharedString::from(format!("browser-import-source-{index}")))
                    .w_full()
                    .h(px(44.))
                    .px_2p5()
                    .gap_2p5()
                    .rounded(Radius::Md.rems())
                    .cursor_pointer()
                    .when(selected, |this| this.bg(selected_background))
                    .when(!selected, |this| {
                        this.hover(|this| this.bg(hover_background))
                    })
                    .child(radio_indicator(selected, cx))
                    .child(monogram(source.kind, selected, cx))
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                Label::new(source.kind.name())
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::MEDIUM),
                            )
                            .child(
                                Label::new(detail)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select_source(index, cx))),
            );
        }

        let mut body = v_flex()
            .w_full()
            .px(px(20.))
            .py(px(12.))
            .gap(px(16.))
            .child(
                v_flex()
                    .gap_1p5()
                    .child(ui::section_label(t!("web_preview.import.from")))
                    .child(browsers),
            );

        if let Some(source) = self.source()
            && source.profiles.len() > 1
        {
            let profile_name = source
                .profiles
                .get(self.selected_profile)
                .map(|profile| profile.name.clone())
                .unwrap_or_default();
            let names: Vec<String> = source
                .profiles
                .iter()
                .map(|profile| profile.name.clone())
                .collect();
            let this = cx.weak_entity();
            body = body.child(
                v_flex()
                    .gap_1p5()
                    .child(ui::section_label(t!(
                        "web_preview.import.profile",
                        browser = source.kind.short_name()
                    )))
                    .child(
                        PopoverMenu::new("browser-import-profile")
                            .full_width(true)
                            .trigger(PopoverTriggerArea::new(ui::select_field(
                                "browser-import-profile-field",
                                profile_name,
                                cx,
                            )))
                            .menu(move |window, cx| {
                                let this = this.clone();
                                let names = names.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for (index, name) in names.iter().enumerate() {
                                        let this = this.clone();
                                        menu = menu.entry(name.clone(), None, move |_, cx| {
                                            this.update(cx, |this, cx| {
                                                this.selected_profile = index;
                                                cx.notify();
                                            })
                                            .ok();
                                        });
                                    }
                                    menu
                                }))
                            }),
                    ),
            );
        }

        if self.needs_full_disk_access() {
            let warning = Color::Warning.color(cx);
            return body
                .child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .p_2p5()
                        .rounded(Radius::Md.rems())
                        .bg(warning.opacity(0.08))
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            h_flex()
                                .items_start()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Warning)
                                        .size(IconSize::Small)
                                        .color(Color::Warning),
                                )
                                .child(
                                    v_flex()
                                        .min_w_0()
                                        .gap_0p5()
                                        .child(
                                            Label::new(t!("web_preview.import.full_disk_title"))
                                                .size(LabelSize::Small)
                                                .weight(FontWeight::MEDIUM),
                                        )
                                        .child(
                                            Label::new(t!("web_preview.import.full_disk_detail"))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        ),
                                ),
                        )
                        .child(
                            h_flex().pl_6().child(
                                secondary_button(
                                    "browser-import-full-disk",
                                    Some((IconName::Settings, Color::Muted)),
                                    t!("web_preview.import.full_disk_open"),
                                    true,
                                    cx,
                                )
                                .on_click(|_, _, cx| cx.open_url(FULL_DISK_ACCESS_SETTINGS)),
                            ),
                        ),
                )
                .into_any_element();
        }

        let keychain_service = self
            .source()
            .and_then(|source| source.kind.keychain_service());
        body = body
            .child(
                v_flex()
                    .gap_1p5()
                    .child(ui::section_label(t!("web_preview.import.what")))
                    .child(
                        ui::grouped_block(cx)
                            .p_1()
                            .child(self.render_option(
                                "bookmarks",
                                self.import_bookmarks,
                                t!("web_preview.import.bookmarks"),
                                t!("web_preview.import.bookmarks_detail"),
                                |this| this.import_bookmarks = !this.import_bookmarks,
                                cx,
                            ))
                            .child(self.render_option(
                                "cookies",
                                self.import_cookies,
                                t!("web_preview.import.cookies"),
                                t!("web_preview.import.cookies_detail"),
                                |this| this.import_cookies = !this.import_cookies,
                                cx,
                            )),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2()
                    .p_2p5()
                    .rounded(Radius::Md.rems())
                    .bg(note_background)
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(match keychain_service {
                            Some(service) if self.import_cookies => {
                                t!("web_preview.import.keychain_note", service = service)
                            }
                            _ => t!("web_preview.import.passwords_note"),
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            );
        body.into_any_element()
    }

    fn render_option(
        &self,
        id: &'static str,
        checked: bool,
        title: SharedString,
        detail: SharedString,
        toggle: fn(&mut Self),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .id(id)
            .w_full()
            .items_start()
            .gap_2p5()
            .px_2p5()
            .py_2()
            .rounded(Radius::Md.rems())
            .cursor_pointer()
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .child(div().pt_0p5().child(checkbox_indicator(checked, cx)))
            .child(
                v_flex()
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
            .on_click(cx.listener(move |this, _, _, cx| {
                toggle(this);
                cx.notify();
            }))
    }

    fn render_importing(
        &self,
        bookmarks_done: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let step = |id: &'static str, done: Option<String>, label: SharedString, cx: &App| {
            h_flex()
                .id(id)
                .w_full()
                .h(px(34.))
                .gap_2p5()
                .child(match done {
                    Some(_) => Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Success)
                        .into_any_element(),
                    None => Icon::new(IconName::LoadCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2)
                        .into_any_element(),
                })
                .child(Label::new(label).size(LabelSize::Small))
                .child(div().flex_1())
                .when_some(done, |this, count| {
                    this.child(
                        Label::new(count)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                })
        };
        let mut steps = v_flex().w_full().px(px(20.)).py(px(12.));
        if self.import_bookmarks {
            steps = steps.child(step(
                "step-bookmarks",
                bookmarks_done.map(|count| count.to_string()),
                t!("web_preview.import.bookmarks"),
                cx,
            ));
        }
        if self.import_cookies {
            steps = steps.child(step(
                "step-cookies",
                None,
                t!("web_preview.import.cookies"),
                cx,
            ));
        }
        steps.into_any_element()
    }

    fn render_done(&self, outcome: &ImportOutcome, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let summary = match &outcome.cookies {
            Ok(cookies) => t!(
                "web_preview.import.done_summary",
                bookmarks = outcome.bookmarks.to_string(),
                cookies = cookies.to_string(),
                source = outcome.source.clone()
            ),
            Err(_) => t!(
                "web_preview.import.done_summary_bookmarks",
                bookmarks = outcome.bookmarks.to_string(),
                source = outcome.source.clone()
            ),
        };
        let mut body = v_flex()
            .w_full()
            .px(px(20.))
            .py(px(12.))
            .gap(px(14.))
            .child(Label::new(summary).size(LabelSize::Small));

        if !outcome.sites.is_empty() {
            let mut chips = h_flex().flex_wrap().gap_1p5();
            for site in outcome.sites.iter().take(SHOWN_SITES) {
                chips = chips.child(site_chip(site.clone(), Color::Default, cx));
            }
            if outcome.sites.len() > SHOWN_SITES {
                chips = chips.child(site_chip(
                    format!("+{}", outcome.sites.len() - SHOWN_SITES),
                    Color::Muted,
                    cx,
                ));
            }
            body = body.child(chips);
        }

        let (note, tone) = match &outcome.cookies {
            Ok(_) => (Some(t!("web_preview.import.done_note")), Color::Warning),
            Err(CookieFailure::KeychainDenied) => (
                Some(t!("web_preview.import.keychain_denied")),
                Color::Warning,
            ),
            Err(CookieFailure::FullDiskAccess) => (
                Some(t!("web_preview.import.full_disk_detail")),
                Color::Warning,
            ),
            Err(CookieFailure::Failed(error)) => (
                Some(t!(
                    "web_preview.import.cookies_failed",
                    error = error.clone()
                )),
                Color::Error,
            ),
            Err(CookieFailure::NotRequested) => (None, Color::Muted),
        };
        if let Some(note) = note {
            let tint = tone.color(cx);
            body = body.child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap_2()
                    .p_2p5()
                    .rounded(Radius::Md.rems())
                    .bg(tint.opacity(0.08))
                    .border_1()
                    .border_color(colors.border_variant)
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(tone),
                    )
                    .child(Label::new(note).size(LabelSize::XSmall)),
            );
        }
        body.into_any_element()
    }
}

impl Render for ImportModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, body) = match &self.stage {
            Stage::Detecting => (
                t!("web_preview.import.title"),
                v_flex()
                    .px(px(20.))
                    .py(px(24.))
                    .child(
                        ui::EmptyState::new(IconName::ToolWeb, t!("web_preview.import.detecting"))
                            .spinning(true),
                    )
                    .into_any_element(),
            ),
            Stage::Choosing => (t!("web_preview.import.title"), self.render_choosing(cx)),
            Stage::Importing { bookmarks_done } => {
                let source = self
                    .source()
                    .map(|source| source.kind.short_name())
                    .unwrap_or_default();
                (
                    t!("web_preview.import.importing", source = source),
                    self.render_importing(*bookmarks_done, cx),
                )
            }
            Stage::Done(outcome) => (
                t!("web_preview.import.done_title"),
                self.render_done(outcome, cx),
            ),
        };

        let footer = match &self.stage {
            Stage::Detecting | Stage::Importing { .. } => {
                modal_footer(cx).child(div().flex_1()).child(
                    secondary_button(
                        "browser-import-cancel",
                        None,
                        t!("web_preview.import.cancel"),
                        true,
                        cx,
                    )
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                )
            }
            Stage::Choosing => {
                let can_import = self.source().is_some()
                    && (self.import_bookmarks || self.import_cookies)
                    && !self.needs_full_disk_access();
                modal_footer(cx)
                    .child(div().flex_1())
                    .child(
                        secondary_button(
                            "browser-import-cancel",
                            None,
                            t!("web_preview.import.cancel"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .when(can_import, |this| {
                        this.child(
                            primary_button(
                                "browser-import-start",
                                Some(IconName::Download),
                                t!("web_preview.import.start"),
                                None,
                                cx,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.start(cx))),
                        )
                    })
            }
            Stage::Done(outcome) => {
                let retry_keychain = matches!(outcome.cookies, Err(CookieFailure::KeychainDenied));
                modal_footer(cx)
                    .child(div().flex_1())
                    .child(
                        secondary_button(
                            "browser-import-again",
                            None,
                            if retry_keychain {
                                t!("web_preview.import.retry")
                            } else {
                                t!("web_preview.import.another")
                            },
                            true,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if retry_keychain {
                                this.import_bookmarks = false;
                                this.start(cx);
                            } else {
                                this.back_to_choosing(cx);
                            }
                        })),
                    )
                    .child(
                        primary_button(
                            "browser-import-finish",
                            None,
                            t!("web_preview.import.finish"),
                            None,
                            cx,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
            }
        };

        modal_surface(cx)
            .key_context("BrowserImport")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .w(px(520.))
            .child(self.render_header(title, cx))
            .child(body)
            .child(footer)
    }
}

fn monogram(kind: BrowserKind, selected: bool, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    div()
        .flex_none()
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(Radius::Sm.rems())
        .bg(if selected {
            colors.element_active
        } else {
            colors.element_background
        })
        .child(
            Label::new(kind.monogram())
                .size(LabelSize::Small)
                .weight(FontWeight::BOLD),
        )
}

fn site_chip(label: impl Into<SharedString>, color: Color, cx: &App) -> impl IntoElement {
    h_flex()
        .h(px(22.))
        .px_2()
        .rounded_full()
        .bg(cx.theme().colors().element_background)
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .color(color)
                .buffer_font(cx),
        )
}

/// The sites with the most cookies, which are the ones the user is most likely logged in to.
fn logged_in_sites(cookies: &[crate::browser_import::ImportedCookie]) -> Vec<String> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for cookie in cookies {
        *counts.entry(site_of(cookie.host())).or_default() += 1;
    }
    let mut sites: Vec<(&str, usize)> = counts.into_iter().collect();
    sites.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
    sites
        .into_iter()
        .map(|(site, _)| site.to_string())
        .collect()
}

/// The registrable part of a host, close enough for a summary: `accounts.google.com` is
/// `google.com`, and `app.nubank.com.br` keeps the country's second level.
fn site_of(host: &str) -> &str {
    let labels: Vec<&str> = host.split('.').collect();
    let count = labels.len();
    let keep = match (
        labels.get(count.wrapping_sub(2)),
        labels.get(count.wrapping_sub(1)),
    ) {
        (Some(second), Some(top)) if count >= 3 && second.len() <= 3 && top.len() == 2 => 3,
        _ => 2,
    };
    if count <= keep {
        return host;
    }
    let skipped: usize = labels
        .iter()
        .take(count - keep)
        .map(|label| label.len() + 1)
        .sum();
    host.get(skipped..).unwrap_or(host)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_fold_subdomains() {
        assert_eq!(site_of("accounts.google.com"), "google.com");
        assert_eq!(site_of("github.com"), "github.com");
        assert_eq!(site_of("app.nubank.com.br"), "nubank.com.br");
        assert_eq!(site_of("localhost"), "localhost");
    }
}
