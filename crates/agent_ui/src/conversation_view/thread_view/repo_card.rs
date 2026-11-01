use super::{ThreadView, ToolCallLayout};
use acp_thread::{ToolCall, ToolCallStatus};
use gpui::AnyElement;
use i18n::t;
use task_agents::{
    PullRequestCard, PullRequestCheckState, PullRequestMergeCard, REPO_MERGE_TOOL,
    REPO_PULL_REQUEST_TOOL,
};
use ui::prelude::*;

pub(super) enum RepoCard {
    PullRequest(PullRequestCard),
    Merged(PullRequestMergeCard),
}

pub(super) fn repo_card(tool_call: &ToolCall) -> Option<RepoCard> {
    if !matches!(tool_call.status(), ToolCallStatus::Completed) {
        return None;
    }
    let raw_output = tool_call.raw_output.clone()?;
    match tool_call.tool_name.as_deref()? {
        REPO_PULL_REQUEST_TOOL => serde_json::from_value(raw_output)
            .ok()
            .map(RepoCard::PullRequest),
        REPO_MERGE_TOOL => serde_json::from_value(raw_output)
            .ok()
            .map(RepoCard::Merged),
        _ => None,
    }
}

fn badge(text: impl Into<SharedString>, color: Color, cx: &App) -> AnyElement {
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .bg(color.color(cx).opacity(0.12))
        .child(Label::new(text).size(LabelSize::XSmall).color(color))
        .into_any_element()
}

fn dot(color: Color, cx: &App) -> AnyElement {
    div()
        .flex_none()
        .size(px(6.))
        .rounded_full()
        .bg(color.color(cx))
        .into_any_element()
}

fn state_badge(card: &PullRequestCard) -> (SharedString, Color) {
    match card.state.as_str() {
        "merged" => (t!("agent_ui.repo_card.state_merged"), Color::Success),
        "declined" => (t!("agent_ui.repo_card.state_declined"), Color::Error),
        "superseded" => (t!("agent_ui.repo_card.state_superseded"), Color::Muted),
        _ if card.draft => (t!("agent_ui.repo_card.state_draft"), Color::Muted),
        _ => (t!("agent_ui.repo_card.state_open"), Color::Accent),
    }
}

fn check_label(state: PullRequestCheckState) -> (SharedString, Color) {
    match state {
        PullRequestCheckState::Passed => (t!("agent_ui.repo_card.check_passed"), Color::Success),
        PullRequestCheckState::Failed => (t!("agent_ui.repo_card.check_failed"), Color::Error),
        PullRequestCheckState::Running => (t!("agent_ui.repo_card.check_running"), Color::Warning),
        PullRequestCheckState::Stopped => (t!("agent_ui.repo_card.check_stopped"), Color::Muted),
    }
}

impl ThreadView {
    pub(super) fn render_repo_card(
        &self,
        entry_ix: usize,
        card: RepoCard,
        layout: ToolCallLayout,
        cx: &Context<Self>,
    ) -> AnyElement {
        let content = match card {
            RepoCard::PullRequest(card) => self.render_pull_request_card(entry_ix, card, cx),
            RepoCard::Merged(card) => Self::render_merge_card(entry_ix, card),
        };
        v_flex()
            .when(layout == ToolCallLayout::Standalone, |this| {
                this.ml_5().mr_5()
            })
            .mt_1()
            .mb_2()
            .rounded_md()
            .border_1()
            .border_color(self.tool_card_border_color(cx))
            .bg(cx.theme().colors().editor_background)
            .overflow_hidden()
            .children(content)
            .into_any_element()
    }

    fn render_repo_section_title(&self, title: SharedString) -> AnyElement {
        Label::new(title)
            .size(LabelSize::XSmall)
            .color(Color::Muted)
            .into_any_element()
    }

