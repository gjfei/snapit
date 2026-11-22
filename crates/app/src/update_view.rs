//! Update window: check status, release notes, download progress.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    progress::Progress,
    scroll::ScrollableElement as _,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Window, div,
};

use crate::appearance;
use crate::updater::{self, Phase};

enum JobView {
    Downloading { done: u64, total: u64, percent: f32 },
    Ready { path: String },
    Installing,
    Failed(String),
}

pub struct UpdateView {
    _subscriptions: Vec<Subscription>,
}

impl UpdateView {
    pub fn new(window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            _subscriptions: vec![appearance::follow_system_theme(window)],
        }
    }

    fn check_button(&self, label: SharedString) -> Button {
        Button::new("check-updates")
            .label(label)
            .on_click(|_, _, cx| updater::check(true, cx))
    }

    /// `content` at the top, `actions` along the bottom edge. The window keeps
    /// a fixed size, so an action row that merely follows short content floats
    /// in the middle of an empty window.
    fn dialog(content: AnyElement, actions: Option<AnyElement>) -> AnyElement {
        v_flex()
            .size_full()
            .gap_3()
            .child(div().flex_1().min_h_0().child(content))
            .when_some(actions, |this, actions| {
                this.child(h_flex().w_full().justify_end().child(actions))
            })
            .into_any_element()
    }

    fn body(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let state = updater::state(cx);
        let muted = theme.muted_foreground;
        let danger = theme.danger;

        if state.checking() {
            return Self::dialog(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(muted)
                    .child(Spinner::new().small())
                    .child(t!("update.checking").to_string())
                    .into_any_element(),
                None,
            );
        }
        if let Some(error) = state.error() {
            return Self::dialog(
                v_flex()
                    .gap_1()
                    .text_sm()
                    .child(
                        div()
                            .text_color(danger)
                            .child(t!("update.check_failed").to_string()),
                    )
                    .child(div().text_color(muted).child(error.to_string()))
                    .into_any_element(),
                Some(
                    self.check_button(t!("update.check_again").to_string().into())
                        .into_any_element(),
                ),
            );
        }
        let Some(release) = state.release() else {
            return Self::dialog(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .text_color(muted)
                    .when(state.checked(), |this| {
                        this.child(
                            t!("update.up_to_date", version = updater::CURRENT_VERSION).to_string(),
                        )
                    })
                    .into_any_element(),
                state.checked().then(|| {
                    self.check_button(t!("update.check_again").to_string().into())
                        .into_any_element()
                }),
            );
        };

        let notes = if release.notes.is_empty() {
            t!("update.no_notes").to_string()
        } else {
            release.notes.clone()
        };
        let title = t!("update.available", version = release.version).to_string();
        let job = state.job().map(|job| match &job.phase {
            Phase::Downloading => {
                let done = state.downloaded();
                let total = job.total;
                JobView::Downloading {
                    done,
                    total,
                    percent: done as f32 * 100. / total.max(1) as f32,
                }
            }
            Phase::Ready => JobView::Ready {
                path: job.path().unwrap_or_default(),
            },
            Phase::Installing => JobView::Installing,
            Phase::Failed(message) => JobView::Failed(message.clone()),
        });
        let busy = state.busy();
        let downloading = matches!(state.job().map(|job| &job.phase), Some(Phase::Downloading));
        let has_asset = release.asset.is_some();
        let pre_release = updater::needs_confirm(updater::CURRENT_VERSION, &release.version);
        let page_url = state.page_url().map(str::to_string);

        let status = match job {
            Some(JobView::Downloading {
                done,
                total,
                percent,
            }) => Some(
                v_flex()
                    .gap_1()
                    .child(Progress::new("update-progress").value(percent))
                    .child(
                        div().text_xs().text_color(muted).child(
                            t!(
                                "update.downloading",
                                done = format_mb(done),
                                total = format_mb(total)
                            )
                            .to_string(),
                        ),
                    )
                    .into_any_element(),
            ),
            Some(JobView::Ready { path }) => Some(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(t!("update.ready", path = path).to_string()),
                    )
                    .when(cfg!(target_os = "macos"), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("update.reveal_hint").to_string()),
                        )
                    })
                    .into_any_element(),
            ),
            Some(JobView::Installing) => Some(
                v_flex()
                    .gap_2()
                    .child(Progress::new("update-progress").loading(true))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("update.installing").to_string()),
                    )
                    .into_any_element(),
            ),
            Some(JobView::Failed(message)) => Some(
                div()
                    .text_sm()
                    .text_color(danger)
                    .child(t!("update.failed", error = message).to_string())
                    .into_any_element(),
            ),
            None => None,
        };

        let ready = matches!(state.job().map(|job| &job.phase), Some(Phase::Ready));
        let actions = if downloading {
            h_flex().gap_2().child(
                Button::new("cancel-update")
                    .label(t!("update.cancel").to_string())
                    .on_click(|_, _, cx| updater::dismiss_job(cx)),
            )
        } else if ready {
            #[cfg(target_os = "macos")]
            let finish = Button::new("update-reveal")
                .primary()
                .label(t!("update.reveal").to_string())
                .on_click(|_, _, cx| updater::reveal_ready(cx));
            #[cfg(not(target_os = "macos"))]
            let finish = Button::new("update-install")
                .primary()
                .label(t!("update.install_restart").to_string())
                .on_click(|_, _, cx| updater::install_ready(cx));
            h_flex()
                .gap_2()
                .child(
                    Button::new("update-later")
                        .label(t!("update.later").to_string())
                        .on_click(|_, window, cx| {
                            updater::dismiss_job(cx);
                            window.remove_window();
                        }),
                )
                .child(finish)
        } else if busy {
            h_flex()
        } else {
            h_flex()
                .gap_2()
                .child(
                    Button::new("update-later")
                        .label(t!("update.later").to_string())
                        .on_click(|_, window, cx| {
                            updater::dismiss_job(cx);
                            window.remove_window();
                        }),
                )
                .child(
                    Button::new("update-now")
                        .primary()
                        .label(if pre_release {
                            t!("update.install_pre").to_string()
                        } else {
                            t!("update.now").to_string()
                        })
                        .disabled(!has_asset)
                        .when(has_asset, |this| this.cursor_pointer())
                        .on_click(|_, _, cx| updater::start_update(cx)),
                )
        };

        let content = v_flex()
            .gap_3()
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
            .child(
                div()
                    .id("update-notes")
                    .max_h(gpui_kit::px(240.))
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.muted)
                    .text_sm()
                    .overflow_y_scrollbar()
                    .child(notes),
            )
            .when(pre_release, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.warning)
                        .child(t!("update.pre_release").to_string()),
                )
            })
            .when_some(status, |this, status| this.child(status))
            .when(!has_asset, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(danger)
                        .child(t!("update.error.no_asset").to_string()),
                )
            })
            .when_some(page_url, |this, url| {
                this.child(
                    Button::new("update-page")
                        .link()
                        .label(t!("update.open_page").to_string())
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
            })
            .into_any_element();
        Self::dialog(content, Some(actions.into_any_element()))
    }
}

fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.)
}

impl Render for UpdateView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("update.title").to_string()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                t!("update.current_version", version = updater::CURRENT_VERSION)
                                    .to_string(),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.body(cx)))
    }
}
