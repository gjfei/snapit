//! Pinned captures: borderless, always-on-top image windows.

use std::{path::PathBuf, sync::Arc};

use anyhow::Context as _;
use gpui_kit::component::{
    ActiveTheme as _, native_menu::NativeMenu, scroll::ScrollableElement as _,
};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, DisplayId, FocusHandle, InteractiveElement as _,
    IntoElement, KeyBinding, MouseButton, MouseDownEvent, ObjectFit, ParentElement as _, Pixels,
    Render, RenderImage, ScrollEvent, ScrollHandle, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, actions, div, img,
    prelude::FluentBuilder as _, px, size,
};

use snapit_capture::RgbaImage;

use crate::controller::{Captured, Controller};
use crate::session;

const CONTEXT: &str = "SnapItPin";
const ZOOM_RANGE: (f32, f32) = (0.1, 5.0);
const MIN_OPACITY: f32 = 0.2;
/// Pins opened away from their selection start at most this share of the display.
const MAX_INITIAL_SHARE: f32 = 0.6;
const MIN_SIDE: f32 = 24.;
const TILE_MAX_HEIGHT: u32 = 2048;

struct PinTile {
    id: usize,
    height: u32,
    render: Arc<RenderImage>,
}

fn build_pin_tiles(image: &RgbaImage) -> Vec<PinTile> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let count = height.div_ceil(TILE_MAX_HEIGHT) as usize;
    let mut tiles = Vec::with_capacity(count);
    for ix in 0..count {
        let y0 = ix as u32 * TILE_MAX_HEIGHT;
        let h = TILE_MAX_HEIGHT.min(height - y0);
        let slice = image::imageops::crop_imm(image, 0, y0, width, h).to_image();
        tiles.push(PinTile {
            id: ix,
            height: h,
            render: session::render_image(&slice),
        });
    }
    tiles
}

actions!(
    snapit_pin,
    [
        ClosePin, CopyPin, SavePin, ZoomIn, ZoomOut, ResetZoom, Opacity100, Opacity80, Opacity60,
        Opacity40
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", ClosePin, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("w"), ClosePin, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("c"), CopyPin, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("s"), SavePin, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("="), ZoomIn, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("-"), ZoomOut, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("0"), ResetZoom, Some(CONTEXT)),
    ]);
}

/// Where a pin appears: over the selection it was made from, or centered.
pub struct Placement {
    pub display_id: Option<DisplayId>,
    pub bounds: Bounds<Pixels>,
}

pub fn open(
    captured: Captured,
    preview: Arc<RgbaImage>,
    placement: Option<Placement>,
    cx: &mut App,
) {
    let natural = size(
        px(preview.width() as f32 / captured.scale),
        px(preview.height() as f32 / captured.scale),
    );
    let display_size = display_id_size(placement.as_ref().and_then(|p| p.display_id), cx);
    let is_tall =
        natural.height > display_size.height * 0.8 && natural.height > natural.width * 1.5;
    let (display_id, bounds, zoom, scrollable) = match placement {
        Some(placement) => {
            if is_tall {
                let win_h = placement.bounds.size.height.min(display_size.height * 0.75);
                let bounds = Bounds::new(
                    placement.bounds.origin,
                    size(
                        placement.bounds.size.width.max(px(MIN_SIDE)),
                        win_h.max(px(MIN_SIDE)),
                    ),
                );
                (placement.display_id, bounds, 1.0, true)
            } else {
                (placement.display_id, placement.bounds, 1.0, false)
            }
        }
        None => {
            if is_tall {
                let zoom = ((display_size.width * 0.55) / natural.width).clamp(0.25, 1.0);
                let win_w = (natural.width * zoom).max(px(MIN_SIDE));
                let win_h = (natural.height * zoom)
                    .min(display_size.height * 0.75)
                    .max(px(MIN_SIDE));
                (
                    None,
                    Bounds::centered(None, size(win_w, win_h), cx),
                    zoom,
                    true,
                )
            } else {
                let zoom = ((display_size.width / natural.width)
                    .min(display_size.height / natural.height)
                    * MAX_INITIAL_SHARE)
                    .min(1.0);
                let size = scaled(natural, zoom);
                (None, Bounds::centered(None, size, cx), zoom, false)
            }
        }
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: true,
        show: true,
        kind: WindowKind::PopUp,
        is_movable: true,
        is_resizable: false,
        is_minimizable: false,
        display_id,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| PinView::new(captured, preview, natural, zoom, scrollable, window, cx))
    }) {
        log::error!("failed to open pin window: {err:#}");
    }
}

