//! Floating control bar and incremental live preview strip shown while a
//! scrolling screenshot is in progress.

use std::sync::Arc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::{
    App, Bounds, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyBinding,
    MouseButton, ObjectFit, ParentElement as _, Pixels, Render, RenderImage, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription,
    Window, actions, div, img, point, px, size,
};
use snapit_scroll::{ScrollDirection, ScrollStatus, ScrollWarning};

use crate::controller::{Controller, ScrollFinishAction};
use crate::recording_bar::exclude_from_capture;
use crate::session;

actions!(
    scroll_bar,
    [
        FinishCopy,
        FinishSave,
        FinishEdit,
        CancelScroll,
        ToggleAutoScroll,
        TogglePreview,
        ResetScroll
    ]
);

pub const SCROLL_BAR_SIZE: (f32, f32) = (540., 44.);
pub const PREVIEW_WINDOW_WIDTH: f32 = 224.;
const CONTEXT: &str = "ScrollBar";

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CancelScroll, Some(CONTEXT)),
        KeyBinding::new("enter", FinishCopy, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("s"), FinishSave, Some(CONTEXT)),
        KeyBinding::new("e", FinishEdit, Some(CONTEXT)),
        KeyBinding::new("space", ToggleAutoScroll, Some(CONTEXT)),
    ]);
}

pub fn scroll_preview_bounds(
    display_bounds: Bounds<Pixels>,
    selection: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let gap = px(10.);
    let margin = px(16.);
    let w = px(PREVIEW_WINDOW_WIDTH);
    let max_h = (display_bounds.size.height - margin * 2.).max(px(220.));
    let h = selection.size.height.max(px(260.)).min(max_h);
    let y = selection
        .origin
        .y
        .max(margin)
        .min((display_bounds.size.height - margin - h).max(margin));

    let right = selection.origin.x + selection.size.width;
    let x = if right + gap + w <= display_bounds.size.width {
        right + gap
    } else if selection.origin.x >= gap + w {
        selection.origin.x - gap - w
    } else {
        (display_bounds.size.width - w - margin).max(px(0.))
    };

    Bounds::new(display_bounds.origin + point(x, y), size(w, h))
}

pub struct ScrollBar {
    controller: Entity<Controller>,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

impl ScrollBar {
    pub fn new(
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        exclude_from_capture(window);
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let subscription = cx.observe(&controller, |_, _, cx| cx.notify());
        Self {
            controller,
            focus_handle,
            _subscription: subscription,
        }
    }

    fn drag(&mut self, window: &mut Window) {
        #[cfg(target_os = "macos")]
        window.start_window_move();
        #[cfg(target_os = "windows")]
        crate::recording_bar::native_drag(window);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let _ = window;
    }

    fn finish(&mut self, action: ScrollFinishAction, cx: &mut Context<Self>) {
        self.controller
            .update(cx, |controller, cx| controller.finish_scroll(action, cx));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.controller
            .update(cx, |controller, cx| controller.cancel_scroll(cx));
    }
}

impl Render for ScrollBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.read(cx);
        if !controller.has_scroll() {
            return div().id("scroll-bar").size_full();
        }
        let Some(snap) = controller.scroll_snapshot() else {
            return div().id("scroll-bar").size_full();
        };
        let finishing = controller.is_finishing_scroll();
        let preview_visible = controller.is_scroll_preview_visible();
        let started = snap.started;
        let can_act = !finishing;
        let can_export = started && !finishing;

        let theme = cx.theme();
        let dot_color = if snap.warning.is_some() || !started {
            theme.warning
        } else if snap.status == ScrollStatus::ReachedEnd {
            theme.success
        } else {
            theme.primary
        };

        let status_label = if let Some(warn) = snap.warning {
            match warn {
                ScrollWarning::TooFast => t!("scroll.warning.too_fast").to_string(),
                ScrollWarning::LowTexture => t!("scroll.warning.low_texture").to_string(),
            }
        } else if !started {
            t!("scroll.starting").to_string()
        } else {
            let state = match snap.status {
                ScrollStatus::Starting => t!("scroll.starting").to_string(),
                ScrollStatus::Capturing => t!("scroll.capturing").to_string(),
                ScrollStatus::AutoScrolling => t!("scroll.auto_scrolling").to_string(),
                ScrollStatus::Paused => t!("scroll.paused").to_string(),
                ScrollStatus::ReachedEnd => t!("scroll.reached_end").to_string(),
                ScrollStatus::MaxHeightReached => t!("scroll.max_height").to_string(),
                ScrollStatus::StoppedByUser => t!("scroll.stopped_by_user").to_string(),
            };
            format!("{state} · {}×{}", snap.width, snap.height)
        };

