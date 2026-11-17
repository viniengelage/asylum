//! The model's help inside the tab: ⌘K proposals shown as a diff that only runs once accepted,
//! the fix for a query AWS refused, the session-expired card and the summary of a result.

use std::time::SystemTime;

use editor::Editor;
use gpui::{AnyElement, App, ClipboardItem, Entity, Focusable as _, FontWeight, Task, Window, px};
use i18n::t;
use serde::Deserialize;
use ui::{KeyBinding, prelude::*, primary_button, secondary_button};
use util::ResultExt as _;

use super::{
    HistoryEntry, HistoryKind, QueryView, RunState, Sample, TimeWindow,
    insights::{self, DiffKind, DiffLine},
    results::{self, accent_button, format_bytes, format_cost},
    unix,
};
use crate::{
    AcceptSuggestion, AcceptSuggestionAndRun, Connection, DiscardSuggestion, RunQuery,
    agent_bridge::{self, AgentContext},
    llm,
};

const MAX_SAMPLE_CHARS: usize = 600;
const MAX_SUMMARY_ROWS: usize = 200;
const PREVIEW_MINUTES: u32 = 15;

#[derive(Default)]
pub(crate) struct Assist {
    pub prompt: Option<Entity<Editor>>,
    pub requesting: bool,
    pub error: Option<SharedString>,
    pub proposal: Option<Proposal>,
    pub fix: Option<Fix>,
    pub fixing: bool,
    pub summary: Option<Summary>,
    pub summarizing: bool,
    request_task: Option<Task<()>>,
    fix_task: Option<Task<()>>,
    summary_task: Option<Task<()>>,
}

pub(crate) struct Proposal {
    pub request: String,
    pub query: String,
    pub changes: Vec<String>,
    pub window_minutes: Option<u32>,
    pub diff: Vec<DiffLine>,
    pub diagnostic: Option<insights::Diagnostic>,
}

pub(crate) struct Fix {
    pub title: SharedString,
    pub explanation: SharedString,
    pub line: usize,
    pub old_line: String,
    pub new_line: String,
    pub query: String,
}