fn display_id_size(display_id: Option<DisplayId>, cx: &App) -> Size<Pixels> {
    display_id
        .and_then(|id| cx.displays().into_iter().find(|d| d.id() == id))
        .or_else(|| cx.primary_display())
        .map(|d| d.bounds().size)
        .unwrap_or_else(|| size(px(1440.), px(900.)))
}

/// Pin an image file, e.g. from history.
pub fn open_file(path: PathBuf, scale: f32, cx: &mut App) {
    let load = cx.background_spawn(async move {
        let png = Arc::new(
            std::fs::read(&path)
                .with_context(|| t!("pin.read_failed", path = path.display()).to_string())?,
        );
        match snapit_capture::decode_hdr_png_preview(&png) {
            Ok(Some(preview)) => Ok((png, preview, true)),
            Ok(None) => {
                let preview = image::load_from_memory(&png)
                    .with_context(|| t!("pin.read_failed", path = path.display()).to_string())?
                    .to_rgba8();
                Ok((png, preview, false))
            }
            Err(err) => Err(err).context(t!("pin.read_failed", path = path.display()).to_string()),
        }
    });
    cx.spawn(async move |cx| {
        let loaded = load.await;
        cx.update(|cx| match loaded {
            Ok((png, preview, hdr)) => {
                let preview = Arc::new(preview);
                let captured = Captured::frozen(png, preview.clone(), scale, hdr);
                open(captured, preview, None, cx)
            }
            Err(err) => Controller::global(cx)
                .update(cx, |controller, cx| controller.fail(format!("{err:#}"), cx)),
        })
    })
    .detach();
}

fn scaled(natural: Size<Pixels>, zoom: f32) -> Size<Pixels> {
    size(
        (natural.width * zoom).max(px(MIN_SIDE)),
        (natural.height * zoom).max(px(MIN_SIDE)),
    )
}

pub struct PinView {
    captured: Captured,
    tiles: Vec<PinTile>,
    natural: Size<Pixels>,
    zoom: f32,
    opacity: f32,
    scrollable: bool,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
}

impl PinView {
    fn new(
        captured: Captured,
        preview: Arc<RgbaImage>,
        natural: Size<Pixels>,
        zoom: f32,
        scrollable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            tiles: build_pin_tiles(&preview),
            captured,
            natural,
            zoom,
            opacity: 1.0,
            scrollable,
            scroll_handle: ScrollHandle::new(),
            focus_handle,
        }
    }

    fn set_zoom(&mut self, zoom: f32, window: &mut Window, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = zoom;
        let mut next = scaled(self.natural, zoom);
        if self.scrollable {
            let max_h = window
                .display(cx)
                .map(|d| d.bounds().size.height)
                .unwrap_or_else(|| display_id_size(None, cx).height)
                * 0.85;
            next.height = next.height.min(max_h).max(px(MIN_SIDE));
        }
        window.resize(next);
        cx.notify();
    }

    fn set_opacity(&mut self, opacity: f32, cx: &mut Context<Self>) {
        self.opacity = opacity.clamp(MIN_OPACITY, 1.0);
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        if event.click_count >= 2 {
            window.remove_window();
        } else {
            window.start_window_move();
        }
    }

    fn on_right_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let opacity = |percent: u8| (self.opacity * 100.).round() as u8 == percent;
        NativeMenu::new()
            .menu(t!("pin.copy").to_string(), Box::new(CopyPin))
            .menu(t!("pin.save").to_string(), Box::new(SavePin))
            .separator()
            .submenu(
                t!("pin.opacity").to_string(),
                NativeMenu::new()
                    .menu_with_check("100%", opacity(100), Box::new(Opacity100))
                    .menu_with_check("80%", opacity(80), Box::new(Opacity80))
                    .menu_with_check("60%", opacity(60), Box::new(Opacity60))
                    .menu_with_check("40%", opacity(40), Box::new(Opacity40)),
            )
            .submenu(
                t!("pin.zoom").to_string(),
                NativeMenu::new()
                    .menu(t!("pin.zoom_in").to_string(), Box::new(ZoomIn))
                    .menu(t!("pin.zoom_out").to_string(), Box::new(ZoomOut))
                    .menu_with_check(
                        t!("pin.reset_zoom").to_string(),
                        (self.zoom - 1.0).abs() < 0.01,
                        Box::new(ResetZoom),
                    ),
            )
            .separator()
            .menu(t!("pin.close").to_string(), Box::new(ClosePin))
            .show(event.position, window, cx);
    }

    /// Scroll zooms; for tall scrollable pins, plain scroll moves through the
    /// image while Cmd/Ctrl+scroll zooms and Alt+scroll changes opacity.
    fn on_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(window.line_height()).y.as_f32();
        if delta == 0. {
            return;
        }
        if event.modifiers.alt {
            self.set_opacity(self.opacity + delta * 0.002, cx);
            cx.stop_propagation();
        } else if !self.scrollable || event.modifiers.platform || event.modifiers.control {
            self.set_zoom(self.zoom * (delta * 0.005).exp(), window, cx);
            cx.stop_propagation();
        }
    }

    fn copy(&mut self, _: &CopyPin, _: &mut Window, cx: &mut Context<Self>) {
        let captured = self.captured.clone();
        Controller::global(cx).update(cx, |controller, cx| controller.copy_image(captured, cx));
    }

    fn save(&mut self, _: &SavePin, _: &mut Window, cx: &mut Context<Self>) {
        let captured = self.captured.clone();
        Controller::global(cx).update(cx, |controller, cx| controller.save_image(captured, cx));
    }
}

