//! Floating control bar and incremental live preview strip shown while a
//! scrolling screenshot is in progress.

use std::{
    cell::Cell,
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex,
    scroll::ScrollableElement as _,
    toolbar::Toolbar,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, CursorStyle, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Hsla, InteractiveElement as _, IntoElement, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement as _, Pixels, Point, Render, RenderImage, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, UTF16Selection,
    Window, actions, canvas, div, img, point, prelude::FluentBuilder as _, px, rgb, size,
};
use snapit_capture::RgbaImage;
use snapit_scroll::{ScrollDirection, ScrollStatus, ScrollWarning};

use crate::annotate::{self, Layer, Tool};
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
        ResetScroll,
        UndoAnnotation,
        DeleteAnnotation,
        CommitAnnotation,
        CancelAnnotation,
    ]
);

pub const SCROLL_BAR_SIZE: (f32, f32) = (568., 44.);
pub const PREVIEW_WINDOW_WIDTH: f32 = 280.;
const CONTEXT: &str = "ScrollBar";
const PREVIEW_CONTEXT: &str = "ScrollPreview";
const PREVIEW_CONTENT_WIDTH: f32 = PREVIEW_WINDOW_WIDTH - 16.;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CancelScroll, Some(CONTEXT)),
        KeyBinding::new("enter", FinishCopy, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("s"), FinishSave, Some(CONTEXT)),
        KeyBinding::new("e", FinishEdit, Some(CONTEXT)),
        KeyBinding::new("space", ToggleAutoScroll, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("z"), UndoAnnotation, Some(PREVIEW_CONTEXT)),
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

        let theme = cx.theme().clone();
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
                ScrollStatus::AutoScrolling => match snap.auto_direction {
                    ScrollDirection::Up => t!("scroll.auto_up").to_string(),
                    ScrollDirection::Down => t!("scroll.auto_down").to_string(),
                },
                ScrollStatus::Paused => t!("scroll.paused").to_string(),
                ScrollStatus::ReachedEnd => match snap.auto_direction {
                    ScrollDirection::Up => t!("scroll.reached_top").to_string(),
                    ScrollDirection::Down => t!("scroll.reached_end").to_string(),
                },
                ScrollStatus::MaxHeightReached => t!("scroll.max_height").to_string(),
                ScrollStatus::StoppedByUser => t!("scroll.stopped_by_user").to_string(),
            };
            format!("{state} · {}×{}", snap.width, snap.height)
        };

        let auto_up_hint = t!("scroll.auto_up_hint").to_string();
        let auto_down_hint = t!("scroll.auto_down_hint").to_string();
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
                if this.controller.read(cx).can_finish_scroll() {
                    this.finish(ScrollFinishAction::Copy, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FinishSave, _, cx| {
                if this.controller.read(cx).can_finish_scroll() {
                    this.finish(ScrollFinishAction::Save, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FinishEdit, _, cx| {
                if this.controller.read(cx).can_finish_scroll() {
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
                                Button::new("scroll-auto-up")
                                    .small()
                                    .ghost()
                                    .selected(
                                        snap.auto_scrolling
                                            && snap.auto_direction == ScrollDirection::Up,
                                    )
                                    .icon(LucideIcon::ArrowUp)
                                    .tooltip(auto_up_hint.clone())
                                    .accessibility_label(auto_up_hint)
                                    .disabled(!can_export)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.controller.update(cx, |controller, cx| {
                                            controller
                                                .set_scroll_auto_direction(ScrollDirection::Up, cx)
                                        });
                                    })),
                            )
                            .child(
                                Button::new("scroll-auto-down")
                                    .small()
                                    .ghost()
                                    .selected(
                                        snap.auto_scrolling
                                            && snap.auto_direction == ScrollDirection::Down,
                                    )
                                    .icon(LucideIcon::ArrowDown)
                                    .tooltip(auto_down_hint.clone())
                                    .accessibility_label(auto_down_hint)
                                    .disabled(!can_export)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.controller.update(cx, |controller, cx| {
                                            controller.set_scroll_auto_direction(
                                                ScrollDirection::Down,
                                                cx,
                                            )
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

#[derive(Clone)]
struct CachedPreviewTile {
    revision: u64,
    image: Arc<RgbaImage>,
    render: Arc<RenderImage>,
    width: u32,
    height: u32,
}

fn anchored_preview_offset(
    offset: f32,
    old_height: u32,
    new_height: u32,
    prepended_rows: u32,
    direction: ScrollDirection,
    viewport_height: f32,
    source_per_pixel: f32,
) -> f32 {
    let old_max = (old_height as f32 * source_per_pixel - viewport_height).max(0.0);
    let new_max = (new_height as f32 * source_per_pixel - viewport_height).max(0.0);
    let offset = offset.clamp(0.0, old_max);
    let target = match direction {
        ScrollDirection::Up if offset <= 1.0 => 0.0,
        ScrollDirection::Up => offset + prepended_rows as f32 * source_per_pixel,
        ScrollDirection::Down if offset >= old_max - 1.0 => new_max,
        ScrollDirection::Down => offset,
    };
    target.clamp(0.0, new_max)
}

pub struct ScrollPreview {
    controller: Entity<Controller>,
    tile: Option<CachedPreviewTile>,
    last_revision: u64,
    last_height: u32,
    last_content_shift: u32,
    last_frames: usize,
    requested_range: (u32, u32),
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
    color_picker: Entity<ColorPickerState>,
    style_open: bool,
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    mosaics: Arc<Mutex<annotate::MosaicCache>>,
    _subscriptions: Vec<Subscription>,
}

impl ScrollPreview {
    pub fn new(
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        exclude_from_capture(window);
        let color = controller
            .read(cx)
            .scroll_annotations()
            .map_or(0xEF4444, |layer| layer.style().color);
        let color_picker =
            cx.new(|cx| ColorPickerState::new(window, cx).default_value(Hsla::from(rgb(color))));
        let subscriptions = vec![
            cx.observe(&controller, |_, _, cx| cx.notify()),
            cx.subscribe(&color_picker, |this, _, event: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(Some(color)) = event else {
                    return;
                };
                this.controller.update(cx, |controller, cx| {
                    controller.set_scroll_annotation_color(color_to_rgb(*color), cx);
                    controller.persist_scroll_annotation_defaults(cx);
                });
            }),
        ];
        Self {
            controller,
            tile: None,
            last_revision: 0,
            last_height: 0,
            last_content_shift: 0,
            last_frames: 0,
            requested_range: (0, 1),
            scroll_handle: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            color_picker,
            style_open: false,
            canvas_bounds: Rc::new(Cell::new(None)),
            mosaics: Arc::new(Mutex::new(annotate::MosaicCache::default())),
            _subscriptions: subscriptions,
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

    fn sync_tile(&mut self, cx: &mut Context<Self>, window: &mut Window) {
        let Some(snap) = self.controller.read(cx).scroll_snapshot() else {
            return;
        };
        if snap.height == 0 || snap.width == 0 {
            return;
        }
        let content_w = (PREVIEW_WINDOW_WIDTH - 16.).max(64.);
        let source_per_pixel = snap.width as f32 / content_w;
        let viewport_height = self
            .scroll_handle
            .bounds()
            .size
            .height
            .as_f32()
            .max(1.0)
            .min(window.viewport_size().height.as_f32());
        let offset = anchored_preview_offset(
            self.scroll_handle.offset().y.as_f32(),
            self.last_height,
            snap.height,
            snap.content_shift.saturating_sub(self.last_content_shift),
            snap.direction,
            viewport_height,
            source_per_pixel,
        );
        let reset = snap.frames < self.last_frames || snap.content_shift < self.last_content_shift;
        if reset {
            self.scroll_handle.set_offset(point(px(0.), px(0.)));
            self.requested_range = (0, 1);
        } else if snap.height > self.last_height || snap.content_shift > self.last_content_shift {
            self.scroll_handle.set_offset(point(px(0.), px(offset)));
        }
        self.last_height = snap.height;
        self.last_content_shift = snap.content_shift;
        self.last_frames = snap.frames;
        let viewport_rows = (viewport_height / source_per_pixel).ceil().max(1.0) as u32;
        let first = (f32::from(self.scroll_handle.offset().y) / source_per_pixel).max(0.0) as u32;
        let start = first
            .saturating_sub(viewport_rows / 2)
            .min(snap.height.saturating_sub(1));
        let rows = viewport_rows
            .saturating_mul(3)
            .min(snap.height - start)
            .max(1);
        if self.requested_range != (start, rows) {
            self.requested_range = (start, rows);
            self.controller.update(cx, |controller, _| {
                controller.set_scroll_preview_range(start, rows)
            });
        }
        if snap.preview_revision == self.last_revision {
            return;
        }
        let Some(preview) = &snap.preview else {
            return;
        };
        let (width, height) = preview.image.dimensions();
        self.tile = Some(CachedPreviewTile {
            revision: preview.revision,
            image: preview.image.clone(),
            render: session::render_image(&preview.image),
            width,
            height,
        });
        self.last_revision = snap.preview_revision;
    }

    fn preview_zoom(&self, cx: &Context<Self>) -> Option<f32> {
        let snap = self.controller.read(cx).scroll_snapshot()?;
        let scale = self.controller.read(cx).scroll_annotation_scale()?;
        (snap.width > 0 && scale.is_finite() && scale > 0.0)
            .then_some(scale * PREVIEW_CONTENT_WIDTH / snap.width as f32)
    }

    fn to_logical(&self, position: Point<Pixels>, cx: &Context<Self>) -> Option<[f32; 2]> {
        let bounds = self.canvas_bounds.get()?;
        let zoom = self.preview_zoom(cx)?;
        let snap = self.controller.read(cx).scroll_snapshot()?;
        let scale = self.controller.read(cx).scroll_annotation_scale()?;
        let x = ((position.x - bounds.origin.x).as_f32() / zoom).max(0.0);
        let y = ((position.y - bounds.origin.y).as_f32() / zoom).max(0.0);
        Some([
            x.min(snap.width as f32 / scale),
            y.min(snap.height as f32 / scale),
        ])
    }

    fn on_canvas_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let Some(local) = self.to_logical(event.position, cx) else {
            return;
        };
        self.controller.update(cx, |controller, cx| {
            controller.begin_scroll_annotation(local[0], local[1], event.click_count >= 2, cx)
        });
        cx.stop_propagation();
    }

    fn on_canvas_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(local) = self.to_logical(event.position, cx) else {
            return;
        };
        self.controller.update(cx, |controller, cx| {
            controller.move_scroll_annotation(local[0], local[1], cx)
        });
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.controller
            .update(cx, |controller, cx| controller.end_scroll_annotation(cx));
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let typing = self
            .controller
            .read(cx)
            .scroll_annotations()
            .is_some_and(Layer::is_typing);
        let stroke = &event.keystroke;
        if !typing {
            if !stroke.modifiers.control
                && !stroke.modifiers.platform
                && !stroke.modifiers.function
                && !stroke.modifiers.alt
                && !stroke.modifiers.shift
                && matches!(stroke.key.as_str(), "backspace" | "delete")
            {
                self.controller
                    .update(cx, |controller, cx| controller.delete_scroll_annotation(cx));
                cx.stop_propagation();
            }
            return;
        }
        if stroke.modifiers.control || stroke.modifiers.platform || stroke.modifiers.function {
            return;
        }
        let handled = match stroke.key.as_str() {
            "backspace" | "delete" => {
                self.controller.update(cx, |controller, cx| {
                    controller.backspace_scroll_annotation(cx)
                });
                true
            }
            "enter" => {
                self.controller
                    .update(cx, |controller, cx| controller.commit_scroll_annotation(cx));
                true
            }
            "escape" => {
                self.controller.update(cx, |controller, cx| {
                    controller.cancel_scroll_annotation_draft(cx)
                });
                true
            }
            _ => {
                if let Some(text) = stroke.key_char.as_deref()
                    && !text.is_empty()
                    && text.chars().all(|ch| !ch.is_control())
                {
                    self.controller.update(cx, |controller, cx| {
                        controller.insert_scroll_annotation_text(text, cx)
                    });
                    true
                } else {
                    false
                }
            }
        };
        if handled {
            cx.stop_propagation();
        }
    }

    fn tool_button(
        &self,
        id: &'static str,
        icon: LucideIcon,
        label: String,
        tool: Tool,
        current: Tool,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .xsmall()
            .compact()
            .ghost()
            .icon(icon)
            .tooltip(label.clone())
            .accessibility_label(label)
            .selected(current == tool)
            .on_click(cx.listener(move |this, _, _, cx| {
                let same = this
                    .controller
                    .read(cx)
                    .scroll_annotations()
                    .is_some_and(|layer| layer.tool() == tool && layer.selected().is_none());
                this.style_open = if same { !this.style_open } else { true };
                this.controller.update(cx, |controller, cx| {
                    controller.set_scroll_annotation_tool(tool, cx)
                });
            }))
    }

    fn render_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(layer) = self.controller.read(cx).scroll_annotations() else {
            return Toolbar::new("scroll-preview-tools");
        };
        let current = layer.tool();
        let can_undo = layer.can_undo();
        Toolbar::new("scroll-preview-tools")
            .px_1()
            .child(self.tool_button(
                "live-rect",
                LucideIcon::Square,
                t!("overlay.tool.rect").to_string(),
                Tool::Rect,
                current,
                cx,
            ))
            .child(self.tool_button(
                "live-arrow",
                LucideIcon::ArrowUpRight,
                t!("overlay.tool.arrow").to_string(),
                Tool::Arrow,
                current,
                cx,
            ))
            .child(self.tool_button(
                "live-line",
                LucideIcon::Minus,
                t!("overlay.tool.line").to_string(),
                Tool::Line,
                current,
                cx,
            ))
            .child(self.tool_button(
                "live-pen",
                LucideIcon::Pencil,
                t!("overlay.tool.pen").to_string(),
                Tool::Pen,
                current,
                cx,
            ))
            .child(self.tool_button(
                "live-mosaic",
                LucideIcon::Grid3x3,
                t!("overlay.tool.mosaic").to_string(),
                Tool::Mosaic,
                current,
                cx,
            ))
            .child(self.tool_button(
                "live-text",
                LucideIcon::Type,
                t!("overlay.tool.text").to_string(),
                Tool::Text,
                current,
                cx,
            ))
            .child(self.tool_button(
                "live-step",
                LucideIcon::ListOrdered,
                t!("overlay.tool.step").to_string(),
                Tool::Step,
                current,
                cx,
            ))
            .content(div().w(px(1.)).h(px(14.)).bg(theme.border).mx_0p5())
            .child(
                Button::new("live-undo")
                    .xsmall()
                    .compact()
                    .ghost()
                    .icon(LucideIcon::Undo2)
                    .tooltip(t!("overlay.undo").to_string())
                    .accessibility_label(t!("overlay.undo").to_string())
                    .disabled(!can_undo)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller
                            .update(cx, |controller, cx| controller.undo_scroll_annotation(cx));
                    })),
            )
            .child(
                Button::new("live-clear")
                    .xsmall()
                    .compact()
                    .ghost()
                    .icon(LucideIcon::Eraser)
                    .tooltip(t!("overlay.clear").to_string())
                    .accessibility_label(t!("overlay.clear").to_string())
                    .disabled(!can_undo)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.style_open = false;
                        this.controller
                            .update(cx, |controller, cx| controller.clear_scroll_annotations(cx));
                    })),
            )
    }

    fn render_style_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(layer) = self.controller.read(cx).scroll_annotations() else {
            return div().flex_none();
        };
        let style = layer.style();
        let picked = layer.selected().is_some();
        let (min, max) = layer.size_range();
        let swatches = annotate::COLORS.into_iter().map(|color| {
            div()
                .id(SharedString::from(format!("live-color-{color}")))
                .size(px(14.))
                .rounded(px(3.))
                .bg(rgb(color))
                .when((style.color & 0x00FF_FFFF) == color, |this| {
                    this.border_2().border_color(theme.primary)
                })
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.controller.update(cx, |controller, cx| {
                        controller.set_scroll_annotation_color(color, cx);
                        controller.persist_scroll_annotation_defaults(cx);
                    });
                }))
        });
        h_flex()
            .flex_none()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.group_box)
            .when(layer.active_tool().uses_color(), |this| {
                this.children(swatches).child(
                    ColorPicker::new(&self.color_picker)
                        .with_size(px(14.))
                        .accessibility_label(t!("overlay.style").to_string())
                        .featured_colors(
                            annotate::COLORS
                                .into_iter()
                                .map(|color| Hsla::from(rgb(color)))
                                .collect(),
                        ),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("overlay.size").to_string()),
            )
            .child(
                Button::new("live-size-down")
                    .xsmall()
                    .compact()
                    .ghost()
                    .icon(LucideIcon::Minus)
                    .disabled(style.size <= min as f32)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.controller.update(cx, |controller, cx| {
                            let size = controller
                                .scroll_annotations()
                                .map_or(style.size, |layer| layer.style().size - 1.0);
                            controller.set_scroll_annotation_size(size, cx);
                            controller.persist_scroll_annotation_defaults(cx);
                        });
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("overlay.size_value", value = style.size.round() as u32).to_string()),
            )
            .child(
                Button::new("live-size-up")
                    .xsmall()
                    .compact()
                    .ghost()
                    .icon(LucideIcon::Plus)
                    .disabled(style.size >= max as f32)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.controller.update(cx, |controller, cx| {
                            let size = controller
                                .scroll_annotations()
                                .map_or(style.size, |layer| layer.style().size + 1.0);
                            controller.set_scroll_annotation_size(size, cx);
                            controller.persist_scroll_annotation_defaults(cx);
                        });
                    })),
            )
            .when(picked, |this| {
                this.child(
                    Button::new("live-delete")
                        .xsmall()
                        .compact()
                        .ghost()
                        .icon(LucideIcon::Trash)
                        .tooltip(t!("overlay.delete").to_string())
                        .accessibility_label(t!("overlay.delete").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.style_open = false;
                            this.controller.update(cx, |controller, cx| {
                                controller.delete_scroll_annotation(cx)
                            });
                        })),
                )
            })
    }
}

