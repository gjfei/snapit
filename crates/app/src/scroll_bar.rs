//! Floating control bar and incremental live preview strip shown while a
//! scrolling screenshot is in progress.

use std::{
    cell::Cell,
    rc::Rc,
    sync::{Arc, Mutex},
};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    toolbar::Toolbar,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, Entity, FocusHandle,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, MouseDownEvent, ObjectFit,
    ParentElement as _, Pixels, Render, RenderImage, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, Window, actions,
    canvas, div, img, point, prelude::FluentBuilder as _, px, size,
};
use snapit_capture::RgbaImage;
use snapit_scroll::{ScrollDirection, ScrollSnapshot, ScrollStatus, ScrollWarning};

use crate::annotate::{self, Tool};
use crate::capture_toolbar::{self, ExportActionLabels};
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
    ]
);

pub const SCROLL_BAR_SIZE: (f32, f32) = (586., 36.);
const SCROLL_BAR_WARNING_HEIGHT: f32 = 28.;
pub const SCROLL_BAR_EXPANDED_HEIGHT: f32 = SCROLL_BAR_SIZE.1 + SCROLL_BAR_WARNING_HEIGHT;
pub const PREVIEW_WINDOW_WIDTH: f32 = 280.;
const CONTEXT: &str = "ScrollBar";
const PREVIEW_CONTENT_WIDTH: f32 = PREVIEW_WINDOW_WIDTH - 16.;
const PREVIEW_CHROME_HEIGHT: f32 = 48.;
const PREVIEW_AREA_PADDING: f32 = 16.;

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CancelScroll, Some(CONTEXT)),
        KeyBinding::new("enter", FinishCopy, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("s"), FinishSave, Some(CONTEXT)),
        KeyBinding::new("e", FinishEdit, Some(CONTEXT)),
        KeyBinding::new("space", ToggleAutoScroll, Some(CONTEXT)),
    ]);
}

pub fn scroll_bar_bounds(
    display_bounds: Bounds<Pixels>,
    selection: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let bar = size(px(SCROLL_BAR_SIZE.0), px(SCROLL_BAR_SIZE.1));
    let gap = px(8.);
    let bottom = selection.origin.y + selection.size.height;
    let right = selection.origin.x + selection.size.width;
    let aligned_x = if right >= bar.width {
        right - bar.width
    } else {
        selection.origin.x
    }
    .max(px(0.))
    .min((display_bounds.size.width - bar.width).max(px(0.)));
    let expanded_h = px(SCROLL_BAR_EXPANDED_HEIGHT);
    let y = if bottom + gap + expanded_h <= display_bounds.size.height {
        bottom + gap
    } else if selection.origin.y >= bar.height + gap {
        selection.origin.y - gap - bar.height
    } else {
        (bottom - gap - expanded_h).max(px(0.))
    }
    .min((display_bounds.size.height - expanded_h).max(px(0.)));
    Bounds::new(display_bounds.origin + point(aligned_x, y), bar)
}