impl Render for PinView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        div()
            .id("pin")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .opacity(self.opacity)
            .cursor_move()
            .on_action(cx.listener(|_, _: &ClosePin, window, _| window.remove_window()))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| {
                this.set_zoom(this.zoom * 1.25, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| {
                this.set_zoom(this.zoom / 1.25, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ResetZoom, window, cx| this.set_zoom(1.0, window, cx)),
            )
            .on_action(cx.listener(|this, _: &Opacity100, _, cx| this.set_opacity(1.0, cx)))
            .on_action(cx.listener(|this, _: &Opacity80, _, cx| this.set_opacity(0.8, cx)))
            .on_action(cx.listener(|this, _: &Opacity60, _, cx| this.set_opacity(0.6, cx)))
            .on_action(cx.listener(|this, _: &Opacity40, _, cx| this.set_opacity(0.4, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_click))
            .on_scroll(cx.listener(Self::on_scroll))
            .when(self.scrollable, |this| {
                let win_w = self.natural.width * self.zoom;
                let scale = self.captured.scale;
                let zoom = self.zoom;
                this.child(
                    div()
                        .id("pin-scroll-wrap")
                        .size_full()
                        .relative()
                        .child(
                            div()
                                .id("pin-scroll")
                                .size_full()
                                .overflow_y_scroll()
                                .track_scroll(&self.scroll_handle)
                                .child(gpui_kit::component::v_flex().w(win_w).children(
                                    self.tiles.iter().map(|tile| {
                                        let tile_h = px((tile.height as f32 / scale) * zoom);
                                        img(tile.render.clone())
                                            .id(SharedString::from(format!("pin-tile-{}", tile.id)))
                                            .w(win_w)
                                            .h(tile_h)
                                            .object_fit(ObjectFit::Fill)
                                    }),
                                )),
                        )
                        .vertical_scrollbar(&self.scroll_handle),
                )
            })
            .when(!self.scrollable, |this| {
                if self.tiles.len() <= 1 {
                    this.when_some(self.tiles.first(), |this, tile| {
                        this.child(
                            img(tile.render.clone())
                                .size_full()
                                .object_fit(ObjectFit::Fill),
                        )
                    })
                } else {
                    let scale = self.captured.scale;
                    let zoom = self.zoom;
                    this.child(
                        gpui_kit::component::v_flex()
                            .size_full()
                            .overflow_hidden()
                            .children(self.tiles.iter().map(|tile| {
                                let tile_h = px((tile.height as f32 / scale) * zoom);
                                img(tile.render.clone())
                                    .id(SharedString::from(format!("pin-tile-{}", tile.id)))
                                    .flex_none()
                                    .w_full()
                                    .h(tile_h)
                                    .object_fit(ObjectFit::Fill)
                            })),
                    )
                }
            })
            .child(div().absolute().inset_0().border_1().border_color(border))
            // Windows has no programmatic window drag; give it a caption strip.
            .when(cfg!(target_os = "windows"), |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(12.))
                        .window_control_area(gpui_kit::WindowControlArea::Drag),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_pin_tiles_splits_only_above_gpu_tile_limit() {
        let short = RgbaImage::new(200, 1080);
        let short_tiles = build_pin_tiles(&short);
        assert_eq!(short_tiles.len(), 1);
        assert_eq!(short_tiles[0].height, 1080);

        let tall = RgbaImage::new(200, 5000);
        let tall_tiles = build_pin_tiles(&tall);
        let heights: Vec<u32> = tall_tiles.iter().map(|t| t.height).collect();
        assert_eq!(heights, vec![TILE_MAX_HEIGHT, TILE_MAX_HEIGHT, 904]);
    }
}
