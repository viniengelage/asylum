//! A pull request's changes as the host has them, in an editor tab: one card per file, with
//! the inline comment threads under the lines they are on. Works without a checkout, so forks
//! and branches that aren't local open the same way.

use crate::{
    agent_toolkit::file_section,
    api::{ChangedFile, Comment, FileChange, PullRequest, PullRequestDetail, RemoteRepository},
    host,
    panel::{avatar, mention_action, relative_age},
};
use chrono::Utc;
use db::kvp::KeyValueStore;
use file_icons::FileIcons;
use gpui::{
    Action as _, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    ListAlignment, ListState, Task, list, px,
};
use i18n::t;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};
use ui::{
    Disclosure, EmptyState, EmptyStateTone, StatusTone, Tooltip, card, checkbox_indicator,
    prelude::*, segment_option, segmented_control, status_chip, status_pill, toolbar_rail,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

const LINE_HEIGHT: f32 = 20.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffLineKind {
    Hunk,
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffLine {
    kind: DiffLineKind,
    old_line: Option<u64>,
    new_line: Option<u64>,
    text: String,
}

/// A row of the side-by-side view; the indices point into the file's unified lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SplitRow {
    Hunk(usize),
    Pair {
        left: Option<usize>,
        right: Option<usize>,
    },
}

fn parse_file_diff(section: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut old_line = 0;
    let mut new_line = 0;
    let mut in_hunk = false;
    for text in section.lines() {
        if text.starts_with("@@") {
            if let Some((old_start, new_start)) = parse_hunk_header(text) {
                old_line = old_start;
                new_line = new_start;
                in_hunk = true;
                lines.push(DiffLine {
                    kind: DiffLineKind::Hunk,
                    old_line: None,
                    new_line: None,
                    text: text.to_string(),
                });
                continue;
            }
        }
        if !in_hunk {
            continue;
        }
        if let Some(text) = text.strip_prefix('+') {
            lines.push(DiffLine {
                kind: DiffLineKind::Added,
                old_line: None,
                new_line: Some(new_line),
                text: text.to_string(),
            });
            new_line += 1;
        } else if let Some(text) = text.strip_prefix('-') {
            lines.push(DiffLine {
                kind: DiffLineKind::Removed,
                old_line: Some(old_line),
                new_line: None,
                text: text.to_string(),
            });
            old_line += 1;
        } else if text.starts_with('\\') {
            // "\ No newline at end of file" belongs to the line before it.
        } else if text.starts_with("diff --git ") {
            in_hunk = false;
        } else {
            lines.push(DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(old_line),
                new_line: Some(new_line),
                text: text.strip_prefix(' ').unwrap_or(text).to_string(),
            });
            old_line += 1;
            new_line += 1;
        }
    }
    lines
}

/// The start lines of `@@ -12,5 +12,7 @@`; a count may be left out, as in `@@ -1 +1 @@`.
fn parse_hunk_header(header: &str) -> Option<(u64, u64)> {
    let mut ranges = header.strip_prefix("@@ ")?.split(' ');
    let old_start = ranges.next()?.strip_prefix('-')?.split(',').next()?;
    let new_start = ranges.next()?.strip_prefix('+')?.split(',').next()?;
    Some((old_start.parse().ok()?, new_start.parse().ok()?))
}

fn is_binary(section: &str) -> bool {
    section
        .lines()
        .any(|line| line.starts_with("Binary files ") || line.starts_with("GIT binary patch"))
}

