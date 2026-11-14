//! Small OCR results panel (not a launcher). Opened after region OCR finishes or fails.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::Button,
    h_flex,
    input::{Textarea, TextareaState},
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
};

use crate::appearance;
use crate::controller::{Controller, OcrState};

pub struct OcrView {
    controller: Entity<Controller>,
    ocr_text: Entity<TextareaState>,
    synced_ocr_revision: Option<usize>,
    _subscriptions: Vec<Subscription>,
}

impl OcrView {
    pub fn new(
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ocr_text = cx.new(|cx| TextareaState::new(window, cx));
        let subscription = cx.observe_in(&controller, window, |this, _, window, cx| {
            this.sync_ocr_text(window, cx);
            cx.notify();
        });
        let mut this = Self {
            controller,
            ocr_text,
            synced_ocr_revision: None,
            _subscriptions: vec![subscription, appearance::follow_system_theme(window)],
        };
        this.sync_ocr_text(window, cx);
        this
    }

    fn sync_ocr_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let controller = self.controller.read(cx);
        let revision = controller.ocr_revision();
        if self.synced_ocr_revision == Some(revision) {
            return;
        }
        self.synced_ocr_revision = Some(revision);
        let text = match controller.ocr() {
            OcrState::Done { text, .. } => text.clone(),
            _ => SharedString::default(),
        };
        self.ocr_text
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    fn copy_ocr_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.ocr_text.read(cx).value();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        window.push_notification(t!("ocr.copied").to_string(), cx);
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        match self.controller.read(cx).ocr() {
            OcrState::Idle => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("ocr.idle").to_string())
                .into_any_element(),
            OcrState::Running => h_flex()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Spinner::new().small())
                .child(t!("ocr.running").to_string())
                .into_any_element(),
            OcrState::Failed(error) => v_flex()
                .gap_1()
                .text_sm()
                .child(
                    div()
                        .text_color(theme.danger)
                        .child(t!("ocr.failed").to_string()),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(error.clone()),
                )
                .into_any_element(),
            OcrState::Done { text, engine } => {
                let is_empty = text.is_empty();
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(t!("ocr.result", engine = engine).to_string()),
                            )
                            .child(
                                Button::new("copy-ocr-text")
                                    .small()
                                    .icon(IconName::Copy)
                                    .label(t!("ocr.copy").to_string())
                                    .disabled(is_empty)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.copy_ocr_text(window, cx)
                                    })),
                            ),
                    )
                    .child(if is_empty {
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("ocr.empty").to_string())
                            .into_any_element()
                    } else {
                        Textarea::new(&self.ocr_text)
                            .readonly(true)
                            .flex_1()
                            .min_h_0()
                            .into_any_element()
                    })
                    .into_any_element()
            }
        }
    }
}

impl Render for OcrView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(t!("ocr.title").to_string()),
            )
            .child(self.render_body(cx))
    }
}