        let auto_hint = if snap.auto_scrolling {
            t!("scroll.auto_stop").to_string()
        } else {
            t!("scroll.auto_start").to_string()
        };
        let preview_hint = t!("scroll.preview_toggle").to_string();
        let reset_hint = t!("scroll.reset").to_string();
        let edit_hint = t!("scroll.edit").to_string();
        let ocr_hint = t!("overlay.ocr").to_string();
        let pin_hint = t!("overlay.pin").to_string();
        let save_hint = t!("scroll.save_hint").to_string();
        let cancel_hint = t!("scroll.cancel_hint").to_string();
        let copy_hint = t!("scroll.copy_hint").to_string();

        div()
            .id("scroll-bar")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &FinishCopy, _, cx| {
                if !this.controller.read(cx).is_finishing_scroll() {
                    this.finish(ScrollFinishAction::Copy, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FinishSave, _, cx| {
                if !this.controller.read(cx).is_finishing_scroll() {
                    this.finish(ScrollFinishAction::Save, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FinishEdit, _, cx| {
                if !this.controller.read(cx).is_finishing_scroll() {
                    this.finish(ScrollFinishAction::Edit, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CancelScroll, _, cx| {
                if !this.controller.read(cx).is_finishing_scroll() {
                    this.cancel(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleAutoScroll, _, cx| {
                this.controller
                    .update(cx, |controller, cx| controller.toggle_scroll_auto(cx));
            }))
            .on_action(cx.listener(|this, _: &TogglePreview, _, cx| {
                this.controller
                    .update(cx, |controller, cx| controller.toggle_scroll_preview(cx));
            }))
            .on_action(cx.listener(|this, _: &ResetScroll, _, cx| {
                this.controller
                    .update(cx, |controller, cx| controller.reset_scroll(cx));
            }))
            .child(
                h_flex()
                    .size_full()
                    .px_2()
                    .gap_2()
                    .justify_between()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .text_sm()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, _| this.drag(window)),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(div().size_2().flex_none().rounded_full().bg(dot_color))
                            .child(div().flex_1().min_w_0().truncate().child(status_label)),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Button::new("scroll-auto-toggle")
                                    .small()
                                    .ghost()
                                    .selected(snap.auto_scrolling)
                                    .icon(LucideIcon::ChevronsDown)
                                    .tooltip(auto_hint.clone())
                                    .accessibility_label(auto_hint)
                                    .disabled(!can_export)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.controller.update(cx, |controller, cx| {
                                            controller.toggle_scroll_auto(cx)
                                        });
                                    })),
                            )
                            .child(
                                Button::new("scroll-preview-toggle")
                                    .small()
                                    .ghost()
                                    .selected(preview_visible)
                                    .icon(LucideIcon::PanelRight)
                                    .tooltip(preview_hint.clone())
                                    .accessibility_label(preview_hint)
                                    .disabled(!can_act)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.controller.update(cx, |controller, cx| {
                                            controller.toggle_scroll_preview(cx)
                                        });
                                    })),
                            )
                            .child(
                                Button::new("scroll-reset")
                                    .small()
                                    .ghost()
                                    .icon(LucideIcon::RotateCcw)
                                    .tooltip(reset_hint.clone())
                                    .accessibility_label(reset_hint)
                                    .disabled(!can_export)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.controller.update(cx, |controller, cx| {
                                            controller.reset_scroll(cx)
                                        });
                                    })),
                            )
                            .child(div().w(px(1.)).h(px(16.)).bg(theme.border).mx_0p5())
                            .child(
                                Button::new("scroll-edit")
                                    .small()
                                    .ghost()
                                    .icon(LucideIcon::Pencil)
                                    .tooltip(edit_hint.clone())
                                    .accessibility_label(edit_hint)
                                    .disabled(!can_export)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.finish(ScrollFinishAction::Edit, cx)
                                    })),
                            )
                            .child(
                                Button::new("scroll-ocr")
                                    .small()
                                    .ghost()
                                    .icon(LucideIcon::ScanText)
                                    .tooltip(ocr_hint.clone())
                                    .accessibility_label(ocr_hint)
                                    .disabled(!can_export)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.finish(ScrollFinishAction::Recognize, cx)
                                    })),
                            )
                            .child(
                                Button::new("scroll-pin")
                                    .small()
                                    .ghost()
                                    .icon(LucideIcon::Pin)
                                    .tooltip(pin_hint.clone())
                                    .accessibility_label(pin_hint)
                                    .disabled(!can_export)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.finish(ScrollFinishAction::Pin, cx)
                                    })),
                            )
                            .child(
                                Button::new("scroll-save")
                                    .small()
                                    .ghost()
                                    .icon(LucideIcon::Save)
                                    .tooltip(save_hint.clone())
                                    .accessibility_label(save_hint)
                                    .disabled(!can_export)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.finish(ScrollFinishAction::Save, cx)
                                    })),
                            )
                            .child(
                                Button::new("scroll-cancel")
                                    .small()
                                    .ghost()
                                    .icon(IconName::Close)
                                    .tooltip(cancel_hint.clone())
                                    .accessibility_label(cancel_hint)
                                    .disabled(!can_act)
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                            )
                            .child(
                                Button::new("scroll-copy")
                                    .small()
                                    .primary()
                                    .icon(IconName::Copy)
                                    .tooltip(copy_hint.clone())
                                    .accessibility_label(copy_hint)
                                    .disabled(!can_export)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.finish(ScrollFinishAction::Copy, cx)
                                    })),
                            ),
                    ),
            )
    }
}