pub(crate) struct Summary {
    pub events: usize,
    pub patterns: Vec<Pattern>,
    pub narrative: String,
    pub followups: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct Pattern {
    pub count: u64,
    pub message: String,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
}

#[derive(Deserialize)]
struct ProposalAnswer {
    query: String,
    #[serde(default)]
    changes: Vec<String>,
    #[serde(default)]
    window_minutes: Option<u32>,
}

#[derive(Deserialize)]
struct FixAnswer {
    title: String,
    explanation: String,
    query: String,
}

#[derive(Deserialize)]
struct SummaryAnswer {
    #[serde(default)]
    patterns: Vec<Pattern>,
    #[serde(default)]
    narrative: String,
    #[serde(default)]
    followups: Vec<String>,
}

const PROPOSAL_SYSTEM: &str = "You write CloudWatch Logs Insights queries. Commands: fields, \
filter, stats, sort, limit, parse, dedup, display, unmask, pattern, diff; stages are separated \
by |; comments start with #. Use only fields that exist in the sample. Answer with one JSON \
object and nothing else: {\"query\": the whole new query, \"changes\": short bullets in the \
user's language saying what changed and why, \"window_minutes\": the time window in minutes if \
the request asks for a different one, otherwise null}.";

const FIX_SYSTEM: &str = "You fix CloudWatch Logs Insights queries that AWS rejected. Change \
as little as possible. Answer with one JSON object and nothing else: {\"title\": one short \
sentence naming the mistake, \"explanation\": one or two sentences, \"query\": the whole \
corrected query}. Write title and explanation in Brazilian Portuguese.";

const SUMMARY_SYSTEM: &str = "You summarize log events returned by a CloudWatch Logs Insights \
query. Group them into at most 4 patterns by cause, most frequent first, with a last \"Outros\" \
pattern for the rest when needed. Answer with one JSON object and nothing else: {\"patterns\": \
[{\"count\": n, \"message\": short normalized message, \"location\": \"function · file:line\" \
from the stack trace or null, \"detail\": short note or null, \"since\": \"HH:MM\" of the first \
event or null}], \"narrative\": two sentences on what probably happened, \"followups\": up to 2 \
short requests for follow-up queries}. Write in Brazilian Portuguese.";

impl QueryView {
    pub(super) fn open_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = match &self.assist.prompt {
            Some(prompt) => prompt.clone(),
            None => {
                let prompt = cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_placeholder_text(
                        &t!("cloudwatch_query.ask.placeholder"),
                        window,
                        cx,
                    );
                    editor
                });
                self.assist.prompt = Some(prompt.clone());
                prompt
            }
        };
        window.focus(&prompt.focus_handle(cx), cx);
        cx.notify();
    }

    fn close_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.assist.prompt = None;
        self.assist.request_task = None;
        self.assist.requesting = false;
        self.assist.error = None;
        window.focus(&self.editor.focus_handle(cx), cx);
        cx.notify();
    }

    pub(super) fn submit_request(&mut self, request: String, cx: &mut Context<Self>) {
        let request = request.trim().to_string();
        if request.is_empty() {
            return;
        }
        let current = self.query_text(cx);
        let base = self
            .assist
            .proposal
            .as_ref()
            .map(|proposal| proposal.query.clone());
        let fields = self.fields.borrow().clone();
        let sample = self
            .sample
            .events
            .iter()
            .filter_map(|event| {
                event
                    .iter()
                    .find(|(name, _)| name == "@message")
                    .map(|(_, message)| {
                        llm::mask(&message.chars().take(MAX_SAMPLE_CHARS).collect::<String>())
                    })
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut prompt = format!(
            "Log groups: {}\nTime window: {}\nFields: {}\n\nCurrent query:\n```\n{current}\n```\n",
            self.file.groups.join(", "),
            self.file.window.label(),
            fields.join(", "),
        );
        if let Some(base) = &base {
            prompt.push_str(&format!(
                "\nYour previous proposal, which the user wants refined:\n```\n{base}\n```\n"
            ));
        }
        prompt.push_str(&format!(
            "\nSample events (personal data masked):\n{sample}\n\nRequest: {request}"
        ));
        let answer = llm::complete(PROPOSAL_SYSTEM.to_string(), prompt, cx);
        self.assist.requesting = true;
        self.assist.error = None;
        cx.notify();
        self.assist.request_task = Some(cx.spawn(async move |this, cx| {
            let result = answer
                .await
                .and_then(|answer| llm::parse_json::<ProposalAnswer>(&answer));
            this.update(cx, |this, cx| {
                this.assist.requesting = false;
                match result {
                    Ok(answer) => {
                        let query = answer.query.trim_end().to_string() + "\n";
                        this.assist.proposal = Some(Proposal {
                            diff: insights::line_diff(&current, &query),
                            diagnostic: insights::validate(&query),
                            request,
                            query,
                            changes: answer.changes,
                            window_minutes: answer.window_minutes.filter(|minutes| *minutes > 0),
                        });
                    }
                    Err(error) => {
                        this.assist.error = Some(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    pub(super) fn accept_proposal(
        &mut self,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(proposal) = self.assist.proposal.take() else {
            return;
        };
        self.replace_text(&proposal.query, cx);
        if let Some(minutes) = proposal.window_minutes {
            self.set_window(TimeWindow::Last { minutes }, cx);
        }
        self.push_history(
            HistoryEntry {
                kind: HistoryKind::Agent,
                query: proposal.query,
                summary: proposal.request,
                at: unix(SystemTime::now()),
            },
            cx,
        );
        self.close_prompt(window, cx);
        if run {
            self.run(None, cx);
        }
    }

    pub(super) fn accept_suggestion(
        &mut self,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.assist.proposal.is_some() {
            self.accept_proposal(run, window, cx);
        } else if self.assist.fix.is_some() {
            self.apply_fix(run, window, cx);
        }
    }

    pub(super) fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.assist.proposal.take().is_some() {
            window.focus(&self.editor.focus_handle(cx), cx);
            cx.notify();
        } else if self.assist.prompt.is_some() {
            self.close_prompt(window, cx);
        } else if self.assist.fix.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn apply_fix(&mut self, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(fix) = self.assist.fix.take() else {
            return;
        };
        self.replace_text(&fix.query, cx);
        self.clear_error(cx);
        self.state = RunState::Idle;
        window.focus(&self.editor.focus_handle(cx), cx);
        if run {
            self.run(None, cx);
        }
        cx.notify();
    }

    /// A local rule first (`where` → `filter`); only when none applies does the model answer.
    pub(super) fn suggest_fix(&mut self, text: &str, message: &str, cx: &mut Context<Self>) {
        let line = insights::error_position(message).map(|(line, _)| line);
        if let Some(fix) = line.and_then(|line| insights::local_fix(text, line)) {
            let mut lines: Vec<&str> = text.lines().collect();
            if let Some(slot) = lines.get_mut(fix.line - 1) {
                *slot = &fix.new_line;
            }
            let mut query = lines.join("\n");
            if text.ends_with('\n') {
                query.push('\n');
            }
            let meaning = match fix.rule {
                insights::FixRule::Where => t!("cloudwatch_query.fix.meaning_filter"),
                insights::FixRule::Select => t!("cloudwatch_query.fix.meaning_fields"),
                insights::FixRule::OrderBy => t!("cloudwatch_query.fix.meaning_sort"),
                insights::FixRule::Head => t!("cloudwatch_query.fix.meaning_limit"),
                insights::FixRule::Distinct => t!("cloudwatch_query.fix.meaning_dedup"),
            };
            self.assist.fix = Some(Fix {
                title: t!("cloudwatch_query.fix.no_command", command = fix.symbol),
                explanation: t!("cloudwatch_query.fix.explanation", meaning = meaning),
                line: fix.line,
                old_line: fix.old_line,
                new_line: fix.new_line,
                query,
            });
            return;
        }
        let prompt = format!("Query:\n```\n{text}\n```\nAWS error: {message}");
        let answer = llm::complete(FIX_SYSTEM.to_string(), prompt, cx);
        let original = text.to_string();
        self.assist.fixing = true;
        self.assist.fix_task = Some(cx.spawn(async move |this, cx| {
            let result = answer
                .await
                .and_then(|answer| llm::parse_json::<FixAnswer>(&answer));
            this.update(cx, |this, cx| {
                this.assist.fixing = false;
                match result {
                    Ok(answer) => {
                        let query = answer.query.trim_end().to_string() + "\n";
                        let diff = insights::line_diff(&original, &query);
                        let removed = diff.iter().find(|line| line.kind == DiffKind::Removed);
                        let added = diff.iter().find(|line| line.kind == DiffKind::Added);
                        this.assist.fix = Some(Fix {
                            title: answer.title.into(),
                            explanation: answer.explanation.into(),
                            line: removed.or(added).map_or(1, |line| line.number),
                            old_line: removed.map(|line| line.text.clone()).unwrap_or_default(),
                            new_line: added.map(|line| line.text.clone()).unwrap_or_default(),
                            query,
                        });
                    }
                    Err(error) => log::warn!("CloudWatch query fix failed: {error:#}"),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    pub(super) fn summarize(&mut self, cx: &mut Context<Self>) {
        let Some(results) = &self.results else {
            return;
        };
        if self.assist.summarizing || results.rows.is_empty() {
            return;
        }
        let events = results.rows.len();
        let rows = (0..events.min(MAX_SUMMARY_ROWS))
            .map(|row| {
                let mut line = results
                    .rows
                    .get(row)
                    .into_iter()
                    .flatten()
                    .filter(|(name, _)| name != "@ptr")
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join(" | ");
                line.truncate(line.floor_char_boundary(MAX_SAMPLE_CHARS * 2));
                llm::mask(&line)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "Query:\n```\n{}\n```\n{events} events (times in UTC, personal data masked):\n{rows}",
            self.last_query.clone().unwrap_or_default()
        );
        let answer = llm::complete(SUMMARY_SYSTEM.to_string(), prompt, cx);
        self.assist.summarizing = true;
        cx.notify();
        self.assist.summary_task = Some(cx.spawn(async move |this, cx| {
            let result = answer
                .await
                .and_then(|answer| llm::parse_json::<SummaryAnswer>(&answer));
            this.update(cx, |this, cx| {
                this.assist.summarizing = false;
                match result {
                    Ok(answer) => {
                        this.assist.summary = Some(Summary {
                            events,
                            patterns: answer.patterns,
                            narrative: answer.narrative,
                            followups: answer.followups,
                        });
                        this.tab = super::ResultTab::Events;
                    }
                    Err(error) => {
                        this.state = RunState::Failed(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn agent_query_context(&self, cx: &App) -> AgentContext {
        AgentContext::Query {
            path: Some(self.path.clone()),
            query: self.query_text(cx),
            log_groups: self.file.groups.clone(),
        }
    }

    pub(super) fn render_prompt(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let prompt = self.assist.prompt.clone()?;
        let colors = cx.theme().colors();
        let accent = colors.border_focused;
        let focus = prompt.focus_handle(cx);
        let group = self.file.groups.first().cloned().unwrap_or_default();
        let context = t!(
            "cloudwatch_query.ask.context",
            group = group,
            fields = self.fields.borrow().len(),
            events = self.sample.events.len()
        );
        let has_proposal = self.assist.proposal.is_some();
        let chip = |icon: IconName, label: SharedString| {
            h_flex()
                .h(px(22.))
                .px_2()
                .gap_1p5()
                .rounded(Radius::Sm.rems())
                .border_1()
                .border_color(colors.border_variant)
                .bg(colors.element_background)
                .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .single_line(),
                )
        };
        Some(
            v_flex()
                .key_context("CloudWatchAskPrompt")
                .on_action(cx.listener(|this, _: &menu::Confirm, _, cx| {
                    let Some(prompt) = &this.assist.prompt else {
                        return;
                    };
                    let request = prompt.read(cx).text(cx);
                    this.submit_request(request, cx);
                }))
                .on_action(
                    cx.listener(|this, _: &menu::Cancel, window, cx| this.discard(window, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &editor::actions::Cancel, window, cx| {
                        this.discard(window, cx)
                    }),
                )
                .m_2p5()
                .p_3()
                .gap_2()
                .rounded(Radius::Lg.rems())
                .border_1()
                .border_color(accent)
                .bg(accent.opacity(0.08))
                .child(
                    h_flex()
                        .gap_3()
                        .child(
                            Icon::new(IconName::Sparkle)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                        .child(div().flex_1().min_w_0().child(prompt))
                        .children(
                            llm::default_model_name(cx).map(|name| chip(IconName::Sparkle, name)),
                        ),
                )
                .child(
                    h_flex()
                        .pl_7()
                        .gap_2()
                        .child(chip(IconName::CloudPulse, context)),
                )
                .child(
                    h_flex()
                        .pl_7()
                        .gap_2()
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(match &self.assist.error {
                                    Some(error) => error.clone(),
                                    None if self.assist.requesting => {
                                        t!("cloudwatch_query.ask.thinking")
                                    }
                                    None => t!("cloudwatch_query.ask.note"),
                                })
                                .size(LabelSize::Small)
                                .color(if self.assist.error.is_some() {
                                    Color::Error
                                } else {
                                    Color::Placeholder
                                })
                                .truncate(),
                            ),
                        )
                        .when(has_proposal, |this| {
                            this.child(
                                secondary_button(
                                    "cloudwatch-refine",
                                    Some((IconName::Pencil, Color::Muted)),
                                    t!("cloudwatch_query.ask.refine"),
                                    true,
                                    cx,
                                )
                                .h(px(28.))
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        if let Some(prompt) = &this.assist.prompt {
                                            let request = prompt.read(cx).text(cx);
                                            let unchanged =
                                                this.assist.proposal.as_ref().is_some_and(
                                                    |proposal| {
                                                        proposal.request.trim() == request.trim()
                                                    },
                                                );
                                            if request.trim().is_empty() || unchanged {
                                                window.focus(&prompt.focus_handle(cx), cx);
                                            } else {
                                                this.submit_request(request, cx);
                                            }
                                        }
                                    },
                                )),
                            )
                        })
                        .child(
                            secondary_button(
                                "cloudwatch-discard",
                                None,
                                t!("cloudwatch_query.ask.discard"),
                                true,
                                cx,
                            )
                            .h(px(28.))
                            .child(
                                KeyBinding::for_action_in(&DiscardSuggestion, &focus, cx)
                                    .size(rems_from_px(12_f32)),
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.discard(window, cx))),
                        )
                        .child(if has_proposal {
                            primary_button(
                                "cloudwatch-accept",
                                None,
                                t!("cloudwatch_query.ask.accept"),
                                Some(KeyBinding::for_action_in(&AcceptSuggestion, &focus, cx)),
                                cx,
                            )
                            .h(px(28.))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.accept_proposal(false, window, cx)
                            }))
                            .into_any_element()
                        } else {
                            primary_button(
                                "cloudwatch-send-request",
                                Some(IconName::Sparkle),
                                t!("cloudwatch_query.ask.send"),
                                None,
                                cx,
                            )
                            .h(px(28.))
                            .when(self.assist.requesting, |this| this.opacity(0.6))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(prompt) = &this.assist.prompt {
                                    let request = prompt.read(cx).text(cx);
                                    this.submit_request(request, cx);
                                }
                            }))
                            .into_any_element()
                        }),
                )
                .into_any_element(),
        )
    }

    pub(super) fn render_fix_card(&self, message: &str, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let accent = colors.border_focused;
        let focus = self.editor.focus_handle(cx);
        let raw = h_flex()
            .px_3()
            .py_2()
            .rounded(Radius::Sm.rems())
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.editor_background)
            .child(
                Label::new(format!("MalformedQueryException: {message}"))
                    .size(LabelSize::Small)
                    .buffer_font(cx),
            );
        let card = v_flex()
            .mx_3()
            .p_4()
            .gap_3()
            .rounded(Radius::Lg.rems())
            .border_1()
            .border_color(accent)
            .bg(accent.opacity(0.06))
            .map(|this| match &self.assist.fix {
                Some(fix) => this
                    .child(
                        h_flex()
                            .items_start()
                            .gap_3()
                            .child(
                                Icon::new(IconName::Sparkle)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(Label::new(fix.title.clone()).weight(FontWeight::BOLD))
                                    .child(
                                        Label::new(fix.explanation.clone())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            ),
                    )
                    .child(div().pl_7().child(raw))
                    .child(div().pl_7().child(render_diff(
                        &[
                            DiffLine {
                                kind: DiffKind::Removed,
                                number: fix.line,
                                text: fix.old_line.clone(),
                            },
                            DiffLine {
                                kind: DiffKind::Added,
                                number: fix.line,
                                text: fix.new_line.clone(),
                            },
                        ],
                        cx,
                    )))
                    .child(
                        h_flex()
                            .pl_7()
                            .gap_2()
                            .child(
                                primary_button(
                                    "cloudwatch-apply-fix",
                                    None,
                                    t!("cloudwatch_query.fix.apply"),
                                    Some(KeyBinding::for_action_in(&AcceptSuggestion, &focus, cx)),
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.apply_fix(false, window, cx),
                                )),
                            )
                            .child(
                                secondary_button(
                                    "cloudwatch-apply-fix-run",
                                    Some((IconName::PlayFilled, Color::Default)),
                                    t!("cloudwatch_query.fix.apply_and_run"),
                                    true,
                                    cx,
                                )
                                .child(
                                    KeyBinding::for_action_in(&AcceptSuggestionAndRun, &focus, cx)
                                        .size(rems_from_px(12_f32)),
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.apply_fix(true, window, cx),
                                )),
                            )
                            .child(self.explain_button(message, cx)),
                    )
                    .child(
                        div().pl_7().child(
                            Label::new(t!("cloudwatch_query.fix.note"))
                                .size(LabelSize::Small)
                                .color(Color::Placeholder),
                        ),
                    ),
                None => this
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                Icon::new(IconName::XCircle)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            )
                            .child(
                                Label::new(if self.assist.fixing {
                                    t!("cloudwatch_query.fix.thinking")
                                } else {
                                    t!("cloudwatch_query.fix.no_fix")
                                })
                                .weight(FontWeight::BOLD),
                            ),
                    )
                    .child(div().pl_7().child(raw))
                    .child(h_flex().pl_7().child(self.explain_button(message, cx))),
            });
        v_flex()
            .id("cloudwatch-fix")
            .size_full()
            .overflow_y_scroll()
            .gap_3()
            .pb_3()
            .child(card)
            .child(self.render_quick_reference(cx))
            .into_any_element()
    }

    fn explain_button(&self, message: &str, cx: &mut Context<Self>) -> AnyElement {
        let message = message.to_string();
        secondary_button(
            "cloudwatch-explain-syntax",
            Some((IconName::Book, Color::Muted)),
            t!("cloudwatch_query.fix.explain"),
            true,
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            let context = this.agent_query_context(cx);
            agent_bridge::send_to_agent(
                context,
                this.file_name().into(),
                Some(t!("cloudwatch_query.fix.explain_prompt", error = message).to_string()),
                true,
                window,
                cx,
            );
        }))
        .into_any_element()
    }

    fn render_quick_reference(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let rows: [(&str, SharedString, &str); 6] = [
            (
                "fields",
                t!("cloudwatch_query.reference.fields"),
                "fields @timestamp, msg",
            ),
            (
                "filter",
                t!("cloudwatch_query.reference.filter"),
                "filter status >= 500",
            ),
            (
                "stats",
                t!("cloudwatch_query.reference.stats"),
                "stats count(*) by bin(5m)",
            ),
            (
                "parse",
                t!("cloudwatch_query.reference.parse"),
                "parse msg \"user=* \" as user",
            ),
            (
                "sort · limit",
                t!("cloudwatch_query.reference.sort_limit"),
                "sort @timestamp desc | limit 50",
            ),
            (
                "dedup",
                t!("cloudwatch_query.reference.dedup"),
                "dedup requestId",
            ),
        ];
        v_flex()
            .mx_3()
            .gap_2()
            .child(
                Label::new(t!("cloudwatch_query.reference.title"))
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::BOLD)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .rounded(Radius::Md.rems())
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_background)
                    .children(rows.into_iter().enumerate().map(
                        |(index, (command, description, example))| {
                            h_flex()
                                .h(px(36.))
                                .px_3()
                                .gap_3()
                                .when(index > 0, |this| {
                                    this.border_t_1().border_color(colors.border_variant)
                                })
                                .child(
                                    div().w(px(110.)).flex_none().child(
                                        Label::new(command)
                                            .size(LabelSize::Small)
                                            .weight(FontWeight::SEMIBOLD)
                                            .buffer_font(cx)
                                            .color(Color::Accent),
                                    ),
                                )
                                .child(
                                    div().w(px(200.)).flex_none().child(
                                        Label::new(description)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                                )
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Label::new(example)
                                            .size(LabelSize::Small)
                                            .buffer_font(cx)
                                            .truncate(),
                                    ),
                                )
                                .child(
                                    Button::new(
                                        ("cloudwatch-use-example", index),
                                        t!("cloudwatch_query.reference.use"),
                                    )
                                    .style(ButtonStyle::Transparent)
                                    .label_size(LabelSize::Small)
                                    .color(Color::Accent)
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            let text = this.query_text(cx);
                                            let trimmed = text.trim_end();
                                            let new_text = if trimmed.is_empty() {
                                                format!("{example}\n")
                                            } else {
                                                format!("{trimmed}\n| {example}\n")
                                            };
                                            this.replace_text(&new_text, cx);
                                            window.focus(&this.editor.focus_handle(cx), cx);
                                        },
                                    )),
                                )
                        },
                    )),
            )
            .into_any_element()
    }

    pub(super) fn render_session_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(store) = self.store.clone() else {
            return gpui::Empty.into_any_element();
        };
        let error = Color::Error.color(cx);
        let colors = cx.theme().colors();
        let (store_profile, region) = {
            let store = store.read(cx);
            (store.profile_name(), store.region().unwrap_or_default())
        };
        let (since, message) = match store.read(cx).connection() {
            Connection::SessionExpired { since, message, .. } => (Some(*since), message.clone()),
            _ => (None, String::new()),
        };
        let command = crate::aws_login::login_command(&store_profile);
        let title = match since {
            Some(since) => t!(
                "cloudwatch_query.session.title",
                ago = results::ago_label(since)
            ),
            None => t!("cloudwatch_query.session.title_now"),
        };
        v_flex()
            .id("cloudwatch-session")
            .size_full()
            .overflow_y_scroll()
            .child(
                h_flex()
                    .mx_3()
                    .p_5()
                    .gap_4()
                    .items_start()
                    .rounded(Radius::Lg.rems())
                    .border_1()
                    .border_color(error.opacity(0.3))
                    .bg(error.opacity(0.05))
                    .child(
                        div()
                            .size(px(36.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(Radius::Md.rems())
                            .bg(error.opacity(0.14))
                            .child(
                                Icon::new(IconName::Lock)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_3()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        Label::new(title)
                                            .size(LabelSize::Large)
                                            .weight(FontWeight::BOLD),
                                    )
                                    .child(
                                        Label::new(t!(
                                            "cloudwatch_query.session.explanation",
                                            profile = store_profile
                                        ))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .px_3()
                                    .py_2()
                                    .rounded(Radius::Sm.rems())
                                    .border_1()
                                    .border_color(colors.border_variant)
                                    .bg(colors.editor_background)
                                    .child(
                                        Label::new(message).size(LabelSize::Small).buffer_font(cx),
                                    )
                                    .child(
                                        Label::new(t!(
                                            "cloudwatch_query.session.source",
                                            profile = store_profile,
                                            region = region
                                        ))
                                        .size(LabelSize::Small)
                                        .buffer_font(cx)
                                        .color(Color::Placeholder),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        primary_button(
                                            "cloudwatch-aws-login",
                                            Some(IconName::Terminal),
                                            t!("cloudwatch_query.session.run_login"),
                                            None,
                                            cx,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                let (Some(workspace), Some(store)) =
                                                    (this.workspace.clone(), this.store.clone())
                                                else {
                                                    return;
                                                };
                                                workspace
                                                    .update(cx, |workspace, cx| {
                                                        crate::aws_login::run_aws_login(
                                                            workspace, store, window, cx,
                                                        )
                                                    })
                                                    .log_err();
                                            }),
                                        ),
                                    )
                                    .child(
                                        secondary_button(
                                            "cloudwatch-other-profile",
                                            Some((IconName::Person, Color::Muted)),
                                            t!("cloudwatch_query.session.other_profile"),
                                            true,
                                            cx,
                                        )
                                        .on_click(
                                            |_, window, cx| {
                                                window.dispatch_action(
                                                    Box::new(crate::ToggleFocus),
                                                    cx,
                                                )
                                            },
                                        ),
                                    )
                                    .child(
                                        secondary_button(
                                            "cloudwatch-copy-login",
                                            Some((IconName::Copy, Color::Muted)),
                                            t!("cloudwatch_query.session.copy_command"),
                                            true,
                                            cx,
                                        )
                                        .on_click({
                                            let command = command.clone();
                                            move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    command.clone(),
                                                ))
                                            }
                                        }),
                                    ),
                            )
                            .child(
                                Label::new(t!("cloudwatch_query.session.note", command = command))
                                    .size(LabelSize::Small)
                                    .color(Color::Placeholder),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_proposal_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(proposal) = &self.assist.proposal else {
            return gpui::Empty.into_any_element();
        };
        let colors = cx.theme().colors();
        let focus = self.editor.focus_handle(cx);
        let minutes = proposal
            .window_minutes
            .unwrap_or_else(|| (self.file.window.seconds() / 60) as u32);
        let group = self.file.groups.first().cloned().unwrap_or_default();
        let estimate = match &self.state {
            RunState::Done {
                bytes_scanned,
                from,
                to,
                ..
            } => {
                let base_seconds = to
                    .duration_since(*from)
                    .unwrap_or_default()
                    .as_secs()
                    .max(1);
                let estimated = bytes_scanned * (f64::from(minutes) * 60.0) / base_seconds as f64;
                t!(
                    "cloudwatch_query.proposal.estimate",
                    minutes = minutes,
                    group = group,
                    bytes = format_bytes(estimated),
                    cost = format_cost(estimated),
                    base = super::minutes_label((base_seconds / 60).max(1) as u32)
                )
            }
            _ => t!(
                "cloudwatch_query.proposal.no_estimate",
                minutes = minutes,
                group = group
            ),
        };
        let others: [SharedString; 3] = [
            t!("cloudwatch_query.proposal.other_per_minute"),
            t!("cloudwatch_query.proposal.other_first_error"),
            t!("cloudwatch_query.proposal.other_yesterday"),
        ];
        let preview_query = proposal.query.clone();
        v_flex()
            .id("cloudwatch-proposal")
            .size_full()
            .overflow_y_scroll()
            .px_3()
            .gap_4()
            .child(
                v_flex()
                    .p_4()
                    .gap_3()
                    .rounded(Radius::Lg.rems())
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.element_background)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                Icon::new(IconName::Sparkle)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(t!("cloudwatch_query.proposal.changes"))
                                    .weight(FontWeight::BOLD),
                            ),
                    )
                    .children(proposal.changes.iter().map(|change| {
                        h_flex()
                            .pl_7()
                            .gap_2()
                            .items_start()
                            .child(
                                div()
                                    .mt(px(7.))
                                    .size(px(4.))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(colors.text_muted),
                            )
                            .child(
                                Label::new(change.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                    }))
                    .child(div().h_px().bg(colors.border_variant))
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        Label::new(t!("cloudwatch_query.proposal.before_running"))
                                            .size(LabelSize::XSmall)
                                            .weight(FontWeight::BOLD)
                                            .color(Color::Muted),
                                    )
                                    .child(Label::new(estimate).size(LabelSize::Small)),
                            )
                            .child(
                                secondary_button(
                                    "cloudwatch-preview",
                                    Some((IconName::Clock, Color::Muted)),
                                    t!(
                                        "cloudwatch_query.proposal.preview",
                                        minutes = PREVIEW_MINUTES
                                    ),
                                    true,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.run(
                                            Some((
                                                preview_query.clone(),
                                                TimeWindow::Last {
                                                    minutes: PREVIEW_MINUTES,
                                                },
                                            )),
                                            cx,
                                        )
                                    },
                                )),
                            )
                            .child(
                                primary_button(
                                    "cloudwatch-accept-run",
                                    Some(IconName::PlayFilled),
                                    t!("cloudwatch_query.proposal.accept_and_run"),
                                    Some(KeyBinding::for_action_in(&RunQuery, &focus, cx)),
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.accept_proposal(true, window, cx),
                                )),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        Label::new(t!("cloudwatch_query.proposal.other_requests"))
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::BOLD)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                secondary_button(
                                    "cloudwatch-other-explain",
                                    Some((IconName::Sparkle, Color::Accent)),
                                    t!("cloudwatch_query.proposal.other_explain"),
                                    true,
                                    cx,
                                )
                                .h(px(28.))
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        let context = this.agent_query_context(cx);
                                        agent_bridge::send_to_agent(
                                            context,
                                            this.file_name().into(),
                                            Some(
                                                t!("cloudwatch_query.proposal.explain_prompt")
                                                    .to_string(),
                                            ),
                                            true,
                                            window,
                                            cx,
                                        );
                                    },
                                )),
                            )
                            .children(others.into_iter().enumerate().map(|(index, request)| {
                                secondary_button(
                                    ("cloudwatch-other-request", index),
                                    Some((IconName::Sparkle, Color::Accent)),
                                    request.clone(),
                                    true,
                                    cx,
                                )
                                .h(px(28.))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.submit_request(request.to_string(), cx)
                                    },
                                ))
                            })),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn render_summary_card(
        &self,
        summary: &Summary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let accent = colors.border_focused;
        let max = summary
            .patterns
            .iter()
            .map(|pattern| pattern.count)
            .max()
            .unwrap_or(1)
            .max(1);
        let first_link = summary
            .patterns
            .iter()
            .filter_map(|pattern| results::stack_link(pattern.location.as_deref()?))
            .find(|link| results::resolve_in_project(&self.project, &link.path, cx).is_some());
        let narrative = summary.narrative.clone();
        v_flex()
            .flex_none()
            .p_4()
            .gap_2()
            .rounded(Radius::Lg.rems())
            .border_1()
            .border_color(accent)
            .bg(accent.opacity(0.06))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Icon::new(IconName::Sparkle)
                            .size(IconSize::Small)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new(t!("cloudwatch_query.summary.title")).weight(FontWeight::BOLD),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(t!("cloudwatch_query.summary.read", count = summary.events))
                                .size(LabelSize::Small)
                                .color(Color::Placeholder)
                                .truncate(),
                        ),
                    )
                    .child(
                        IconButton::new("cloudwatch-close-summary", IconName::Close)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.assist.summary = None;
                                cx.notify();
                            })),
                    ),
            )
            .children(summary.patterns.iter().enumerate().map(|(index, pattern)| {
                let color = match index {
                    0 => Color::Error,
                    1 => Color::Warning,
                    _ => Color::Muted,
                };
                h_flex()
                    .gap_3()
                    .child(
                        div().w(px(34.)).flex_none().child(
                            Label::new(format!("{}×", pattern.count))
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD)
                                .color(color),
                        ),
                    )
                    .child(
                        div().w(px(120.)).flex_none().child(
                            div()
                                .h(px(8.))
                                .rounded(px(2.))
                                .bg(color.color(cx).opacity(0.8))
                                .w(px(120. * pattern.count as f32 / max as f32)),
                        ),
                    )
                    .child(
                        Label::new(pattern.message.clone())
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM),
                    )
                    .children(pattern.location.clone().map(|location| {
                        Label::new(location)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Accent)
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .children(pattern.detail.clone().map(|detail| {
                                Label::new(detail)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate()
                            })),
                    )
                    .children(pattern.since.clone().map(|since| {
                        Label::new(t!("cloudwatch_query.summary.since", time = since))
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder)
                    }))
            }))
            .child(
                div()
                    .pt_1()
                    .child(Label::new(narrative.clone()).size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .when_some(first_link, |this, link| {
                        let workspace = self.workspace.clone();
                        let project = self.project.clone();
                        this.child(
                            accent_button(
                                "cloudwatch-summary-open",
                                IconName::FileCode,
                                t!(
                                    "cloudwatch_query.event.open",
                                    location = results::link_label(&link)
                                ),
                                cx,
                            )
                            .h(px(26.))
                            .on_click(move |_, window, cx| {
                                results::open_stack_link(
                                    workspace.clone(),
                                    &project,
                                    &link,
                                    window,
                                    cx,
                                )
                            }),
                        )
                    })
                    .children(
                        summary
                            .followups
                            .iter()
                            .enumerate()
                            .map(|(index, followup)| {
                                let followup = followup.clone();
                                secondary_button(
                                    ("cloudwatch-followup", index),
                                    Some((IconName::Sparkle, Color::Accent)),
                                    followup.clone(),
                                    true,
                                    cx,
                                )
                                .h(px(26.))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.open_prompt(window, cx);
                                        this.submit_request(followup.clone(), cx);
                                    },
                                ))
                            }),
                    )
                    .child(
                        secondary_button(
                            "cloudwatch-summary-chat",
                            Some((IconName::Chat, Color::Muted)),
                            t!("cloudwatch_query.summary.continue_chat"),
                            true,
                            cx,
                        )
                        .h(px(26.))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                let context = this.agent_query_context(cx);
                                agent_bridge::send_to_agent(
                                    context,
                                    this.file_name().into(),
                                    Some(
                                        t!(
                                            "cloudwatch_query.summary.chat_prompt",
                                            summary = narrative
                                        )
                                        .to_string(),
                                    ),
                                    false,
                                    window,
                                    cx,
                                );
                            },
                        )),
                    ),
            )
            .into_any_element()
    }
}

