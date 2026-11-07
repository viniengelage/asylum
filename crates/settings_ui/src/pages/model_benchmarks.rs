use agent::model_benchmarks::{self, ModelBenchmarksSource, ModelBenchmarksStore};
use editor::Editor;
use fs::Fs;
use gpui::{AnyElement, App, Context, Window};
use language::Point;
use language_model::LanguageModelRegistry;
use ui::{Indicator, prelude::*, secondary_button};
use workspace::{OpenOptions, OpenVisible};

use crate::SettingsWindow;

fn available_model_ids(cx: &App) -> Vec<String> {
    LanguageModelRegistry::read_global(cx)
        .available_models(cx)
        .map(|model| format!("{}/{}", model.provider_id().0, model.id().0))
        .collect()
}

pub(crate) fn open_model_benchmarks(
    settings_window: &SettingsWindow,
    line: Option<usize>,
    window: &mut Window,
    cx: &mut App,
) {
    let workspace = settings_window.workspace.clone();
    let fs = <dyn Fs>::global(cx);
    window
        .spawn(cx, async move |cx| {
            let path = model_benchmarks::ensure_user_model_benchmarks(fs).await?;
            let open = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    path,
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })?;
            let item = open.await?;
            if let Some(line) = line
                && let Some(editor) = item.downcast::<Editor>()
            {
                let row = u32::try_from(line.saturating_sub(1)).unwrap_or_default();
                editor.update_in(cx, |editor, window, cx| {
                    editor.go_to_singleton_buffer_point(Point::new(row, 0), window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

fn restore_default(cx: &mut App) {
    let fs = <dyn Fs>::global(cx);
    let path = model_benchmarks::user_model_benchmarks_path();
    cx.background_spawn(model_benchmarks::restore_default_model_benchmarks(fs, path))
        .detach_and_log_err(cx);
}

fn add_rows(model_ids: Vec<String>, cx: &mut App) {
    let fs = <dyn Fs>::global(cx);
    cx.background_spawn(async move {
        let path = model_benchmarks::ensure_user_model_benchmarks(fs.clone()).await?;
        let text = fs.load(&path).await?;
        fs.atomic_write(path, model_benchmarks::append_model_rows(&text, &model_ids))
            .await
    })
    .detach_and_log_err(cx);
}

pub(crate) fn render_model_benchmarks_details(
    _settings_window: &SettingsWindow,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let loaded = ModelBenchmarksStore::global(cx).read(cx).loaded().clone();
    let available = available_model_ids(cx);
    let missing: Vec<String> = available
        .iter()
        .filter(|model_id| !loaded.benchmarks.has_model(model_id))
        .cloned()
        .collect();
    let colors = cx.theme().colors();

    let (path_label, source_label) = match &loaded.source {
        ModelBenchmarksSource::Default => (
            model_benchmarks::user_model_benchmarks_path()
                .display()
                .to_string(),
            i18n::t!("agent.model_benchmarks.source_default"),
        ),
        ModelBenchmarksSource::User(path) | ModelBenchmarksSource::Project(path) => (
            path.display().to_string(),
            i18n::t!("agent.model_benchmarks.source_user"),
        ),
    };
    let source_label = match loaded.benchmarks.reviewed.as_deref() {
        Some(date) => format!(
            "{source_label}, {}",
            i18n::t!("agent.model_benchmarks.reviewed", date = date)
        ),
        None => source_label.to_string(),
    };
    let problem_count = loaded.warnings.len() + usize::from(!missing.is_empty());
    let has_problems = problem_count > 0 || loaded.fallback.is_some();

    let status = if has_problems {
        h_flex()
            .gap_1p5()
            .child(Indicator::dot().color(Color::Warning))
            .child(
                Label::new(if problem_count == 1 {
                    i18n::t!("agent.model_benchmarks.warning_one")
                } else {
                    i18n::t!(
                        "agent.model_benchmarks.warning_other",
                        count = problem_count
                    )
                })
                .size(LabelSize::Small)
                .color(Color::Warning),
            )
            .into_any_element()
    } else if !available.is_empty() {
        h_flex()
            .gap_1p5()
            .child(Indicator::dot().color(Color::Success))
            .child(
                Label::new(i18n::t!(
                    "agent.model_benchmarks.covers_all",
                    count = available.len()
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .into_any_element()
    } else {
        div().into_any_element()
    };

    let file_row = h_flex()
        .w_full()
        .gap_2()
        .flex_wrap()
        .child(
            Icon::new(IconName::FileMarkdown)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .child(
            Label::new(path_label)
                .size(LabelSize::Small)
                .buffer_font(cx),
        )
        .child(
            Label::new("·")
                .size(LabelSize::Small)
                .color(Color::Disabled),
        )
        .child(
            Label::new(source_label)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            Label::new("·")
                .size(LabelSize::Small)
                .color(Color::Disabled),
        )
        .child(status)
        .child(div().flex_1())
        .when(
            matches!(loaded.source, ModelBenchmarksSource::User(_)),
            |this| {
                this.child(
                    secondary_button(
                        "model-benchmarks-restore",
                        Some((IconName::RotateCcw, Color::Muted)),
                        i18n::t!("agent.model_benchmarks.restore"),
                        true,
                        cx,
                    )
                    .on_click(|_, _, cx| restore_default(cx)),
                )
            },
        );

    let problem_row = |id: SharedString, text: SharedString, detail: Option<SharedString>| {
        h_flex().id(id).w_full().pl_6().gap_3().child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(Label::new(text).size(LabelSize::Small))
                .when_some(detail, |this, detail| {
                    this.child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                }),
        )
    };

    let warning_rows = loaded.warnings.iter().take(5).map(|warning| {
        let row = problem_row(
            format!("model-benchmarks-warning-{:?}", warning.line).into(),
            model_benchmarks::warning_message(warning),
            None,
        );
        match warning.line {
            Some(line) => row
                .child(
                    secondary_button(
                        SharedString::from(format!("model-benchmarks-line-{line}")),
                        None,
                        i18n::t!("agent.model_benchmarks.go_to_line", line = line),
                        true,
                        cx,
                    )
                    .on_click(cx.listener(
                        move |settings_window, _, window, cx| {
                            open_model_benchmarks(settings_window, Some(line), window, cx);
                        },
                    )),
                )
                .into_any_element(),
            None => row.into_any_element(),
        }
    });

    let missing_row = (!missing.is_empty()).then(|| {
        let models = missing.clone();
        problem_row(
            "model-benchmarks-missing".into(),
            i18n::t!(
                "agent.model_benchmarks.missing_models",
                count = missing.len()
            ),
            Some(missing.join(" · ").into()),
        )
        .child(
            secondary_button(
                "model-benchmarks-add-rows",
                None,
                i18n::t!("agent.model_benchmarks.add_rows"),
                true,
                cx,
            )
            .on_click(move |_, _, cx| add_rows(models.clone(), cx)),
        )
    });

    let problems = has_problems.then(|| {
        v_flex()
            .w_full()
            .gap_2p5()
            .p_3()
            .rounded_md()
            .bg(colors
                .status_bar_background
                .blend(cx.theme().status().warning.opacity(0.08)))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::Small)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(match loaded.fallback.as_ref() {
                            Some(reason) => {
                                i18n::t!("agent.model_benchmarks.fallback", reason = reason)
                            }
                            None => i18n::t!("agent.model_benchmarks.notice"),
                        })
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::MEDIUM),
                    ),
            )
            .children(warning_rows)
            .children(missing_row)
    });

    v_flex()
        .w_full()
        .gap_2p5()
        .child(file_row)
        .children(problems)
        .into_any_element()
}