fn pair_for_split(lines: &[DiffLine]) -> Vec<SplitRow> {
    fn flush(rows: &mut Vec<SplitRow>, removed: &mut Vec<usize>, added: &mut Vec<usize>) {
        for index in 0..removed.len().max(added.len()) {
            rows.push(SplitRow::Pair {
                left: removed.get(index).copied(),
                right: added.get(index).copied(),
            });
        }
        removed.clear();
        added.clear();
    }

    let mut rows = Vec::new();
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        match line.kind {
            DiffLineKind::Removed => {
                if !added.is_empty() {
                    flush(&mut rows, &mut removed, &mut added);
                }
                removed.push(index);
            }
            DiffLineKind::Added => added.push(index),
            DiffLineKind::Context => {
                flush(&mut rows, &mut removed, &mut added);
                rows.push(SplitRow::Pair {
                    left: Some(index),
                    right: Some(index),
                });
            }
            DiffLineKind::Hunk => {
                flush(&mut rows, &mut removed, &mut added);
                rows.push(SplitRow::Hunk(index));
            }
        }
    }
    flush(&mut rows, &mut removed, &mut added);
    rows
}

/// The line a comment on `line` sits under. Hosts number comments on the new side unless the
/// line only exists on the old one.
fn anchor_line(lines: &[DiffLine], line: u64) -> Option<usize> {
    lines
        .iter()
        .position(|diff_line| {
            diff_line.kind != DiffLineKind::Removed && diff_line.new_line == Some(line)
        })
        .or_else(|| {
            lines.iter().position(|diff_line| {
                diff_line.kind == DiffLineKind::Removed && diff_line.old_line == Some(line)
            })
        })
}

/// The comments on one line of one file, oldest first.
struct Thread {
    path: String,
    line: Option<u64>,
    comments: Vec<Comment>,
}

impl Thread {
    fn root_id(&self) -> Option<u64> {
        self.comments.first().map(|comment| comment.id)
    }
}

fn group_threads(comments: &[Comment]) -> Vec<Thread> {
    let mut threads: Vec<Thread> = Vec::new();
    for comment in comments {
        let Some((path, line)) = &comment.inline else {
            continue;
        };
        match threads
            .iter_mut()
            .find(|thread| &thread.path == path && thread.line == *line)
        {
            Some(thread) => thread.comments.push(comment.clone()),
            None => threads.push(Thread {
                path: path.clone(),
                line: *line,
                comments: vec![comment.clone()],
            }),
        }
    }
    for thread in &mut threads {
        thread.comments.sort_by_key(|comment| comment.created_at);
    }
    threads
}