pub fn scroll_preview_bounds(
    display_bounds: Bounds<Pixels>,
    selection: Bounds<Pixels>,
    bar: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let gap = px(10.);
    let margin = px(16.);
    let w = px(PREVIEW_WINDOW_WIDTH);
    let max_h = (display_bounds.size.height - margin * 2.).max(px(220.));
    let content_h = PREVIEW_CONTENT_WIDTH * selection.size.height.as_f32()
        / selection.size.width.as_f32().max(1.0);
    let h = (px(content_h + PREVIEW_CHROME_HEIGHT))
        .max(px(220.))
        .min(max_h);
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

    let mut bounds = Bounds::new(display_bounds.origin + point(x, y), size(w, h));
    if !bounds.intersects(&bar) {
        return bounds;
    }
    let display_top = display_bounds.origin.y;
    let display_bottom = display_top + display_bounds.size.height;
    let above = bar.origin.y - gap - display_top;
    let below = display_bottom - (bar.origin.y + bar.size.height) - gap;
    if above >= px(220.) && above >= below {
        bounds.size.height = bounds.size.height.min(above - margin);
        bounds.origin.y = display_top
            + (bounds.origin.y - display_top)
                .max(margin)
                .min(above - bounds.size.height);
    } else if below >= px(220.) {
        bounds.origin.y = bar.origin.y + bar.size.height + gap;
        bounds.size.height = bounds.size.height.min(below - margin);
    } else if bar.origin.x - display_bounds.origin.x >= gap + w {
        bounds.origin.x = bar.origin.x - gap - w;
    } else {
        bounds.origin.x = (bar.origin.x + bar.size.width + gap)
            .min(display_bounds.origin.x + display_bounds.size.width - w)
            .max(display_bounds.origin.x);
    }
    bounds
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollStatusTone {
    Active,
    Success,
    Warning,
}

struct ScrollStatusInfo {
    label: String,
    tone: ScrollStatusTone,
    warning: Option<String>,
}

fn scroll_status_info(snap: &ScrollSnapshot) -> ScrollStatusInfo {
    let warning = snap.warning.map(|warn| match warn {
        ScrollWarning::TooFast => t!("scroll.warning.too_fast").to_string(),
        ScrollWarning::LowTexture => t!("scroll.warning.low_texture").to_string(),
    });
    let tone = if snap.warning.is_some() || !snap.started || snap.status == ScrollStatus::Failed {
        ScrollStatusTone::Warning
    } else if snap.status == ScrollStatus::ReachedEnd {
        ScrollStatusTone::Success
    } else {
        ScrollStatusTone::Active
    };
    let state_label = if !snap.started {
        t!("scroll.starting").to_string()
    } else {
        match snap.status {
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
            ScrollStatus::StoppedByUser => t!("scroll.stopped_by_user").to_string(),
            ScrollStatus::Failed => t!("scroll.failed").to_string(),
        }
    };
    let label = warning.clone().unwrap_or(state_label);
    ScrollStatusInfo {
        label,
        tone,
        warning,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ScrollBarDisplayState {
    started: bool,
    finishing: bool,
    status: ScrollStatus,
    warning: Option<ScrollWarning>,
    auto_scrolling: bool,
    auto_direction: ScrollDirection,
    preview_visible: bool,
    can_undo: bool,
}

impl ScrollBarDisplayState {
    fn from_controller(controller: &Controller) -> Option<Self> {
        let snap = controller.scroll_snapshot()?;
        Some(Self {
            started: snap.started,
            finishing: controller.is_finishing_scroll(),
            status: snap.status,
            warning: snap.warning,
            auto_scrolling: snap.auto_scrolling,
            auto_direction: snap.auto_direction,
            preview_visible: controller.is_scroll_preview_visible(),
            can_undo: controller
                .scroll_annotations()
                .is_some_and(|layer| layer.can_undo()),
        })
    }
}

pub struct ScrollBar {
    controller: Entity<Controller>,
    focus_handle: FocusHandle,
    warning_expanded: bool,
    display_state: Option<ScrollBarDisplayState>,
    _subscription: Subscription,
}

fn drag_window(window: &mut Window) {
    #[cfg(target_os = "macos")]
    window.start_window_move();
    #[cfg(target_os = "windows")]
    crate::recording_bar::native_drag(window);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = window;
}

/// Unified scrolling-capture control bar card shared by the floating
/// `ScrollBar` window and the in-place `OverlayView` annotation state.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_scroll_bar_card(
    controller: Entity<Controller>,
    snap: &ScrollSnapshot,
    active_tool: Option<Tool>,
    preview_visible: bool,
    can_act: bool,
    can_export: bool,
    can_undo: bool,
    id_prefix: &'static str,
    on_select_tool: impl Fn(Tool, &ClickEvent, &mut Window, &mut App) + 'static,
    on_drag: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    extra_bottom_row: Option<AnyElement>,
    theme: &Theme,
) -> Div {
    let info = scroll_status_info(snap);
    let dot_color = if active_tool.is_some() {
        theme.primary
    } else {
        match info.tone {
            ScrollStatusTone::Active => theme.primary,
            ScrollStatusTone::Success => theme.success,
            ScrollStatusTone::Warning => theme.warning,
        }
    };
    let auto_up_hint = t!("scroll.auto_up_hint").to_string();
    let auto_down_hint = t!("scroll.auto_down_hint").to_string();
    let preview_hint = t!("scroll.preview_toggle").to_string();
    let reset_hint = t!("scroll.reset").to_string();
    let edit_hint = t!("scroll.edit").to_string();

    let undo_ctrl = controller.clone();
    let clear_ctrl = controller.clone();
    let auto_up_ctrl = controller.clone();
    let auto_down_ctrl = controller.clone();
    let preview_ctrl = controller.clone();
    let reset_ctrl = controller.clone();
    let edit_ctrl = controller.clone();
    let ocr_ctrl = controller.clone();
    let pin_ctrl = controller.clone();
    let save_ctrl = controller.clone();
    let cancel_ctrl = controller.clone();
    let copy_ctrl = controller;

    let actions_toolbar = capture_toolbar::append_export_actions(
        capture_toolbar::append_annotation_tools(
            Toolbar::new(SharedString::from(format!("{id_prefix}-actions"))).p_0(),
            id_prefix,
            active_tool,
            can_undo,
            !can_export,
            theme,
            on_select_tool,
            move |_, _, cx| {
                undo_ctrl.update(cx, |controller, cx| controller.undo_scroll_annotation(cx));
            },
            move |_, _, cx| {
                clear_ctrl.update(cx, |controller, cx| controller.clear_scroll_annotations(cx));
            },
        )
        .content(capture_toolbar::toolbar_divider(theme))
        .child(
            capture_toolbar::toolbar_button(
                SharedString::from(format!("{id_prefix}-auto-up")),
                LucideIcon::ArrowUp,
                auto_up_hint,
                snap.auto_scrolling && snap.auto_direction == ScrollDirection::Up,
                !can_export,
            )
            .on_click(move |_, _, cx| {
                auto_up_ctrl.update(cx, |controller, cx| {
                    controller.set_scroll_auto_direction(ScrollDirection::Up, cx)
                });
            }),
        )
        .child(
            capture_toolbar::toolbar_button(
                SharedString::from(format!("{id_prefix}-auto-down")),
                LucideIcon::ArrowDown,
                auto_down_hint,
                snap.auto_scrolling && snap.auto_direction == ScrollDirection::Down,
                !can_export,
            )
            .on_click(move |_, _, cx| {
                auto_down_ctrl.update(cx, |controller, cx| {
                    controller.set_scroll_auto_direction(ScrollDirection::Down, cx)
                });
            }),
        )
        .child(
            capture_toolbar::toolbar_button(
                SharedString::from(format!("{id_prefix}-preview-toggle")),
                LucideIcon::PanelRight,
                preview_hint,
                preview_visible,
                !can_act,
            )
            .on_click(move |_, _, cx| {
                preview_ctrl.update(cx, |controller, cx| controller.toggle_scroll_preview(cx));
            }),
        )
        .child(
            capture_toolbar::toolbar_button(
                SharedString::from(format!("{id_prefix}-reset")),
                LucideIcon::RotateCcw,
                reset_hint,
                false,
                !can_export,
            )
            .on_click(move |_, _, cx| {
                reset_ctrl.update(cx, |controller, cx| controller.reset_scroll(cx));
            }),
        )
        .child(
            capture_toolbar::toolbar_button(
                SharedString::from(format!("{id_prefix}-edit")),
                LucideIcon::Crop,
                edit_hint,
                false,
                !can_export,
            )
            .on_click(move |_, _, cx| {
                edit_ctrl.update(cx, |controller, cx| {
                    controller.finish_scroll(ScrollFinishAction::Edit, cx)
                });
            }),
        )
        .content(capture_toolbar::toolbar_divider(theme)),
        id_prefix,
        ExportActionLabels::scroll(),
        !can_export,
        !can_act,
        move |_, _, cx| {
            ocr_ctrl.update(cx, |controller, cx| {
                controller.finish_scroll(ScrollFinishAction::Recognize, cx)
            });
        },
        move |_, _, cx| {
            pin_ctrl.update(cx, |controller, cx| {
                controller.finish_scroll(ScrollFinishAction::Pin, cx)
            });
        },
        move |_, _, cx| {
            save_ctrl.update(cx, |controller, cx| {
                controller.finish_scroll(ScrollFinishAction::Save, cx)
            });
        },
        move |_, _, cx| {
            cancel_ctrl.update(cx, |controller, cx| controller.cancel_scroll(cx));
        },
        move |_, _, cx| {
            copy_ctrl.update(cx, |controller, cx| {
                controller.finish_scroll(ScrollFinishAction::Copy, cx)
            });
        },
    );

    v_flex()
        .w(px(SCROLL_BAR_SIZE.0))
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .text_color(theme.popover_foreground)
        .child(
            h_flex()
                .w_full()
                .h(px(SCROLL_BAR_SIZE.1 - 2.))
                .px_1()
                .items_center()
                .justify_between()
                .on_mouse_down(MouseButton::Left, on_drag)
                .child(
                    h_flex()
                        .flex_none()
                        .h_full()
                        .pl_1p5()
                        .pr_1p5()
                        .items_center()
                        .justify_center()
                        .child(div().size_2().flex_none().rounded_full().bg(dot_color)),
                )
                .child(
                    div()
                        .flex_none()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(actions_toolbar),
                ),
        )
        .when_some(extra_bottom_row, |this, row| this.child(row))
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
        let initial_state = ScrollBarDisplayState::from_controller(controller.read(cx));
        let subscription = cx.observe(&controller, |this, controller, cx| {
            let next_state = ScrollBarDisplayState::from_controller(controller.read(cx));
            if next_state != this.display_state {
                this.display_state = next_state;
                cx.notify();
            }
        });
        Self {
            controller,
            focus_handle,
            warning_expanded: false,
            display_state: initial_state,
            _subscription: subscription,
        }
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.read(cx);
        if !controller.has_scroll() {
            return div().id("scroll-bar").size_full();
        }
        let Some(snap) = controller.scroll_snapshot() else {
            return div().id("scroll-bar").size_full();
        };
        let finishing = controller.is_finishing_scroll();
        let preview_visible = controller.is_scroll_preview_visible();
        let can_undo = controller
            .scroll_annotations()
            .is_some_and(|layer| layer.can_undo());
        let started = snap.started;
        let can_act = !finishing;
        let can_export = started && !finishing;
        let info = scroll_status_info(&snap);
        let show_warning = !preview_visible && info.warning.is_some();
        if self.warning_expanded != show_warning {
            self.warning_expanded = show_warning;
            let target_h = if show_warning {
                SCROLL_BAR_SIZE.1 + SCROLL_BAR_WARNING_HEIGHT
            } else {
                SCROLL_BAR_SIZE.1
            };
            window.resize(size(px(SCROLL_BAR_SIZE.0), px(target_h)));
        }

        let theme = cx.theme().clone();
        let select_controller = self.controller.clone();
        let warning_row = info.warning.filter(|_| show_warning).map(|warning| {
            h_flex()
                .w_full()
                .h(px(SCROLL_BAR_WARNING_HEIGHT))
                .px_2p5()
                .gap_1p5()
                .items_center()
                .border_t_1()
                .border_color(theme.warning.opacity(0.45))
                .bg(theme.warning.opacity(0.14))
                .on_mouse_down(MouseButton::Left, |_, window, _| drag_window(window))
                .child(div().size_2().flex_none().rounded_full().bg(theme.warning))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(theme.foreground)
                        .truncate()
                        .child(warning),
                )
                .into_any_element()
        });

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
                render_scroll_bar_card(
                    self.controller.clone(),
                    &snap,
                    None,
                    preview_visible,
                    can_act,
                    can_export,
                    can_undo,
                    "scroll-bar",
                    move |tool, _, _, cx| {
                        select_controller.update(cx, |controller, cx| {
                            controller.start_scroll_annotation_with_tool(tool, cx)
                        });
                    },
                    |_, window, _| drag_window(window),
                    warning_row,
                    &theme,
                )
                .size_full()
                .overflow_hidden(),
            )
    }
}