impl Render for ScrollPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.controller.read(cx).has_scroll() {
            return div().id("scroll-preview").size_full();
        }
        self.sync_tile(cx, window);
        let controller = self.controller.read(cx);
        let snap = controller.scroll_snapshot();
        let layer = controller.scroll_annotations().cloned();
        let scale = controller.scroll_annotation_scale().unwrap_or(1.0);
        let dims = snap
            .as_ref()
            .filter(|snapshot| snapshot.started)
            .map(|snapshot| format!("{} × {}", snapshot.width, snapshot.height))
            .unwrap_or_else(|| t!("scroll.preview_title").to_string());
        let show_style = self.style_open
            || layer
                .as_ref()
                .is_some_and(|annotations| annotations.selected().is_some());
        let theme = cx.theme().clone();
        let image_width = snap.as_ref().map_or(1, |snapshot| snapshot.width.max(1));
        let image_height = snap.as_ref().map_or(1, |snapshot| snapshot.height.max(1));
        let preview_range = snap
            .as_ref()
            .map_or((0, 1), |snapshot| snapshot.preview_range);
        let tile = self.tile.clone();
        let canvas_bounds = self.canvas_bounds.clone();
        let mosaics = self.mosaics.clone();
        let scroll_handle = self.scroll_handle.clone();
        let focus_handle = self.focus_handle.clone();
        let view_entity = cx.entity();
        let input_view = view_entity.clone();
        let zoom = scale * PREVIEW_CONTENT_WIDTH / image_width as f32;

        v_flex()
            .id("scroll-preview")
            .key_context(PREVIEW_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .overflow_hidden()
            .on_action(cx.listener(|this, _: &UndoAnnotation, _, cx| {
                this.controller
                    .update(cx, |controller, cx| controller.undo_scroll_annotation(cx));
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
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
            .when(layer.is_some(), |this| this.child(self.render_tools(cx)))
            .when(show_style, |this| this.child(self.render_style_strip(cx)))
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
                            .child(
                                div()
                                    .id("scroll-preview-canvas")
                                    .w_full()
                                    .h(px(image_height as f32 * PREVIEW_CONTENT_WIDTH
                                        / image_width as f32))
                                    .relative()
                                    .cursor(CursorStyle::Crosshair)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(Self::on_canvas_mouse_down),
                                    )
                                    .on_mouse_move(cx.listener(Self::on_canvas_mouse_move))
                                    .when_some(tile.as_ref(), |content, tile| {
                                        content.child(
                                            img(tile.render.clone())
                                                .id(SharedString::from(format!(
                                                    "scroll-preview-{}",
                                                    tile.revision
                                                )))
                                                .absolute()
                                                .left_0()
                                                .top(px(preview_range.0 as f32
                                                    * PREVIEW_CONTENT_WIDTH
                                                    / image_width as f32))
                                                .w_full()
                                                .h(px(tile.height as f32 * PREVIEW_CONTENT_WIDTH
                                                    / tile.width.max(1) as f32))
                                                .object_fit(ObjectFit::Fill),
                                        )
                                    })
                                    .child(
                                        div().absolute().inset_0().overflow_hidden().child(
                                            canvas(
                                                move |bounds, _, _| {
                                                    canvas_bounds.set(Some(bounds));
                                                },
                                                move |bounds, _, window, cx| {
                                                    let Some(layer) = layer.as_ref() else {
                                                        return;
                                                    };
                                                    let viewport = scroll_handle.bounds();
                                                    let visible_top =
                                                        (viewport.top() - bounds.top()).as_f32();
                                                    let clip = [
                                                        0.0,
                                                        visible_top / zoom,
                                                        image_width as f32 / scale,
                                                        (visible_top
                                                            + viewport.size.height.as_f32())
                                                            / zoom,
                                                    ];
                                                    let mut guard = mosaics.lock().ok();
                                                    let mut fallback =
                                                        annotate::MosaicCache::default();
                                                    let cache = guard
                                                        .as_deref_mut()
                                                        .unwrap_or(&mut fallback);
                                                    let source = tile
                                                        .as_ref()
                                                        .filter(|_| layer.needs_source())
                                                        .map(|tile| {
                                                            annotate::MosaicSource::source_at_with_ratio(
                                                                &tile.image,
                                                                tile.revision,
                                                                [
                                                                    0.0,
                                                                    preview_range.0 as f32 / scale,
                                                                ],
                                                                tile.width as f32 * scale
                                                                    / image_width as f32,
                                                            )
                                                        });
                                                    if source.is_none() {
                                                        cache.clear();
                                                    }
                                                    annotate::paint_layer_zoomed_clipped(
                                                        layer,
                                                        bounds.origin,
                                                        scale,
                                                        zoom,
                                                        clip,
                                                        source,
                                                        cache,
                                                        window,
                                                        cx,
                                                    );
                                                    if layer.is_typing() {
                                                        window.handle_input(
                                                            &focus_handle,
                                                            ElementInputHandler::new(
                                                                bounds,
                                                                input_view.clone(),
                                                            ),
                                                            cx,
                                                        );
                                                    }
                                                },
                                            )
                                            .size_full(),
                                        ),
                                    ),
                            ),
                    )
                    .vertical_scrollbar(&self.scroll_handle),
            )
    }
}