struct FileDiff {
    file: ChangedFile,
    lines: Vec<DiffLine>,
    split_rows: Vec<SplitRow>,
    binary: bool,
    /// Each thread on the file with the line it sits under; `None` when the line isn't in the
    /// diff anymore, which shows it under the file's header.
    threads: Vec<(Option<usize>, usize)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffMode {
    Unified,
    Split,
}

pub(crate) struct PullRequestChangesView {
    focus_handle: FocusHandle,
    client: host::Client,
    repository: RemoteRepository,
    pull_request: PullRequest,
    detail: Option<PullRequestDetail>,
    diff: Option<String>,
    error: Option<SharedString>,
    files: Vec<FileDiff>,
    threads: Vec<Thread>,
    collapsed: HashSet<String>,
    viewed: HashSet<String>,
    mode: DiffMode,
    reply_inputs: HashMap<u64, Entity<InputField>>,
    replying: HashSet<u64>,
    resolving: HashSet<u64>,
    resolved: HashSet<u64>,
    reply_errors: HashMap<u64, SharedString>,
    list_state: ListState,
    load_task: Option<Task<()>>,
}

impl EventEmitter<ItemEvent> for PullRequestChangesView {}

impl Focusable for PullRequestChangesView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Opens the changes of `pull_request` in a tab, or brings back the one already open.
pub(crate) fn open_pull_request_changes(
    workspace: &mut Workspace,
    pull_request: PullRequest,
    detail: Option<PullRequestDetail>,
    client: host::Client,
    repository: RemoteRepository,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<PullRequestChangesView>(cx)
        .find(|view| {
            let view = view.read(cx);
            view.repository == repository && view.pull_request.number == pull_request.number
        });
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let view = cx.new(|cx| {
        PullRequestChangesView::new(pull_request, detail, client, repository, window, cx)
    });
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

impl PullRequestChangesView {
    fn new(
        pull_request: PullRequest,
        detail: Option<PullRequestDetail>,
        client: host::Client,
        repository: RemoteRepository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let viewed: HashSet<String> = KeyValueStore::global(cx)
            .read_kvp(&viewed_key(&repository, pull_request.number))
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).log_err())
            .unwrap_or_default()
            .into_iter()
            .collect();
        let fetch_detail = detail.is_none();
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            client,
            repository,
            pull_request,
            detail,
            diff: None,
            error: None,
            files: Vec::new(),
            threads: Vec::new(),
            collapsed: viewed.clone(),
            viewed,
            mode: DiffMode::Unified,
            reply_inputs: HashMap::new(),
            replying: HashSet::new(),
            resolving: HashSet::new(),
            resolved: HashSet::new(),
            reply_errors: HashMap::new(),
            list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
            load_task: None,
        };
        this.load(fetch_detail, true, window, cx);
        this
    }

    fn load(
        &mut self,
        fetch_detail: bool,
        fetch_diff: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let repository = self.repository.clone();
        let number = self.pull_request.number;
        let load = cx.background_spawn(async move {
            let detail = async {
                if fetch_detail {
                    client
                        .get_pull_request_detail(&repository, number)
                        .await
                        .map(Some)
                } else {
                    Ok(None)
                }
            };
            let diff = async {
                if fetch_diff {
                    client
                        .get_pull_request_diff(&repository, number)
                        .await
                        .map(Some)
                } else {
                    Ok(None)
                }
            };
            futures::join!(detail, diff)
        });
        self.load_task = Some(cx.spawn_in(window, async move |this, cx| {
            let (detail, diff) = load.await;
            this.update_in(cx, |this, window, cx| {
                this.load_task = None;
                match (detail, diff) {
                    (Ok(detail), Ok(diff)) => {
                        this.error = None;
                        if let Some(detail) = detail {
                            this.pull_request = detail.pull_request.clone();
                            this.detail = Some(detail);
                        }
                        if let Some(diff) = diff {
                            this.diff = Some(diff);
                        }
                        this.rebuild(window, cx);
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        // A failed refresh (after a reply) must not hide a diff that is still valid.
                        if this.files.is_empty() {
                            this.error = Some(format!("{error:#}").into());
                        } else {
                            log::warn!("Repo: falha ao atualizar as mudanças do PR: {error:#}");
                        }
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn rebuild(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(detail), Some(diff)) = (&self.detail, &self.diff) else {
            return;
        };
        let threads = group_threads(&detail.comments);
        let files: Vec<FileDiff> = detail
            .files
            .iter()
            .map(|file| {
                let section = file_section(diff, &file.path).unwrap_or_default();
                let lines = parse_file_diff(section);
                let split_rows = pair_for_split(&lines);
                let file_threads = threads
                    .iter()
                    .enumerate()
                    .filter(|(_, thread)| thread.path == file.path)
                    .map(|(index, thread)| {
                        let anchor = thread.line.and_then(|line| anchor_line(&lines, line));
                        (anchor, index)
                    })
                    .collect();
                FileDiff {
                    file: file.clone(),
                    binary: lines.is_empty() && is_binary(section),
                    lines,
                    split_rows,
                    threads: file_threads,
                }
            })
            .collect();

        for thread in &threads {
            let Some(root_id) = thread.root_id() else {
                continue;
            };
            if thread.line.is_some() && !self.reply_inputs.contains_key(&root_id) {
                let input = cx.new(|cx| {
                    InputField::new(window, cx, &t!("repo_hosting.changes.reply_placeholder"))
                });
                self.reply_inputs.insert(root_id, input);
            }
        }

        let previous_count = self.files.len();
        self.threads = threads;
        self.files = files;
        if previous_count == self.files.len() {
            self.list_state.remeasure_items(0..self.files.len());
        } else {
            self.list_state.reset(self.files.len());
        }
    }

    fn file_index(&self, path: &str) -> Option<usize> {
        self.files.iter().position(|file| file.file.path == path)
    }

    fn remeasure_file(&self, path: &str) {
        if let Some(index) = self.file_index(path) {
            self.list_state.remeasure_items(index..index + 1);
        }
    }

    fn toggle_collapsed(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_string());
        }
        self.remeasure_file(path);
        cx.notify();
    }

    fn toggle_viewed(&mut self, path: &str, cx: &mut Context<Self>) {
        // A file marked as viewed folds away, as on GitHub; unmarking it opens it again.
        if self.viewed.remove(path) {
            self.collapsed.remove(path);
        } else {
            self.viewed.insert(path.to_string());
            self.collapsed.insert(path.to_string());
        }
        self.remeasure_file(path);

        let mut paths: Vec<String> = self.viewed.iter().cloned().collect();
        paths.sort();
        let key = viewed_key(&self.repository, self.pull_request.number);
        let store = KeyValueStore::global(cx);
        match serde_json::to_string(&paths) {
            Ok(json) => cx
                .background_spawn(async move { store.write_kvp(key, json).await })
                .detach_and_log_err(cx),
            Err(error) => log::error!("Repo: arquivos vistos não salvaram: {error}"),
        }
        cx.notify();
    }

    fn set_mode(&mut self, mode: DiffMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.list_state.remeasure();
            cx.notify();
        }
    }

    fn send_reply(&mut self, thread_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.threads.get(thread_index) else {
            return;
        };
        let (Some(root_id), Some(line)) = (thread.root_id(), thread.line) else {
            return;
        };
        let Some(input) = self.reply_inputs.get(&root_id) else {
            return;
        };
        let text = input.read(cx).text(cx).trim().to_string();
        if text.is_empty() || !self.replying.insert(root_id) {
            return;
        }
        self.reply_errors.remove(&root_id);
        let path = thread.path.clone();
        let client = self.client.clone();
        let repository = self.repository.clone();
        let number = self.pull_request.number;
        let post = cx.background_spawn(async move {
            client
                .post_inline_comment(&repository, number, &path, line, &text)
                .await
        });
        let thread_path = thread.path.clone();
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = post.await;
            this.update_in(cx, |this, window, cx| {
                this.replying.remove(&root_id);
                match result {
                    Ok(()) => {
                        if let Some(input) = this.reply_inputs.get(&root_id) {
                            input.update(cx, |input, cx| input.clear(window, cx));
                        }
                        this.load(true, false, window, cx);
                    }
                    Err(error) => {
                        this.reply_errors.insert(
                            root_id,
                            t!(
                                "repo_hosting.changes.reply_failed",
                                error = format!("{error:#}")
                            ),
                        );
                        this.remeasure_file(&thread_path);
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn resolve_thread(&mut self, root_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread_path) = self
            .threads
            .iter()
            .find(|thread| thread.root_id() == Some(root_id))
            .map(|thread| thread.path.clone())
        else {
            return;
        };
        if self.resolved.contains(&root_id) || !self.resolving.insert(root_id) {
            return;
        }
        self.reply_errors.remove(&root_id);
        let client = self.client.clone();
        let repository = self.repository.clone();
        let number = self.pull_request.number;
        let resolve = cx.background_spawn(async move {
            client
                .resolve_comment_thread(&repository, number, root_id)
                .await
        });
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = resolve.await;
            this.update(cx, |this, cx| {
                this.resolving.remove(&root_id);
                match result {
                    Ok(()) => {
                        this.resolved.insert(root_id);
                    }
                    Err(error) => {
                        this.reply_errors.insert(
                            root_id,
                            t!(
                                "repo_hosting.changes.resolve_failed",
                                error = format!("{error:#}")
                            ),
                        );
                    }
                }
                this.remeasure_file(&thread_path);
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let added: u64 = self.files.iter().map(|file| file.file.lines_added).sum();
        let removed: u64 = self.files.iter().map(|file| file.file.lines_removed).sum();
        let file_count = self.files.len();
        let files = if file_count == 1 {
            t!("repo_hosting.changes.files.one")
        } else {
            t!("repo_hosting.changes.files.other", count = file_count)
        };
        let commit_count = self
            .detail
            .as_ref()
            .map_or(0, |detail| detail.commits.len());
        let commits = if commit_count == 1 {
            t!("repo_hosting.changes.commits.one")
        } else {
            t!("repo_hosting.changes.commits.other", count = commit_count)
        };
        let viewed_count = self
            .files
            .iter()
            .filter(|file| self.viewed.contains(&file.file.path))
            .count();
        let separator = || Label::new("·").size(LabelSize::Small).color(Color::Muted);

        toolbar_rail(cx)
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(Label::new(files).size(LabelSize::Small))
                    .child(
                        Label::new(format!("+{added}"))
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Success),
                    )
                    .child(
                        Label::new(format!("−{removed}"))
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Error),
                    )
                    .child(separator())
                    .child(
                        Label::new(commits)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(separator())
                    .child(
                        Label::new(t!(
                            "repo_hosting.changes.since",
                            branch = self.pull_request.destination_branch
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_3()
                    .child(
                        segmented_control(cx)
                            .child(
                                segment_option(
                                    "repo-changes-unified",
                                    t!("repo_hosting.changes.unified"),
                                    self.mode == DiffMode::Unified,
                                    true,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| this.set_mode(DiffMode::Unified, cx),
                                )),
                            )
                            .child(
                                segment_option(
                                    "repo-changes-split",
                                    t!("repo_hosting.changes.split"),
                                    self.mode == DiffMode::Split,
                                    true,
                                    cx,
                                )
                                .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.set_mode(DiffMode::Split, cx)
                                    }),
                                ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Icon::new(IconName::Check)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(t!(
                                    "repo_hosting.changes.viewed_count",
                                    viewed = viewed_count,
                                    total = file_count
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    ),
            )
    }

    fn render_file(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(file) = self.files.get(index) else {
            return div().into_any_element();
        };
        let path = file.file.path.clone();
        let collapsed = self.collapsed.contains(&path);
        let mut content = card(cx)
            .overflow_hidden()
            .child(self.render_file_header(index, file, collapsed, cx));
        if !collapsed {
            for (anchor, thread_index) in &file.threads {
                if anchor.is_none() {
                    content = content.child(self.render_thread(*thread_index, cx));
                }
            }
            if file.binary {
                content = content.child(
                    h_flex().px_3().py_2().child(
                        Label::new(t!("repo_hosting.changes.binary_file"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                );
            }
            let threads_after = |line_indices: &[usize]| -> Vec<usize> {
                file.threads
                    .iter()
                    .filter(|(anchor, _)| {
                        anchor.is_some_and(|anchor| line_indices.contains(&anchor))
                    })
                    .map(|(_, thread_index)| *thread_index)
                    .collect()
            };
            match self.mode {
                DiffMode::Unified => {
                    for (line_index, line) in file.lines.iter().enumerate() {
                        content = content.child(render_line(line, cx));
                        for thread_index in threads_after(&[line_index]) {
                            content = content.child(self.render_thread(thread_index, cx));
                        }
                    }
                }
                DiffMode::Split => {
                    for row in &file.split_rows {
                        match *row {
                            SplitRow::Hunk(line_index) => {
                                if let Some(line) = file.lines.get(line_index) {
                                    content = content.child(render_line(line, cx));
                                }
                            }
                            SplitRow::Pair { left, right } => {
                                let left_line = left.and_then(|ix| file.lines.get(ix));
                                let right_line = right.and_then(|ix| file.lines.get(ix));
                                content = content.child(
                                    h_flex()
                                        .w_full()
                                        .child(render_half(left_line, true, cx))
                                        .child(render_half(right_line, false, cx)),
                                );
                                let indices: Vec<usize> = left.into_iter().chain(right).collect();
                                for thread_index in threads_after(&indices) {
                                    content = content.child(self.render_thread(thread_index, cx));
                                }
                            }
                        }
                    }
                }
            }
        }
        div()
            .px_3()
            .pb_2()
            .when(index == 0, |this| this.pt_2())
            .child(content)
            .into_any_element()
    }

    fn render_file_header(
        &self,
        index: usize,
        file: &FileDiff,
        collapsed: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let path = file.file.path.clone();
        let viewed = self.viewed.contains(&path);
        let (directory, name) = match path.rsplit_once('/') {
            Some((directory, name)) => (Some(directory.to_string()), name.to_string()),
            None => (None, path.clone()),
        };
        let icon = FileIcons::get_icon(Path::new(&path), cx)
            .map(Icon::from_path)
            .unwrap_or_else(|| Icon::new(IconName::File));
        let collapse_path = path.clone();
        let disclosure_path = path.clone();
        let viewed_path = path;
        h_flex()
            .id(("repo-changes-file", index))
            .w_full()
            .gap_2()
            .px_2()
            .py_1p5()
            .cursor_pointer()
            .when(!collapsed, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_collapsed(&collapse_path, cx)))
            .child(
                Disclosure::new(("repo-changes-disclosure", index), !collapsed).on_click(
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_collapsed(&disclosure_path, cx)
                    }),
                ),
            )
            .child(icon.size(IconSize::Small).color(Color::Muted))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        Label::new(name)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .when_some(directory, |this, directory| {
                        this.child(
                            Label::new(directory)
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Placeholder)
                                .truncate(),
                        )
                    })
                    .when(file.file.change == FileChange::Added, |this| {
                        this.child(status_pill(
                            t!("repo_hosting.changes.new_file"),
                            StatusTone::Success,
                            cx,
                        ))
                    }),
            )
            .child(
                Label::new(format!("+{}", file.file.lines_added))
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Success),
            )
            .child(
                Label::new(format!("−{}", file.file.lines_removed))
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Error),
            )
            .child(
                h_flex()
                    .id(("repo-changes-viewed", index))
                    .gap_1()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_viewed(&viewed_path, cx)
                    }))
                    .child(checkbox_indicator(viewed, cx))
                    .child(
                        Label::new(t!("repo_hosting.changes.viewed"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
    }

    fn render_thread(&self, thread_index: usize, cx: &Context<Self>) -> AnyElement {
        let Some(thread) = self.threads.get(thread_index) else {
            return div().into_any_element();
        };
        let Some(root_id) = thread.root_id() else {
            return div().into_any_element();
        };
        let now = Utc::now();
        let mut comments = v_flex().gap_2();
        for (position, comment) in thread.comments.iter().enumerate() {
            let mut meta = Vec::new();
            if position == 0
                && let Some(line) = thread.line
            {
                meta.push(t!("repo_hosting.changes.line", line = line).to_string());
            }
            if let Some(created_at) = comment.created_at {
                meta.push(relative_age(created_at, now));
            }
            comments = comments.child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(avatar(&comment.author.display_name, Color::Info, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .child(
                                        Label::new(comment.author.short_name().to_string())
                                            .size(LabelSize::Small)
                                            .weight(FontWeight::SEMIBOLD),
                                    )
                                    .child(
                                        Label::new(meta.join(" · "))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                    .when(position == 0, |this| {
                                        this.child(self.render_resolve(root_id, cx))
                                    }),
                            )
                            .child(
                                Label::new(comment.body.trim().to_string()).size(LabelSize::Small),
                            ),
                    ),
            );
        }

        let mention = mention_action(
            &self.pull_request,
            Some(root_id),
            Some(thread.path.clone()),
            None,
        );
        let replying = self.replying.contains(&root_id);
        let reply_input = self.reply_inputs.get(&root_id).cloned();
        let reply_row = h_flex()
            .gap_2()
            .on_action(cx.listener(move |this, _: &menu::Confirm, window, cx| {
                this.send_reply(thread_index, window, cx)
            }))
            .when_some(reply_input, |this, input| {
                this.child(div().flex_1().min_w_0().child(input))
            })
            .when(replying, |this| {
                this.child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                status_chip(
                    Some(IconName::ZedAssistant),
                    t!("repo_hosting.changes.ask_agent"),
                    Color::Accent,
                    cx,
                )
                .id(("repo-changes-ask-agent", root_id))
                .flex_none()
                .cursor_pointer()
                .tooltip(Tooltip::text(t!("repo_hosting.changes.ask_agent_tooltip")))
                .on_click(move |_, window, cx| window.dispatch_action(mention.boxed_clone(), cx)),
            );

        div()
            .px_2()
            .py_1()
            .child(
                card(cx)
                    .p_2()
                    .gap_2()
                    .child(comments)
                    .child(reply_row)
                    .when_some(self.reply_errors.get(&root_id), |this, error| {
                        this.child(
                            Label::new(error.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_resolve(&self, root_id: u64, cx: &Context<Self>) -> AnyElement {
        if self.resolved.contains(&root_id) {
            return Label::new(t!("repo_hosting.changes.resolved"))
                .size(LabelSize::XSmall)
                .color(Color::Success)
                .into_any_element();
        }
        Button::new(
            ("repo-changes-resolve", root_id),
            t!("repo_hosting.changes.resolve"),
        )
        .style(ButtonStyle::Transparent)
        .label_size(LabelSize::XSmall)
        .color(Color::Accent)
        .disabled(self.resolving.contains(&root_id))
        .on_click(cx.listener(move |this, _, window, cx| this.resolve_thread(root_id, window, cx)))
        .into_any_element()
    }
}

fn line_colors(kind: DiffLineKind, cx: &App) -> (Option<Hsla>, &'static str) {
    let colors = cx.theme().colors();
    match kind {
        DiffLineKind::Added => (Some(colors.version_control_added), "+"),
        DiffLineKind::Removed => (Some(colors.version_control_deleted), "−"),
        DiffLineKind::Context | DiffLineKind::Hunk => (None, ""),
    }
}

fn line_text(text: &str) -> String {
    text.replace('\t', "    ")
}

fn render_line(line: &DiffLine, cx: &App) -> AnyElement {
    if line.kind == DiffLineKind::Hunk {
        return h_flex()
            .h(px(LINE_HEIGHT))
            .w_full()
            .px_2()
            .bg(cx.theme().colors().ghost_element_hover)
            .child(
                Label::new(line.text.clone())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted)
                    .single_line()
                    .truncate(),
            )
            .into_any_element();
    }
    let number = line.new_line.or(line.old_line);
    line_shell(line.kind, number, &line.text, cx).into_any_element()
}

fn render_half(line: Option<&DiffLine>, is_left: bool, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    let half = div().flex_1().min_w_0().when(is_left, |this| {
        this.border_r_1().border_color(colors.border)
    });
    match line {
        Some(line) => {
            let number = if is_left {
                line.old_line
            } else {
                line.new_line
            };
            half.child(line_shell(line.kind, number, &line.text, cx))
        }
        None => half.h(px(LINE_HEIGHT)).bg(colors.ghost_element_hover),
    }
}

fn line_shell(kind: DiffLineKind, number: Option<u64>, text: &str, cx: &App) -> Div {
    let (color, sign) = line_colors(kind, cx);
    h_flex()
        .h(px(LINE_HEIGHT))
        .w_full()
        .border_l_2()
        .border_color(color.unwrap_or(gpui::transparent_black()))
        .when_some(color, |this, color| this.bg(color.opacity(0.1)))
        .child(
            h_flex().flex_none().w(px(44.)).justify_end().pr_2().child(
                Label::new(number.map(|number| number.to_string()).unwrap_or_default())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted),
            ),
        )
        .child(
            div().flex_none().w(px(14.)).child(
                Label::new(sign)
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted),
            ),
        )
        .child(
            Label::new(line_text(text))
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .single_line()
                .truncate(),
        )
}

fn viewed_key(repository: &RemoteRepository, number: u64) -> String {
    format!(
        "repo_hosting.viewed.{}:{}#{number}",
        repository.hosting.name(),
        repository.full_name()
    )
}

impl Render for PullRequestChangesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if let Some(error) = &self.error {
            EmptyState::new(IconName::Warning, t!("repo_hosting.changes.load_failed"))
                .tone(EmptyStateTone::Error)
                .detail(error.clone())
                .into_any_element()
        } else if self.load_task.is_some() && self.files.is_empty() {
            EmptyState::new(IconName::LoadCircle, t!("repo_hosting.changes.loading"))
                .spinning(true)
                .into_any_element()
        } else {
            list(self.list_state.clone(), cx.processor(Self::render_file))
                .size_full()
                .into_any_element()
        };
        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_bar(cx))
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl Item for PullRequestChangesView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        t!(
            "repo_hosting.changes.tab_title",
            number = self.pull_request.number
        )
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.pull_request.title.clone().into())
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn show_toolbar(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTION: &str = "diff --git a/src/app.rs b/src/app.rs\n\
index 1111111..2222222 100644\n\
--- a/src/app.rs\n\
+++ b/src/app.rs\n\
@@ -10,4 +10,5 @@ fn main() {\n \
let a = 1;\n\
-let b = 2;\n\
+let b = 3;\n\
+let c = 4;\n \
let d = 5;\n\
\\ No newline at end of file\n";

    #[test]
    fn parses_hunks_with_old_and_new_line_numbers() {
        let lines = parse_file_diff(SECTION);
        let kinds: Vec<_> = lines.iter().map(|line| line.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffLineKind::Hunk,
                DiffLineKind::Context,
                DiffLineKind::Removed,
                DiffLineKind::Added,
                DiffLineKind::Added,
                DiffLineKind::Context,
            ]
        );
        assert_eq!(lines[1].old_line, Some(10));
        assert_eq!(lines[1].new_line, Some(10));
        assert_eq!(lines[1].text, "let a = 1;");
        assert_eq!(lines[2].old_line, Some(11));
        assert_eq!(lines[3].new_line, Some(11));
        assert_eq!(lines[4].new_line, Some(12));
        assert_eq!(lines[5].old_line, Some(12));
        assert_eq!(lines[5].new_line, Some(13));
    }

    #[test]
    fn hunk_header_without_counts() {
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1)));
        assert_eq!(parse_hunk_header("@@ -0,0 +1,3 @@"), Some((0, 1)));
        assert_eq!(parse_hunk_header("@@ nonsense"), None);
    }

    #[test]
    fn pairs_removed_and_added_blocks_side_by_side() {
        let lines = parse_file_diff(SECTION);
        assert_eq!(
            pair_for_split(&lines),
            vec![
                SplitRow::Hunk(0),
                SplitRow::Pair {
                    left: Some(1),
                    right: Some(1)
                },
                SplitRow::Pair {
                    left: Some(2),
                    right: Some(3)
                },
                SplitRow::Pair {
                    left: None,
                    right: Some(4)
                },
                SplitRow::Pair {
                    left: Some(5),
                    right: Some(5)
                },
            ]
        );
    }

    #[test]
    fn comments_anchor_to_the_new_side_first() {
        let lines = parse_file_diff(SECTION);
        assert_eq!(anchor_line(&lines, 11), Some(3));
        assert_eq!(anchor_line(&lines, 13), Some(5));
        assert_eq!(anchor_line(&lines, 99), None);
    }

    #[test]
    fn binary_sections_are_detected() {
        assert!(is_binary(
            "diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n"
        ));
        assert!(!is_binary(SECTION));
    }
}
