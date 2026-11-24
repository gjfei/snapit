//! Pinned captures: borderless, always-on-top image windows.

use std::{path::PathBuf, sync::Arc};

use anyhow::Context as _;
use gpui_kit::component::{ActiveTheme as _, native_menu::NativeMenu};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, DisplayId, FocusHandle, InteractiveElement as _,
    IntoElement, KeyBinding, MouseButton, MouseDownEvent, ObjectFit, ParentElement as _, Pixels,
    Render, RenderImage, ScrollEvent, Size, Styled as _, StyledImage as _, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, actions, div, img,
    prelude::FluentBuilder as _, px, size,
};

use crate::controller::{Captured, Controller};
use crate::session;

const CONTEXT: &str = "SnapItPin";
const ZOOM_RANGE: (f32, f32) = (0.1, 5.0);
const MIN_OPACITY: f32 = 0.2;
/// Pins opened away from their selection start at most this share of the display.
const MAX_INITIAL_SHARE: f32 = 0.6;
const MIN_SIDE: f32 = 24.;

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

pub fn open(captured: Captured, placement: Option<Placement>, cx: &mut App) {
    let natural = size(
        px(captured.image.width() as f32 / captured.scale),
        px(captured.image.height() as f32 / captured.scale),
    );
    let (display_id, bounds, zoom) = match placement {
        Some(placement) => (placement.display_id, placement.bounds, 1.0),
        None => {
            let zoom = cx
                .primary_display()
                .map(|display| {
                    let area = display.bounds().size;
                    ((area.width / natural.width).min(area.height / natural.height)
                        * MAX_INITIAL_SHARE)
                        .min(1.0)
                })
                .unwrap_or(1.0);
            let size = scaled(natural, zoom);
            (None, Bounds::centered(None, size, cx), zoom)
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
        cx.new(|cx| PinView::new(captured, natural, zoom, window, cx))
    }) {
        log::error!("failed to open pin window: {err:#}");
    }
}

/// Pin an image file, e.g. from history.
pub fn open_file(path: PathBuf, scale: f32, cx: &mut App) {
    let load = cx.background_spawn(async move {
        // History originals are 16-bit BT.2020/PQ HDR PNGs. Decode those with
        // the PQ-aware path so a pinned history capture shows the same SDR
        // preview as the live capture; ordinary PNG/GIF use the generic decoder.
        let bytes = std::fs::read(&path)
            .with_context(|| t!("pin.read_failed", path = path.display()).to_string())?;
        match snapit_capture::decode_hdr_png_preview(&bytes) {
            Ok(Some(preview)) => Ok(preview),
            Ok(None) => image::load_from_memory(&bytes)
                .with_context(|| t!("pin.read_failed", path = path.display()).to_string())
                .map(|image| image.to_rgba8()),
            Err(err) => {
                log::warn!("failed to decode HDR PNG for pin, trying generic decoder: {err:#}");
                image::load_from_memory(&bytes)
                    .with_context(|| t!("pin.read_failed", path = path.display()).to_string())
                    .map(|image| image.to_rgba8())
            }
        }
    });
    cx.spawn(async move |cx| {
        let image = load.await;
        cx.update(|cx| match image {
            Ok(image) => open(
                Captured {
                    image: Arc::new(image),
                    hdr: None,
                    scale: scale.max(0.1),
                },
                None,
                cx,
            ),
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
    render: Arc<RenderImage>,
    natural: Size<Pixels>,
    zoom: f32,
    opacity: f32,
    focus_handle: FocusHandle,
}

impl PinView {
    fn new(
        captured: Captured,
        natural: Size<Pixels>,
        zoom: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        Self {
            render: session::render_image(&captured.image),
            captured,
            natural,
            zoom,
            opacity: 1.0,
            focus_handle,
        }
    }

    fn set_zoom(&mut self, zoom: f32, window: &mut Window, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = zoom;
        window.resize(scaled(self.natural, zoom));
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

    /// Scroll zooms; with Alt (Option) held it changes opacity instead.
    fn on_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(window.line_height()).y.as_f32();
        if delta == 0. {
            return;
        }
        if event.modifiers.alt {
            self.set_opacity(self.opacity + delta * 0.002, cx);
        } else {
            self.set_zoom(self.zoom * (delta * 0.005).exp(), window, cx);
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
            .child(
                img(self.render.clone())
                    .size_full()
                    .object_fit(ObjectFit::Fill),
            )
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