impl EntityInputHandler for ScrollPreview {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let (text, range) = self
            .controller
            .read(cx)
            .scroll_annotations()?
            .text_for_range(range_utf16)?;
        *actual_range = Some(range);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self
            .controller
            .read(cx)
            .scroll_annotations()?
            .text_length_utf16()?;
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.controller
            .read(cx)
            .scroll_annotations()?
            .marked_text_range()
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.controller.update(cx, |controller, cx| {
            controller.unmark_scroll_annotation_text(cx)
        });
        window.notify_text_input_changed(&self.focus_handle);
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.controller.update(cx, |controller, cx| {
            controller.replace_scroll_annotation_text(range_utf16, text, cx)
        });
        window.notify_text_input_changed(&self.focus_handle);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.controller.update(cx, |controller, cx| {
            controller.replace_and_mark_scroll_annotation_text(range_utf16, new_text, cx)
        });
        window.notify_text_input_changed(&self.focus_handle);
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let bounds = self.canvas_bounds.get()?;
        let zoom = self.preview_zoom(cx)?;
        let at = self
            .controller
            .read(cx)
            .scroll_annotations()?
            .draft_origin()?;
        let font_size = self
            .controller
            .read(cx)
            .scroll_annotations()?
            .draft_style()
            .map_or(16.0, |style| style.size)
            * zoom;
        Some(Bounds::new(
            point(
                bounds.origin.x + px(at[0] * zoom),
                bounds.origin.y + px(at[1] * zoom),
            ),
            size(px(1.), px(font_size)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

fn color_to_rgb(color: Hsla) -> u32 {
    let color = color.to_rgb();
    ((color.r * 255.0).round() as u32) << 16
        | ((color.g * 255.0).round() as u32) << 8
        | (color.b * 255.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downward_growth_only_follows_when_the_view_was_at_the_bottom() {
        assert_eq!(
            anchored_preview_offset(800.0, 1000, 1200, 0, ScrollDirection::Down, 200.0, 1.0),
            1000.0
        );
        assert_eq!(
            anchored_preview_offset(300.0, 1000, 1200, 0, ScrollDirection::Down, 200.0, 1.0),
            300.0
        );
    }

    #[test]
    fn upward_growth_preserves_the_viewed_content() {
        assert_eq!(
            anchored_preview_offset(0.0, 1000, 1200, 200, ScrollDirection::Up, 200.0, 1.0),
            0.0
        );
        assert_eq!(
            anchored_preview_offset(300.0, 1000, 1200, 200, ScrollDirection::Up, 200.0, 1.0),
            500.0
        );
    }
}