#[derive(Clone)]
struct CachedPreviewTile {
    revision: u64,
    source_start: u32,
    source_rows: u32,
    total_height: u32,
    image: Arc<RgbaImage>,
    render: Arc<RenderImage>,
    width: u32,
}

fn preview_display_height(tile: Option<&CachedPreviewTile>, snap_height: u32) -> u32 {
    tile.map_or(snap_height, |tile| {
        tile.total_height
            .max(tile.source_start.saturating_add(tile.source_rows))
    })
    .min(snap_height)
    .max(1)
}

fn desired_preview_request(
    follow_live: bool,
    target_top: f32,
    viewport_height: f32,
    logical_per_source: f32,
    display_height: u32,
) -> (u32, u32, bool) {
    if follow_live {
        let viewport_rows = (viewport_height / logical_per_source).ceil().max(1.0) as u32;
        (0, viewport_rows.saturating_mul(3).max(1), true)
    } else {
        let (start, rows) =
            preview_tile_range(target_top, viewport_height, logical_per_source, display_height);
        (start, rows, false)
    }
}

fn scroll_top(handle: &ScrollHandle) -> f32 {
    (-handle.offset().y.as_f32()).max(0.0)
}

fn set_scroll_top(handle: &ScrollHandle, top: f32) {
    handle.set_offset(point(px(0.), px(-top.max(0.0))));
}