    fn render_pull_request_card(
        &self,
        entry_ix: usize,
        card: PullRequestCard,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let (state_text, state_color) = state_badge(&card);
        let header = v_flex()
            .w_full()
            .gap_0p5()
            .px_2()
            .py_1p5()
            .bg(self.tool_card_header_bg(cx))
            .border_b_1()
            .border_color(self.tool_card_border_color(cx))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(IconSize::Small)
                            .color(state_color),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(format!("#{} {}", card.number, card.title))
                                .size(LabelSize::Small)
                                .truncate(),
                        ),
                    )
                    .when(card.is_current_branch, |this| {
                        this.child(badge(
                            t!("agent_ui.repo_card.your_branch"),
                            Color::Muted,
                            cx,
                        ))
                    })
                    .child(badge(state_text, state_color, cx)),
            )
            .child(
                Label::new(t!(
                    "agent_ui.repo_card.summary_line",
                    source = card.source_branch,
                    destination = card.destination_branch,
                    added = card.lines_added,
                    removed = card.lines_removed,
                    files = if card.file_count == 1 {
                        t!("agent_ui.repo_card.files.one")
                    } else {
                        t!("agent_ui.repo_card.files.other", count = card.file_count)
                    }
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .buffer_font(cx)
                .truncate(),
            )
            .into_any_element();

        let mut elements = vec![header];

        if !card.checks.is_empty() {
            let mut checks = v_flex()
                .gap_0p5()
                .px_2()
                .py_1p5()
                .border_b_1()
                .border_color(self.tool_card_border_color(cx))
                .child(self.render_repo_section_title(t!("agent_ui.repo_card.checks_title")));
            for (check_ix, check) in card.checks.iter().enumerate() {
                let (label, color) = check_label(check.state);
                let log_url = check
                    .url
                    .clone()
                    .filter(|_| check.state == PullRequestCheckState::Failed);
                checks = checks.child(
                    h_flex()
                        .gap_2()
                        .child(dot(color, cx))
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(check.name.clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            ),
                        )
                        .child(Label::new(label).size(LabelSize::XSmall).color(color))
                        .when_some(log_url, |this, url| {
                            this.child(
                                Button::new(
                                    SharedString::from(format!(
                                        "repo-card-check-{entry_ix}-{check_ix}"
                                    )),
                                    t!("agent_ui.repo_card.view_log"),
                                )
                                .label_size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                        }),
                );
            }
            elements.push(checks.into_any_element());
        }

        let mut reviews = v_flex()
            .gap_0p5()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(self.tool_card_border_color(cx))
            .child(self.render_repo_section_title(t!("agent_ui.repo_card.reviews_title")));
        if card.reviewers.is_empty() {
            reviews = reviews.child(
                Label::new(t!("agent_ui.repo_card.no_reviews"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        for reviewer in &card.reviewers {
            let (verdict, color) = if reviewer.requested_changes {
                (
                    t!("agent_ui.repo_card.review_changes_requested"),
                    Color::Warning,
                )
            } else if reviewer.approved {
                (t!("agent_ui.repo_card.review_approved"), Color::Success)
            } else {
                (t!("agent_ui.repo_card.review_waiting"), Color::Muted)
            };
            reviews = reviews.child(
                h_flex()
                    .gap_2()
                    .child(dot(color, cx))
                    .child(Label::new(reviewer.name.clone()).size(LabelSize::Small))
                    .child(Label::new(verdict).size(LabelSize::XSmall).color(color)),
            );
        }
        elements.push(reviews.into_any_element());

        let approvals = card
            .reviewers
            .iter()
            .filter(|reviewer| reviewer.approved)
            .count();
        let mut summary = vec![if card.conflict_count > 0 {
            if card.conflict_count == 1 {
                t!("agent_ui.repo_card.conflicts.one")
            } else {
                t!(
                    "agent_ui.repo_card.conflicts.other",
                    count = card.conflict_count
                )
            }
        } else {
            t!(
                "agent_ui.repo_card.no_conflicts",
                branch = card.destination_branch
            )
        }];
        summary.push(t!(
            "agent_ui.repo_card.approvals",
            approved = approvals,
            total = card.reviewers.len()
        ));
        if card.open_task_count > 0 {
            summary.push(if card.open_task_count == 1 {
                t!("agent_ui.repo_card.open_tasks.one")
            } else {
                t!(
                    "agent_ui.repo_card.open_tasks.other",
                    count = card.open_task_count
                )
            });
        }

        let number = card.number;
        let url = card.url.clone();
        let is_open = card.state == "open";
        let footer = h_flex()
            .w_full()
            .flex_wrap()
            .gap_1()
            .px_1()
            .py_1()
            .when(is_open, |this| {
                this.child(
                    Button::new(
                        ("repo-card-diff", entry_ix),
                        t!("agent_ui.repo_card.view_diff"),
                    )
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::Diff)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(zed_actions::repo_hosting::DiffPullRequest { number }),
                            cx,
                        )
                    }),
                )
                .when(!card.is_current_branch, |this| {
                    this.child(
                        Button::new(
                            ("repo-card-checkout", entry_ix),
                            t!("agent_ui.repo_card.checkout"),
                        )
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .start_icon(
                            Icon::new(IconName::GitBranch)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(zed_actions::repo_hosting::CheckoutPullRequest { number }),
                                cx,
                            )
                        }),
                    )
                })
            })
            .child(
                Button::new(
                    ("repo-card-open", entry_ix),
                    t!("agent_ui.repo_card.open_in_repo"),
                )
                .label_size(LabelSize::Small)
                .color(Color::Muted)
                .start_icon(
                    Icon::new(IconName::PullRequest)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .on_click(move |_, window, cx| {
                    window.dispatch_action(
                        Box::new(zed_actions::repo_hosting::OpenPullRequest { number }),
                        cx,
                    )
                }),
            )
            .child(
                Button::new(("repo-card-web", entry_ix), "Bitbucket")
                    .label_size(LabelSize::Small)
                    .color(Color::Muted)
                    .start_icon(
                        Icon::new(IconName::ArrowUpRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
            .child(div().flex_1())
            .child(
                Label::new(summary.join(" · "))
                    .size(LabelSize::XSmall)
                    .color(if card.conflict_count > 0 {
                        Color::Error
                    } else {
                        Color::Muted
                    }),
            )
            .into_any_element();
        elements.push(footer);
        elements
    }

    fn render_merge_card(entry_ix: usize, card: PullRequestMergeCard) -> Vec<AnyElement> {
        let url = card.url.clone();
        vec![
            h_flex()
                .w_full()
                .gap_2()
                .px_2()
                .py_1p5()
                .child(
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Success),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(format!("#{} {}", card.number, card.title))
                                .size(LabelSize::Small)
                                .truncate(),
                        )
                        .child(
                            Label::new(t!(
                                "agent_ui.repo_card.merged_into",
                                branch = card.destination_branch,
                                strategy = card.strategy
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        ),
                )
                .child(
                    Button::new(("repo-merge-card-web", entry_ix), "Bitbucket")
                        .label_size(LabelSize::Small)
                        .color(Color::Muted)
                        .start_icon(
                            Icon::new(IconName::ArrowUpRight)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
                .into_any_element(),
        ]
    }
}
