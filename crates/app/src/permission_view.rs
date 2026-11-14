//! Permission guidance panel: shows missing capture / accessibility grants and
//! lets the user open System Settings, re-check, or continue a paused capture.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::{
    App, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};

use crate::appearance;
use crate::controller::Controller;
use crate::permissions::{self, Permission};
use crate::settings::AppSettings;

pub struct PermissionView {
    controller: Entity<Controller>,
    _subscriptions: Vec<Subscription>,
}

impl PermissionView {
    pub fn new(
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe_in(&controller, window, |_, _, _, cx| cx.notify());
        let view = cx.entity();
        let released = controller.clone();
        let release_subscription = cx.observe_release(&view, move |_, _, cx| {
            released.update(cx, |controller, _| controller.clear_pending_capture());
        });
        Self {
            controller,
            _subscriptions: vec![
                subscription,
                release_subscription,
                appearance::follow_system_theme(window),
            ],
        }
    }

    /// Permissions worth showing, each with its current grant state.
    fn rows(&self, cx: &App) -> Vec<(Permission, bool)> {
        let mut rows = vec![(
            Permission::ScreenRecording,
            permissions::screen_recording_granted(),
        )];
        if AppSettings::get(cx).capture.element_snap_enabled() {
            rows.push((
                Permission::Accessibility,
                permissions::accessibility_granted(),
            ));
        }
        rows
    }
}

fn permission_row(
    permission: Permission,
    granted: bool,
    cx: &mut Context<PermissionView>,
) -> impl IntoElement {
    let theme = cx.theme();
    let hint = permission.hint();
    let (status, status_color) = if granted {
        (t!("permission.granted").to_string(), theme.success)
    } else {
        (t!("permission.missing").to_string(), theme.danger)
    };
    v_flex()
        .gap_1()
        .p_3()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(permission.label()),
                )
                .child(div().text_xs().text_color(status_color).child(status)),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(hint),
        )
        .when(!granted, |this| {
            this.child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("open-settings-{permission:?}")))
                            .small()
                            .icon(IconName::ExternalLink)
                            .label(t!("permission.open_settings").to_string())
                            .on_click(cx.listener(move |_, _, _, _| {
                                permissions::open_settings(permission);
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("recheck-{permission:?}")))
                            .small()
                            .icon(IconName::RefreshCw)
                            .label(t!("permission.recheck").to_string())
                            .on_click(cx.listener(|_, _, _, cx| cx.notify())),
                    ),
            )
        })
}

impl Render for PermissionView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let muted_foreground = cx.theme().muted_foreground;
        let rows = self.rows(cx);
        let ready = rows.iter().all(|(_, granted)| *granted);
        let pending_capture = self.controller.read(cx).pending_capture().is_some();
        let screen_granted = permissions::screen_recording_granted();
        let mut body =
            v_flex().gap_2().child(
                div()
                    .text_sm()
                    .text_color(muted_foreground)
                    .child(if ready {
                        t!("permission.ready").to_string()
                    } else {
                        t!("permission.required").to_string()
                    }),
            );
        for (permission, granted) in rows {
            body = body.child(permission_row(permission, granted, cx));
        }
        let close = cx.listener(|this, _, window, cx| {
            this.controller
                .update(cx, |controller, _| controller.clear_pending_capture());
            window.remove_window();
        });
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(background)
            .text_color(foreground)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t!("permission.title").to_string()),
            )
            .child(body)
            .child(div().flex_1())
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("close-permissions")
                            .small()
                            .label(t!("permission.close").to_string())
                            .on_click(close),
                    )
                    .when(pending_capture, |this| {
                        this.child(
                            Button::new("continue-capture")
                                .small()
                                .primary()
                                .icon(IconName::ArrowRight)
                                .label(t!("permission.continue_capture").to_string())
                                .disabled(!screen_granted)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.controller.update(cx, |controller, cx| {
                                        controller.continue_pending_capture(cx)
                                    });
                                    window.remove_window();
                                })),
                        )
                    }),
            )
    }
}