fn synced_preview_offset(
    viewport_origin_y: i64,
    frame_height: u32,
    total_height: u32,
    direction: ScrollDirection,
    viewport_height: f32,
    logical_per_source: f32,
) -> f32 {
    let max_top = (total_height as f32 * logical_per_source - viewport_height).max(0.0);
    if max_top <= 0.0 {
        return 0.0;
    }
    let vp_top = viewport_origin_y.clamp(0, total_height as i64) as f32;
    let vp_bottom = (vp_top + frame_height as f32).min(total_height as f32);
    match direction {
        ScrollDirection::Down => {
            (vp_bottom * logical_per_source - viewport_height).clamp(0.0, max_top)
        }
        ScrollDirection::Up => (vp_top * logical_per_source).clamp(0.0, max_top),
    }
}

fn anchored_preview_offset(
    offset: f32,
    old_height: u32,
    new_height: u32,
    content_shift_delta: i64,
    direction: ScrollDirection,
    viewport_height: f32,
    logical_per_source: f32,
) -> f32 {
    if old_height == 0 {
        return 0.0;
    }
    let old_max = (old_height as f32 * logical_per_source - viewport_height).max(0.0);
    let new_max = (new_height as f32 * logical_per_source - viewport_height).max(0.0);
    let offset = offset.clamp(0.0, old_max);
    let target = offset + content_shift_delta as f32 * logical_per_source;
    let target = match direction {
        ScrollDirection::Up if offset <= 1.0 => 0.0,
        ScrollDirection::Down if offset >= old_max - 1.0 => new_max,
        _ => target,
    };
    target.clamp(0.0, new_max)
}