pub(super) fn render_diff(lines: &[DiffLine], cx: &App) -> AnyElement {
    let error = Color::Error.color(cx);
    let success = Color::Success.color(cx);
    v_flex()
        .id("cloudwatch-diff")
        .size_full()
        .overflow_y_scroll()
        .children(lines.iter().map(|line| {
            let (marker, background, marker_color) = match line.kind {
                DiffKind::Removed => ("-", Some(error.opacity(0.14)), Color::Error),
                DiffKind::Added => ("+", Some(success.opacity(0.14)), Color::Success),
                DiffKind::Same => (" ", None, Color::Placeholder),
            };
            h_flex()
                .h(px(24.))
                .gap_2()
                .when_some(background, |this, background| this.bg(background))
                .child(
                    div().w(px(32.)).flex_none().flex().justify_end().child(
                        Label::new(line.number.to_string())
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Placeholder),
                    ),
                )
                .child(
                    div().w(px(12.)).flex_none().child(
                        Label::new(marker)
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(marker_color),
                    ),
                )
                .child(
                    Label::new(line.text.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(if line.kind == DiffKind::Removed {
                            Color::Muted
                        } else {
                            Color::Default
                        })
                        .single_line(),
                )
        }))
        .into_any_element()
}

/// "Sintaxe válida · usa route, status e durationMs, presentes em todos os 20 eventos da amostra".
pub(super) fn proposal_validity(proposal: &Proposal, sample: &Sample) -> SharedString {
    let fields = insights::referenced_fields(&proposal.query);
    if fields.is_empty() || sample.events.is_empty() {
        return t!("cloudwatch_query.proposal.valid");
    }
    let present = sample
        .events
        .iter()
        .filter(|event| {
            fields
                .iter()
                .all(|field| event.iter().any(|(name, _)| name == field))
        })
        .count();
    let names = match fields.split_last() {
        Some((last, rest)) if !rest.is_empty() => t!(
            "cloudwatch_query.proposal.field_list",
            first = rest.join(", "),
            last = last
        )
        .to_string(),
        _ => fields.join(", "),
    };
    if present == sample.events.len() {
        t!(
            "cloudwatch_query.proposal.valid_all",
            fields = names,
            count = sample.events.len()
        )
    } else {
        t!(
            "cloudwatch_query.proposal.valid_some",
            fields = names,
            present = present,
            count = sample.events.len()
        )
    }
}