struct CachedPreviewTile {
    id: u64,
    revision: u64,
    render: Arc<RenderImage>,
    width: u32,
    height: u32,
}

pub struct ScrollPreview {
    controller: Entity<Controller>,
    tiles: Vec<CachedPreviewTile>,
    last_revision: u64,
    last_height: u32,
    scroll_handle: ScrollHandle,
    _subscription: Subscription,
}

impl ScrollPreview {
    pub fn new(
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        exclude_from_capture(window);
        let subscription = cx.observe(&controller, |_, _, cx| cx.notify());
        Self {
            controller,
            tiles: Vec::new(),
            last_revision: 0,
            last_height: 0,
            scroll_handle: ScrollHandle::new(),
            _subscription: subscription,
        }
    }

    fn drag(&mut self, window: &mut Window) {
        #[cfg(target_os = "macos")]
        window.start_window_move();
        #[cfg(target_os = "windows")]
        crate::recording_bar::native_drag(window);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let _ = window;
    }

    fn sync_tiles(&mut self, cx: &App) {
        let Some(snap) = self.controller.read(cx).scroll_snapshot() else {
            return;
        };
        if snap.preview_revision == self.last_revision {
            return;
        }
        let mut next = Vec::with_capacity(snap.preview_tiles.len());
        for tile in &snap.preview_tiles {
            if let Some(existing) = self
                .tiles
                .iter()
                .find(|cached| cached.id == tile.id && cached.revision == tile.revision)
            {
                next.push(CachedPreviewTile {
                    id: existing.id,
                    revision: existing.revision,
                    render: existing.render.clone(),
                    width: existing.width,
                    height: existing.height,
                });
            } else {
                let (width, height) = tile.image.dimensions();
                next.push(CachedPreviewTile {
                    id: tile.id,
                    revision: tile.revision,
                    render: session::render_image(&tile.image),
                    width,
                    height,
                });
            }
        }
        self.tiles = next;
        self.last_revision = snap.preview_revision;
        if snap.height > self.last_height {
            match snap.direction {
                ScrollDirection::Down => self.scroll_handle.scroll_to_bottom(),
                ScrollDirection::Up => self.scroll_handle.set_offset(point(px(0.), px(0.))),
            }
        }
        self.last_height = snap.height;
    }
}

impl Render for ScrollPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.controller.read(cx).has_scroll() {
            return div().id("scroll-preview").size_full();
        }
        self.sync_tiles(cx);
        let snap = self.controller.read(cx).scroll_snapshot();
        let dims = snap
            .as_ref()
            .filter(|s| s.started)
            .map(|s| format!("{} × {}", s.width, s.height))
            .unwrap_or_else(|| t!("scroll.preview_title").to_string());
        let theme = cx.theme();
        let content_w = (PREVIEW_WINDOW_WIDTH - 16.).max(64.);

        v_flex()
            .id("scroll-preview")
            .size_full()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .overflow_hidden()
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, _| this.drag(window)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(dims),
                    )
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Button::new("close-scroll-preview")
                                    .xsmall()
                                    .ghost()
                                    .icon(IconName::Close)
                                    .tooltip(t!("scroll.preview_toggle").to_string())
                                    .accessibility_label(t!("scroll.preview_toggle").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.controller.update(cx, |controller, cx| {
                                            controller.toggle_scroll_preview(cx)
                                        });
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .p_2()
                    .child(
                        div()
                            .id("scroll-preview-area")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .child(v_flex().w_full().children(self.tiles.iter().map(|tile| {
                                let tile_h = (tile.height as f32 * content_w
                                    / tile.width.max(1) as f32)
                                    .max(1.0);
                                img(tile.render.clone())
                                    .id(SharedString::from(format!("scroll-tile-{}", tile.id)))
                                    .w_full()
                                    .h(px(tile_h))
                                    .object_fit(ObjectFit::Fill)
                            }))),
                    )
                    .vertical_scrollbar(&self.scroll_handle),
            )
    }
}