fn preview_tile_range(
    offset: f32,
    viewport_height: f32,
    logical_per_source: f32,
    height: u32,
) -> (u32, u32) {
    if height == 0 {
        return (0, 1);
    }
    let viewport_rows = (viewport_height / logical_per_source).ceil().max(1.0) as u32;
    let first = (offset / logical_per_source).max(0.0) as u32;
    let start = first
        .saturating_sub(viewport_rows / 2)
        .min(height.saturating_sub(1));
    let rows = viewport_rows.saturating_mul(3).min(height - start).max(1);
    (start, rows)
}

pub struct ScrollPreview {
    controller: Entity<Controller>,
    tile: Option<CachedPreviewTile>,
    last_revision: u64,
    last_height: u32,
    display_height: u32,
    last_content_shift: u32,
    last_frames: usize,
    last_viewport_origin_y: i64,
    follow_live: bool,
    last_applied_top: f32,
    last_seen_scroll_top: Rc<Cell<f32>>,
    requested_range: (u32, u32, bool),
    scroll_handle: ScrollHandle,
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    viewport_height: Rc<Cell<Option<f32>>>,
    mosaics: Arc<Mutex<annotate::MosaicCache>>,
    _subscription: Subscription,
}

impl ScrollPreview {
    pub fn new(
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        exclude_from_capture(window);
        let _subscription = cx.observe(&controller, |_, _, cx| cx.notify());
        Self {
            controller,
            tile: None,
            last_revision: 0,
            last_height: 0,
            display_height: 1,
            last_content_shift: 0,
            last_frames: 0,
            last_viewport_origin_y: 0,
            follow_live: true,
            last_applied_top: 0.0,
            last_seen_scroll_top: Rc::new(Cell::new(0.0)),
            requested_range: (0, 1, true),
            scroll_handle: ScrollHandle::new(),
            canvas_bounds: Rc::new(Cell::new(None)),
            viewport_height: Rc::new(Cell::new(None)),
            mosaics: Arc::new(Mutex::new(annotate::MosaicCache::default())),
            _subscription,
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

    fn content_width(&self) -> f32 {
        self.canvas_bounds
            .get()
            .map(|bounds| bounds.size.width.as_f32())
            .filter(|width| width.is_finite() && *width > 0.0)
            .unwrap_or(PREVIEW_CONTENT_WIDTH)
    }

    fn sync_tile(&mut self, cx: &mut Context<Self>, window: &mut Window) {
        let Some(snap) = self.controller.read(cx).scroll_snapshot() else {
            return;
        };
        if snap.height == 0 || snap.width == 0 {
            return;
        }
        let reset = snap.frames < self.last_frames;
        if reset {
            self.tile = None;
            self.last_revision = 0;
            self.last_height = 0;
            self.display_height = 1;
            self.follow_live = true;
        }
        if snap.preview_revision != self.last_revision
            && let Some(preview) = &snap.preview
        {
            let (width, _) = preview.image.dimensions();
            self.tile = Some(CachedPreviewTile {
                revision: preview.revision,
                source_start: preview.source_start,
                source_rows: preview.source_rows,
                total_height: preview.total_height,
                image: preview.image.clone(),
                render: session::render_image(&preview.image),
                width,
            });
            self.last_revision = snap.preview_revision;
        }

        let display_height = preview_display_height(self.tile.as_ref(), snap.height);
        let content_w = self.content_width();
        let logical_per_source = content_w / snap.width as f32;
        let outer_viewport_h = self
            .viewport_height
            .get()
            .filter(|height| height.is_finite() && *height > 1.0)
            .unwrap_or_else(|| {
                (window.viewport_size().height.as_f32()
                    - (PREVIEW_CHROME_HEIGHT - PREVIEW_AREA_PADDING))
                    .max(1.0)
            });
        let inner_viewport_h = (outer_viewport_h - PREVIEW_AREA_PADDING).max(1.0);
        let current_top = scroll_top(&self.scroll_handle);
        if self.last_height > 0 && (current_top - self.last_applied_top).abs() > 3.0 {
            let old_max =
                (self.last_height as f32 * logical_per_source - inner_viewport_h).max(0.0);
            self.follow_live = match snap.direction {
                ScrollDirection::Down => current_top >= old_max - 4.0,
                ScrollDirection::Up => current_top <= 4.0,
            };
        }

        let target_top = if reset {
            0.0
        } else if self.follow_live {
            synced_preview_offset(
                snap.viewport_origin_y,
                snap.frame_height.max(1),
                display_height,
                snap.direction,
                inner_viewport_h,
                logical_per_source,
            )
        } else {
            anchored_preview_offset(
                current_top,
                self.last_height,
                display_height,
                i64::from(snap.content_shift) - i64::from(self.last_content_shift),
                snap.direction,
                inner_viewport_h,
                logical_per_source,
            )
        };
        if reset || (target_top - current_top).abs() > 0.5 {
            set_scroll_top(&self.scroll_handle, target_top);
        }
        self.last_applied_top = target_top;
        self.last_seen_scroll_top.set(target_top);
        self.last_height = display_height;
        self.display_height = display_height;
        self.last_content_shift = snap.content_shift;
        self.last_frames = snap.frames;
        self.last_viewport_origin_y = snap.viewport_origin_y;

        let next_request = desired_preview_request(
            self.follow_live,
            target_top,
            inner_viewport_h,
            logical_per_source,
            display_height,
        );
        if reset || self.requested_range != next_request {
            self.requested_range = next_request;
            self.controller.update(cx, |controller, _| {
                controller.set_scroll_preview_range_with_follow(
                    next_request.0,
                    next_request.1,
                    next_request.2,
                )
            });
        }
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
            .map(|snapshot| format!("{} × {}", snapshot.width, snapshot.height));
        let is_annotating = controller.is_scroll_annotating();
        let status_info = snap.as_ref().map(|snapshot| {
            let mut info = scroll_status_info(snapshot);
            if is_annotating && info.warning.is_none() {
                info.label = t!("scroll.annotating").to_string();
                info.tone = ScrollStatusTone::Active;
            }
            let highlight_warning =
                info.warning.is_some() || snapshot.status == ScrollStatus::Failed;
            (info, highlight_warning)
        });
        let theme = cx.theme().clone();
        let highlight_warning = status_info.as_ref().is_some_and(|(_, warning)| *warning);
        let dot_color = status_info.as_ref().map(|(info, _)| match info.tone {
            ScrollStatusTone::Active => theme.primary,
            ScrollStatusTone::Success => theme.success,
            ScrollStatusTone::Warning => theme.warning,
        });
        let prompt = status_info
            .as_ref()
            .map(|(info, _)| info.label.clone())
            .unwrap_or_else(|| t!("scroll.preview_title").to_string());
        let image_width = snap.as_ref().map_or(1, |snapshot| snapshot.width.max(1));
        let image_height = if self.tile.is_some() {
            self.display_height
        } else {
            snap.as_ref().map_or(1, |snapshot| snapshot.height.max(1))
        };
        let tile = self.tile.clone();
        let canvas_bounds = self.canvas_bounds.clone();
        let viewport_height = self.viewport_height.clone();
        let last_seen_scroll_top = self.last_seen_scroll_top.clone();
        let prepaint_view = cx.entity();
        let mosaics = self.mosaics.clone();
        let scroll_handle = self.scroll_handle.clone();
        let viewport_scroll_handle = scroll_handle.clone();
        let content_w = self.content_width();
        let zoom = scale * content_w / image_width as f32;

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
                    .gap_1p5()
                    .items_center()
                    .border_b_1()
                    .when(highlight_warning, |this| {
                        this.border_color(theme.warning.opacity(0.45))
                            .bg(theme.warning.opacity(0.14))
                    })
                    .when(!highlight_warning, |this| {
                        this.border_color(theme.border)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, _| this.drag(window)),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .items_center()
                            .when_some(dot_color, |this, color| {
                                this.child(div().size_2().flex_none().rounded_full().bg(color))
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(if highlight_warning {
                                        theme.foreground
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .truncate()
                                    .child(prompt),
                            )
                            .when_some(dims, |this, dims| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(dims),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
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
                    .child(
                        div()
                            .id("scroll-preview-area")
                            .size_full()
                            .p_2()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .child(
                                div()
                                    .id("scroll-preview-canvas")
                                    .w_full()
                                    .h(px(image_height as f32 * content_w
                                        / image_width as f32))
                                    .relative()
                                    .when_some(tile.as_ref(), |content, tile| {
                                        content.child(
                                            img(tile.render.clone())
                                                .id("scroll-preview-tile")
                                                .absolute()
                                                .left_0()
                                                .top(px(tile.source_start as f32
                                                    * content_w
                                                    / image_width as f32))
                                                .w_full()
                                                .h(px(tile.source_rows as f32
                                                    * content_w
                                                    / image_width as f32))
                                                .object_fit(ObjectFit::Fill),
                                        )
                                    })
                                    .child(
                                        div().absolute().inset_0().overflow_hidden().child(
                                            canvas(
                                                move |bounds, _, cx| {
                                                    canvas_bounds.set(Some(bounds));
                                                    let new_vp_h = viewport_scroll_handle
                                                        .bounds()
                                                        .size
                                                        .height
                                                        .as_f32();
                                                    let prev_vp_h = viewport_height.get();
                                                    let vp_changed = new_vp_h.is_finite()
                                                        && new_vp_h > 1.0
                                                        && prev_vp_h.is_none_or(|old| {
                                                            (old - new_vp_h).abs() > 1.0
                                                        });
                                                    viewport_height.set(Some(new_vp_h));
                                                    let live_top = (-viewport_scroll_handle
                                                        .offset()
                                                        .y
                                                        .as_f32())
                                                    .max(0.0);
                                                    let scroll_changed =
                                                        (live_top - last_seen_scroll_top.get())
                                                            .abs()
                                                            > 2.0;
                                                    if vp_changed || scroll_changed {
                                                        last_seen_scroll_top.set(live_top);
                                                        let view = prepaint_view.clone();
                                                        cx.defer(move |cx| {
                                                            view.update(cx, |_, cx| cx.notify());
                                                        });
                                                    }
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
                                                                    tile.source_start as f32 / scale,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_range_uses_the_same_width_scale_on_both_axes() {
        assert_eq!(preview_tile_range(0.0, 100.0, 0.5, 2280), (0, 600));
        assert_eq!(preview_tile_range(100.0, 100.0, 0.5, 500), (100, 400));
    }

    #[test]
    fn scaled_downward_growth_preserves_and_follows_the_bottom() {
        assert_eq!(
            anchored_preview_offset(100.0, 1000, 1200, 0, ScrollDirection::Down, 200.0, 0.5),
            100.0
        );
        assert_eq!(
            anchored_preview_offset(300.0, 1000, 1200, 0, ScrollDirection::Down, 200.0, 0.5),
            400.0
        );
    }

    #[test]
    fn first_preview_frame_starts_at_the_top() {
        assert_eq!(
            anchored_preview_offset(0.0, 0, 2280, 0, ScrollDirection::Down, 1140.0, 0.5),
            0.0
        );
        assert_eq!(
            anchored_preview_offset(0.0, 0, 2280, 1140, ScrollDirection::Up, 1140.0, 0.5),
            0.0
        );
    }

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
    fn fullscreen_preview_stays_above_the_toolbar() {
        let display = Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)));
        let selection = Bounds::new(point(px(0.), px(0.)), display.size);
        let bar = scroll_bar_bounds(display, selection);
        let preview = scroll_preview_bounds(display, selection, bar);

        assert_eq!(bar.origin.x, px(1920. - SCROLL_BAR_SIZE.0));
        assert_eq!(bar.origin.y, px(1080. - SCROLL_BAR_EXPANDED_HEIGHT - 8.));
        assert!(
            bar.origin.y + px(SCROLL_BAR_EXPANDED_HEIGHT) <= display.origin.y + display.size.height
        );
        assert_eq!(
            preview.size.height,
            px(PREVIEW_CONTENT_WIDTH * 1080.0 / 1920.0 + PREVIEW_CHROME_HEIGHT).max(px(220.))
        );
        assert!(!preview.intersects(&bar));
        assert!(preview.origin.y >= display.origin.y + px(16.));
        assert!(preview.origin.y + preview.size.height + px(10.) <= bar.origin.y);
        assert!(preview.origin.x + preview.size.width <= display.origin.x + display.size.width);
    }

    #[test]
    fn scroll_bar_bounds_reserves_room_for_warning_row_near_bottom() {
        let display = Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)));
        // 50 px left below selection fits 36 + 8 = 44 px, but would clip when expanded to 64 px.
        let selection = Bounds::new(point(px(200.), px(200.)), size(px(600.), px(830.)));
        let bar = scroll_bar_bounds(display, selection);
        assert_eq!(bar.origin.y, px(200. - 8. - SCROLL_BAR_SIZE.1));
        assert!(bar.origin.y + px(SCROLL_BAR_EXPANDED_HEIGHT) <= display.size.height);
    }

    #[test]
    fn scroll_status_info_formats_warnings_success_and_capturing() {
        let mut snap = ScrollSnapshot {
            started: true,
            status: ScrollStatus::Capturing,
            ..Default::default()
        };
        let capturing = scroll_status_info(&snap);
        assert_eq!(capturing.tone, ScrollStatusTone::Active);
        assert!(capturing.warning.is_none());

        snap.warning = Some(ScrollWarning::TooFast);
        let too_fast = scroll_status_info(&snap);
        assert_eq!(too_fast.tone, ScrollStatusTone::Warning);
        assert!(too_fast.warning.is_some());
        assert_eq!(too_fast.label, too_fast.warning.clone().unwrap());

        snap.warning = Some(ScrollWarning::LowTexture);
        let low_tex = scroll_status_info(&snap);
        assert_eq!(low_tex.tone, ScrollStatusTone::Warning);
        assert!(low_tex.warning.is_some());

        snap.warning = None;
        snap.status = ScrollStatus::ReachedEnd;
        let reached = scroll_status_info(&snap);
        assert_eq!(reached.tone, ScrollStatusTone::Success);
        assert!(reached.warning.is_none());
    }

    #[test]
    fn side_preview_keeps_its_selection_alignment_on_offset_displays() {
        let display = Bounds::new(point(px(-1920.), px(0.)), size(px(1920.), px(1080.)));
        let selection = Bounds::new(point(px(100.), px(100.)), size(px(600.), px(500.)));
        let bar = scroll_bar_bounds(display, selection);
        let preview = scroll_preview_bounds(display, selection, bar);

        assert_eq!(
            preview.origin.x,
            display.origin.x + selection.origin.x + selection.size.width + px(10.)
        );
        assert_eq!(preview.origin.y, display.origin.y + selection.origin.y);
        assert!(!preview.intersects(&bar));
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

    #[test]
    fn sticky_margin_shrink_preserves_the_viewed_content() {
        assert_eq!(
            anchored_preview_offset(500.0, 1200, 1000, -200, ScrollDirection::Down, 200.0, 1.0),
            300.0
        );
        assert_eq!(
            anchored_preview_offset(500.0, 1200, 1000, -200, ScrollDirection::Up, 200.0, 1.0),
            300.0
        );
    }

    #[test]
    fn synced_preview_offset_locks_to_live_viewport_edge() {
        // Downward scroll at canvas bottom locks flush to max_top.
        assert_eq!(
            synced_preview_offset(1000, 500, 1500, ScrollDirection::Down, 200.0, 0.5),
            550.0
        );
        // Downward scroll after scrolling back to mid-canvas follows viewport bottom.
        assert_eq!(
            synced_preview_offset(400, 500, 1500, ScrollDirection::Down, 200.0, 0.5),
            250.0
        );
        // Upward scroll at canvas top locks to 0.
        assert_eq!(
            synced_preview_offset(0, 500, 1500, ScrollDirection::Up, 200.0, 0.5),
            0.0
        );
        // Upward scroll in mid-canvas follows viewport top.
        assert_eq!(
            synced_preview_offset(300, 500, 1500, ScrollDirection::Up, 200.0, 0.5),
            150.0
        );
    }

    #[test]
    fn preview_display_height_tracks_rendered_tile_without_bottom_gap() {
        let tile = CachedPreviewTile {
            revision: 1,
            source_start: 400,
            source_rows: 800,
            total_height: 1200,
            image: Arc::new(RgbaImage::new(1, 1)),
            render: session::render_image(&RgbaImage::new(1, 1)),
            width: 1,
        };
        assert_eq!(preview_display_height(Some(&tile), 1560), 1200);
        assert_eq!(preview_display_height(Some(&tile), 1100), 1100);
        assert_eq!(preview_display_height(None, 1560), 1560);
    }

    #[test]
    fn follow_live_preview_request_stays_stable_as_height_grows() {
        let first = desired_preview_request(true, 100.0, 300.0, 0.5, 1200);
        let second = desired_preview_request(true, 450.0, 300.0, 0.5, 2400);
        assert_eq!(first, second);
        assert!(first.2);
        let manual = desired_preview_request(false, 450.0, 300.0, 0.5, 2400);
        assert!(!manual.2);
        assert_ne!(manual.0, 0);
    }
}
