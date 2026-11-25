use std::{sync::Arc, time::Duration};

use snapit_capture::{HdrImage, MonitorInfo, Region, RgbaImage};

use super::{ClickEffect, FrameData};

pub const CLICK_DURATION: Duration = Duration::from_millis(450);
pub const CLICK_COLOR: [u8; 3] = [238, 155, 44];
pub const CLICK_PALETTE: [[u8; 3]; 7] = [
    CLICK_COLOR,
    [251, 113, 133],
    [251, 191, 36],
    [56, 189, 248],
    [167, 139, 250],
    [52, 211, 153],
    [255, 248, 225],
];
const CLICK_PREVIEW_SIZE: u32 = 64;

pub(crate) struct PointerState {
    monitor: Option<MonitorInfo>,
    region: Region,
    captures_cursor: bool,
    click_effects: bool,
    click_effect: ClickEffect,
    native_cursor: bool,
    buttons: u8,
    sampled_buttons: bool,
    position: Option<(i32, i32)>,
    base: Option<FrameData>,
    pointer_dirty: bool,
    last_click_count: usize,
    cursor_shown: bool,
    #[cfg(target_os = "windows")]
    position_drawn: Option<(i32, i32)>,
    #[cfg(target_os = "windows")]
    cursor_dirty: bool,
    clicks: Vec<Click>,
    #[cfg(target_os = "windows")]
    cursor: Option<CursorState>,
}

#[derive(Clone, Copy)]
struct Click {
    position: (i32, i32),
    started: Duration,
}

#[cfg(target_os = "windows")]
struct CursorState {
    handle: isize,
    visible: bool,
    image: Option<RgbaImage>,
    hotspot: (f64, f64),
}

struct PointerSample {
    position: (i32, i32),
    buttons: u8,
    #[cfg(target_os = "windows")]
    cursor_handle: isize,
    #[cfg(target_os = "windows")]
    cursor_visible: bool,
}

struct HdrPixels {
    width: u32,
    height: u32,
    reference_white: f32,
    sdr_white: f32,
    raw: Vec<u8>,
}

fn sdr_pixels_for_composite(image: Arc<RgbaImage>) -> RgbaImage {
    match Arc::try_unwrap(image) {
        Ok(image) => image,
        Err(image) => (*image).clone(),
    }
}

fn hdr_pixels_for_composite(image: Arc<HdrImage>) -> HdrPixels {
    let width = image.width();
    let height = image.height();
    let reference_white = image.reference_white_nits();
    let sdr_white = image.sdr_white_nits();
    let raw = match Arc::try_unwrap(image) {
        Ok(image) => image.into_bytes(),
        Err(image) => image.as_bytes().to_vec(),
    };
    HdrPixels {
        width,
        height,
        reference_white,
        sdr_white,
        raw,
    }
}

fn click_center(
    monitor_x: i32,
    monitor_y: i32,
    region: Region,
    position: (i32, i32),
    width: u32,
    height: u32,
) -> (f64, f64) {
    let scale_x = f64::from(width) / f64::from(region.width.max(0.001));
    let scale_y = f64::from(height) / f64::from(region.height.max(0.001));
    (
        (f64::from(position.0) - f64::from(monitor_x) - f64::from(region.x)) * scale_x,
        (f64::from(position.1) - f64::from(monitor_y) - f64::from(region.y)) * scale_y,
    )
}

impl PointerState {
    pub(crate) fn new(
        monitor_id: u32,
        region: Region,
        captures_cursor: bool,
        click_effects: bool,
        click_effect: ClickEffect,
    ) -> Self {
        let monitor = snapit_capture::monitors().ok().and_then(|monitors| {
            monitors
                .into_iter()
                .find(|monitor| monitor.id == monitor_id)
        });
        if monitor.is_none() && (captures_cursor || click_effects) {
            log::warn!("monitor {monitor_id} geometry unavailable; pointer effects disabled");
        }
        if let Some(monitor) = &monitor {
            log::info!(
                "pointer geometry: monitor {monitor_id} {}x{}+{}+{} (scale {:.2}), region \
                 {:.1},{:.1} {:.1}x{:.1}, cursor={captures_cursor} native={}, clicks={click_effects} effect={click_effect:?}",
                monitor.width,
                monitor.height,
                monitor.x,
                monitor.y,
                monitor.scale_factor,
                region.x,
                region.y,
                region.width,
                region.height,
                cfg!(target_os = "macos")
            );
        }
        Self {
            monitor,
            region,
            captures_cursor,
            click_effects,
            click_effect,
            native_cursor: cfg!(target_os = "macos"),
            buttons: 0,
            sampled_buttons: false,
            position: None,
            base: None,
            pointer_dirty: false,
            last_click_count: 0,
            cursor_shown: false,
            #[cfg(target_os = "windows")]
            position_drawn: None,
            #[cfg(target_os = "windows")]
            cursor_dirty: captures_cursor,
            clicks: Vec::new(),
            #[cfg(target_os = "windows")]
            cursor: None,
        }
    }

    pub(crate) fn sample(&mut self, at: Duration) {
        let Some(sample) = self.poll() else {
            return;
        };
        let pressed = sample.buttons & !self.buttons;
        if self.click_effects && self.captures_cursor && self.sampled_buttons {
            for button in 0..3 {
                if pressed & (1 << button) != 0 {
                    self.clicks.push(Click {
                        position: sample.position,
                        started: at,
                    });
                    let inside = self.monitor.as_ref().is_some_and(|monitor| {
                        let x = f64::from(sample.position.0) - f64::from(monitor.x);
                        let y = f64::from(sample.position.1) - f64::from(monitor.y);
                        x >= f64::from(self.region.x)
                            && y >= f64::from(self.region.y)
                            && x <= f64::from(self.region.x + self.region.width)
                            && y <= f64::from(self.region.y + self.region.height)
                    });
                    log::info!(
                        "pointer press: button={button} at {:.3} ms, global=({}, {}), inside_region={inside}",
                        at.as_secs_f64() * 1000.0,
                        sample.position.0,
                        sample.position.1
                    );
                }
            }
        }
        self.buttons = sample.buttons;
        self.sampled_buttons = true;
        #[cfg(target_os = "windows")]
        let previous_position = self.position;
        self.position = Some(sample.position);
        self.clicks
            .retain(|click| at.saturating_sub(click.started) < CLICK_DURATION);
        self.pointer_dirty |= !self.clicks.is_empty();
        #[cfg(target_os = "windows")]
        {
            let cursor = self.cursor.get_or_insert(CursorState {
                handle: sample.cursor_handle,
                visible: sample.cursor_visible,
                image: None,
                hotspot: (0.0, 0.0),
            });
            let cursor_changed = cursor.handle != sample.cursor_handle;
            if cursor_changed {
                *cursor = CursorState {
                    handle: sample.cursor_handle,
                    visible: sample.cursor_visible,
                    image: None,
                    hotspot: (0.0, 0.0),
                };
            }
            let visibility_changed = cursor.visible != sample.cursor_visible;
            cursor.visible = sample.cursor_visible;
            self.cursor_dirty |= self.captures_cursor
                && (cursor_changed || visibility_changed || previous_position != self.position);
            self.pointer_dirty |= self.cursor_dirty;
        }
    }

    pub(crate) fn sample_paused(&mut self) {
        let Some(sample) = self.poll() else {
            return;
        };
        self.buttons = sample.buttons;
        self.sampled_buttons = true;
        self.clicks.clear();
    }

    pub(crate) fn apply(&mut self, frame: FrameData, at: Duration) -> FrameData {
        // DXGI reports desktop changes, not pointer movement. Keep every clean
        // incoming frame (an Arc clone, not a pixel copy) so a timeout can move
        // or animate the cursor over the latest static desktop. This must happen
        // before either early return below.
        #[cfg(target_os = "windows")]
        if self.captures_cursor {
            self.base = Some(frame.clone());
        }
        if self.monitor.is_none()
            || (!self.click_effects && (!self.captures_cursor || self.native_cursor))
        {
            return frame;
        }
        #[cfg(target_os = "windows")]
        let position = self.position;
        let active_clicks = self
            .clicks
            .iter()
            .filter(|click| at.saturating_sub(click.started) < CLICK_DURATION)
            .count();
        #[cfg(target_os = "windows")]
        let cursor_changed = self.cursor_dirty;
        #[cfg(not(target_os = "windows"))]
        let cursor_changed = false;
        if !needs_pointer_redraw(
            active_clicks,
            self.last_click_count,
            cursor_changed,
            self.cursor_shown,
        ) {
            return frame;
        }
        if active_clicks > 0 && self.last_click_count == 0 {
            let (width, height) = frame.dimensions();
            let centers = self
                .clicks
                .iter()
                .filter(|click| at.saturating_sub(click.started) < CLICK_DURATION)
                .map(|click| {
                    self.monitor.as_ref().map(|monitor| {
                        click_center(
                            monitor.x,
                            monitor.y,
                            self.region,
                            click.position,
                            width,
                            height,
                        )
                    })
                })
                .collect::<Vec<_>>();
            log::info!(
                "drawing {active_clicks} click effect(s) at {:.3} ms in {width}x{height}, centers={centers:?}",
                at.as_secs_f64() * 1000.0
            );
        }

        // Timeout redraws need the unmodified pixels only while a click is
        // expanding. Windows already retained the clean incoming frame above so
        // cursor-only timeouts have a base too. A plain cursor frame elsewhere
        // can still consume its unique allocation.
        #[cfg(not(target_os = "windows"))]
        if active_clicks > 0 {
            self.base = Some(frame.clone());
        } else {
            self.base = None;
        }
        self.last_click_count = active_clicks;
        let image = match frame {
            FrameData::Sdr(image) => {
                let mut image = sdr_pixels_for_composite(image);
                self.draw_clicks(&mut image, at);
                self.cursor_shown = false;
                #[cfg(target_os = "windows")]
                if self.captures_cursor {
                    self.cursor_shown = self.draw_cursor(&mut image, position);
                }
                FrameData::Sdr(Arc::new(image))
            }
            FrameData::Hdr(image) => {
                let HdrPixels {
                    width,
                    height,
                    reference_white,
                    sdr_white,
                    mut raw,
                } = hdr_pixels_for_composite(image);
                let white_scale = sdr_white / reference_white;
                self.draw_clicks_hdr(width, white_scale, &mut raw, at);
                self.cursor_shown = false;
                #[cfg(target_os = "windows")]
                if self.captures_cursor {
                    self.cursor_shown =
                        self.draw_cursor_hdr(width, height, white_scale, &mut raw, position);
                }
                FrameData::Hdr(Arc::new(
                    HdrImage::new(width, height, raw, reference_white).with_sdr_white(sdr_white),
                ))
            }
        };
        #[cfg(target_os = "windows")]
        if self.cursor_shown {
            self.position_drawn = position;
        } else {
            self.position_drawn = None;
        }
        #[cfg(target_os = "windows")]
        {
            self.cursor_dirty = false;
        }
        self.pointer_dirty = active_clicks > 0;
        image
    }

    pub(crate) fn apply_timeout(&mut self, at: Duration) -> Option<FrameData> {
        if !self.pointer_dirty || self.monitor.is_none() {
            return None;
        }
        Some(self.apply(self.base.clone()?, at))
    }

    fn draw_clicks(&self, image: &mut RgbaImage, at: Duration) {
        let Some(monitor) = &self.monitor else {
            return;
        };
        let scale_x = image.width() as f64 / f64::from(self.region.width.max(0.001));
        let scale_y = image.height() as f64 / f64::from(self.region.height.max(0.001));
        let scale = scale_x.max(scale_y).max(1.0);
        for click in &self.clicks {
            let progress = click_progress(click, at);
            let (center_x, center_y) = click_center(
                monitor.x,
                monitor.y,
                self.region,
                click.position,
                image.width(),
                image.height(),
            );
            visit_click_effect(
                ClickCanvas {
                    width: image.width(),
                    height: image.height(),
                    center_x,
                    center_y,
                },
                self.click_effect,
                progress,
                scale,
                |x, y, color, opacity| blend_pixel(image.get_pixel_mut(x, y), color, opacity),
            );
        }
    }

    fn draw_clicks_hdr(&self, width: u32, white_scale: f32, raw: &mut [u8], at: Duration) {
        let Some(monitor) = &self.monitor else {
            return;
        };
        let height = (raw.len() / 6) as u32 / width.max(1);
        let scale_x = width as f64 / f64::from(self.region.width.max(0.001));
        let scale_y = height as f64 / f64::from(self.region.height.max(0.001));
        let scale = scale_x.max(scale_y).max(1.0);
        for click in &self.clicks {
            let progress = click_progress(click, at);
            let (center_x, center_y) = click_center(
                monitor.x,
                monitor.y,
                self.region,
                click.position,
                width,
                height,
            );
            visit_click_effect(
                ClickCanvas {
                    width,
                    height,
                    center_x,
                    center_y,
                },
                self.click_effect,
                progress,
                scale,
                |x, y, color, opacity| {
                    let color = color.map(|channel| srgb_to_linear(channel) * white_scale);
                    blend_hdr_pixel(width, raw, x, y, color, opacity);
                },
            );
        }
    }

    #[cfg(target_os = "windows")]
    fn draw_cursor(&mut self, image: &mut RgbaImage, position: Option<(i32, i32)>) -> bool {
        let (Some(monitor), Some(position), Some(cursor)) =
            (&self.monitor, position, self.cursor.as_mut())
        else {
            return false;
        };
        if !cursor.visible || cursor.handle == 0 {
            return false;
        }
        if cursor.image.is_none()
            && let Some((image, hotspot)) = windows_cursor_image(cursor.handle)
        {
            cursor.image = Some(image);
            cursor.hotspot = hotspot;
        }
        let Some(cursor_image) = cursor.image.as_ref() else {
            return false;
        };
        let scale_x = image.width() as f64 / f64::from(self.region.width.max(0.001));
        let scale_y = image.height() as f64 / f64::from(self.region.height.max(0.001));
        let left = ((position.0 as f64 - monitor.x as f64 - self.region.x as f64) * scale_x
            - cursor.hotspot.0 * scale_x)
            .round() as i64;
        let top = ((position.1 as f64 - monitor.y as f64 - self.region.y as f64) * scale_y
            - cursor.hotspot.1 * scale_y)
            .round() as i64;
        blend_cursor(image, cursor_image, left, top)
    }

    #[cfg(target_os = "windows")]
    fn draw_cursor_hdr(
        &mut self,
        width: u32,
        height: u32,
        white_scale: f32,
        raw: &mut [u8],
        position: Option<(i32, i32)>,
    ) -> bool {
        let (Some(monitor), Some(position), Some(cursor)) =
            (&self.monitor, position, self.cursor.as_mut())
        else {
            return false;
        };
        if !cursor.visible || cursor.handle == 0 {
            return false;
        }
        if cursor.image.is_none()
            && let Some((image, hotspot)) = windows_cursor_image(cursor.handle)
        {
            cursor.image = Some(image);
            cursor.hotspot = hotspot;
        }
        let Some(cursor_image) = cursor.image.as_ref() else {
            return false;
        };
        let scale_x = width as f64 / f64::from(self.region.width.max(0.001));
        let scale_y = height as f64 / f64::from(self.region.height.max(0.001));
        let left = ((position.0 as f64 - monitor.x as f64 - self.region.x as f64) * scale_x
            - cursor.hotspot.0 * scale_x)
            .round() as i64;
        let top = ((position.1 as f64 - monitor.y as f64 - self.region.y as f64) * scale_y
            - cursor.hotspot.1 * scale_y)
            .round() as i64;
        blend_cursor_hdr(width, height, raw, cursor_image, left, top, white_scale)
    }

    #[cfg(target_os = "macos")]
    fn poll(&self) -> Option<PointerSample> {
        use objc2_app_kit::NSEvent;
        use objc2_core_graphics::{CGEvent, CGEventSource, CGEventSourceStateID, CGMouseButton};
        let event = CGEvent::new(None)?;
        let position = CGEvent::location(Some(&event));
        let state = CGEventSourceStateID::CombinedSessionState;
        let quartz_buttons = (0..3)
            .filter(|button| CGEventSource::button_state(state, CGMouseButton(*button)))
            .fold(0u8, |bits, button| bits | (1 << button));
        let appkit_buttons = NSEvent::pressedMouseButtons() as u8 & 0b111;
        let buttons = quartz_buttons | appkit_buttons;
        if quartz_buttons != appkit_buttons {
            log::debug!(
                "pointer button sources differ: quartz={quartz_buttons:#05b}, appkit={appkit_buttons:#05b}"
            );
        }
        Some(PointerSample {
            position: (position.x.round() as i32, position.y.round() as i32),
            buttons,
        })
    }

    #[cfg(target_os = "windows")]
    fn poll(&self) -> Option<PointerSample> {
        use windows::Win32::{
            Foundation::POINT,
            UI::{
                Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON},
                WindowsAndMessaging::{CURSOR_SHOWING, CURSORINFO, GetCursorInfo},
            },
        };
        unsafe {
            let mut info = CURSORINFO {
                cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            GetCursorInfo(&mut info).ok()?;
            let mut point = POINT::default();
            windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point).ok()?;
            let buttons = [
                VK_LBUTTON.0 as i32,
                VK_RBUTTON.0 as i32,
                VK_MBUTTON.0 as i32,
            ]
            .into_iter()
            .enumerate()
            .filter(|(_, key)| GetAsyncKeyState(*key) < 0)
            .fold(0u8, |bits, (button, _)| bits | (1 << button));
            Some(PointerSample {
                position: (point.x, point.y),
                buttons,
                cursor_handle: info.hCursor.0 as isize,
                cursor_visible: info.flags.0 & CURSOR_SHOWING.0 != 0,
            })
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn poll(&self) -> Option<PointerSample> {
        None
    }
}

fn click_progress(click: &Click, at: Duration) -> f64 {
    (at.saturating_sub(click.started).as_secs_f64() / CLICK_DURATION.as_secs_f64()).clamp(0.0, 1.0)
}

#[derive(Clone, Copy)]
struct ClickCanvas {
    width: u32,
    height: u32,
    center_x: f64,
    center_y: f64,
}

impl ClickEffect {
    /// Draw the same geometry the recorder composites, fitted to a 64px card.
    pub fn visit_preview(
        self,
        progress: f64,
        scale: f64,
        mut visit: impl FnMut(u32, u32, [u8; 3], f64),
    ) {
        let center = f64::from(CLICK_PREVIEW_SIZE) / 2.0;
        visit_click_effect(
            ClickCanvas {
                width: CLICK_PREVIEW_SIZE,
                height: CLICK_PREVIEW_SIZE,
                center_x: center,
                center_y: center,
            },
            self,
            progress,
            scale,
            &mut visit,
        );
    }
}

fn visit_click_effect(
    canvas: ClickCanvas,
    effect: ClickEffect,
    progress: f64,
    scale: f64,
    mut visit: impl FnMut(u32, u32, [u8; 3], f64),
) {
    let progress = progress.clamp(0.0, 1.0);
    let alpha = (1.0 - progress).powf(1.5);
    if alpha <= 0.0 {
        return;
    }
    match effect {
        ClickEffect::Ring => {
            let radius = (12.0 + progress * 16.0) * scale;
            let thickness = (2.0 * scale).max(1.5);
            visit_ring(canvas, radius, thickness, |x, y, coverage| {
                visit(x, y, CLICK_COLOR, coverage * alpha)
            });
        }
        ClickEffect::Dot => {
            let radius = (7.0 + progress * 2.0) * scale;
            visit_disc(canvas, radius, |x, y, coverage| {
                visit(x, y, CLICK_COLOR, coverage * alpha)
            });
        }
        ClickEffect::Outline => {
            let radius = (11.0 + progress * 9.0) * scale;
            let thickness = (2.0 * scale).max(1.5);
            visit_ring(canvas, radius, thickness, |x, y, coverage| {
                visit(x, y, CLICK_COLOR, coverage * alpha)
            });
            visit_disc(canvas, 2.8 * scale, |x, y, coverage| {
                visit(x, y, CLICK_COLOR, coverage * alpha)
            });
        }
        ClickEffect::Cross => {
            let half_length = (7.0 + progress * 12.0) * scale;
            let thickness = (2.5 * scale).max(1.5);
            visit_cross(canvas, half_length, thickness, |x, y, coverage| {
                visit(x, y, CLICK_COLOR, coverage * alpha)
            });
        }
        ClickEffect::Diamond => {
            let radius = (8.0 + progress * 17.0) * scale;
            let points = [
                (canvas.center_x, canvas.center_y - radius),
                (canvas.center_x + radius, canvas.center_y),
                (canvas.center_x, canvas.center_y + radius),
                (canvas.center_x - radius, canvas.center_y),
            ];
            visit_polygon_outline(canvas, &points, (2.0 * scale).max(1.5), |x, y, coverage| {
                visit(x, y, CLICK_COLOR, coverage * alpha)
            });
            visit_disc(canvas, 1.8 * scale, |x, y, coverage| {
                visit(x, y, CLICK_COLOR, coverage * alpha)
            });
        }
        ClickEffect::Corners => {
            let extent = (7.0 + progress * 16.0) * scale;
            let arm = (4.5 * scale).max(2.5);
            let thickness = (1.8 * scale).max(1.3);
            for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let x = canvas.center_x + sx * extent;
                let y = canvas.center_y + sy * extent;
                visit_segment(
                    canvas,
                    x,
                    y,
                    x - sx * arm,
                    y,
                    thickness,
                    |px, py, coverage| visit(px, py, CLICK_COLOR, coverage * alpha),
                );
                visit_segment(
                    canvas,
                    x,
                    y,
                    x,
                    y - sy * arm,
                    thickness,
                    |px, py, coverage| visit(px, py, CLICK_COLOR, coverage * alpha),
                );
            }
        }
        ClickEffect::Burst => {
            let inner = (6.0 + progress * 6.0) * scale;
            let outer = (15.0 + progress * 17.0) * scale;
            let thickness = (2.0 * scale).max(1.5);
            visit_burst(canvas, inner, outer, thickness, |x, y, coverage| {
                visit(x, y, CLICK_COLOR, coverage * alpha)
            });
        }
        ClickEffect::Ripple => {
            let first = progress.min(0.82) / 0.82;
            visit_ring(
                canvas,
                (7.0 + first * 26.0) * scale,
                (1.8 * scale).max(1.3),
                |x, y, coverage| visit(x, y, CLICK_COLOR, coverage * alpha),
            );
            if progress > 0.14 {
                let second = (progress - 0.14) / 0.86;
                visit_ring(
                    canvas,
                    (7.0 + second * 26.0) * scale,
                    (1.5 * scale).max(1.2),
                    |x, y, coverage| visit(x, y, CLICK_PALETTE[3], coverage * alpha * 0.72),
                );
            }
        }
        ClickEffect::Pulse => {
            let radius = (8.5 + (progress * std::f64::consts::PI).sin() * 3.0) * scale;
            visit_disc(canvas, radius, |x, y, coverage| {
                visit(x, y, CLICK_COLOR, coverage * alpha * 0.28)
            });
            visit_ring(
                canvas,
                (12.0 + progress * 12.0) * scale,
                (1.6 * scale).max(1.2),
                |x, y, coverage| visit(x, y, CLICK_COLOR, coverage * alpha * 0.8),
            );
        }
        ClickEffect::Sparkle => {
            let radius = (13.0 + (progress * std::f64::consts::PI).sin() * 10.0) * scale;
            visit_sparkle(canvas, radius, |x, y, coverage| {
                let horizontal = (f64::from(x) + 0.5 - canvas.center_x) / radius.max(0.001);
                let color = mix_color(CLICK_PALETTE[2], CLICK_PALETTE[4], horizontal * 0.5 + 0.5);
                visit(x, y, color, coverage * alpha)
            });
            visit_disc(canvas, 2.2 * scale, |x, y, coverage| {
                visit(x, y, CLICK_PALETTE[6], coverage * alpha)
            });
        }
        ClickEffect::Orbit => {
            let radius = (9.0 + progress * 16.0) * scale;
            let angle = -std::f64::consts::FRAC_PI_2 + progress * std::f64::consts::TAU * 1.35;
            visit_ring(canvas, radius, (0.9 * scale).max(0.8), |x, y, coverage| {
                visit(x, y, CLICK_PALETTE[4], coverage * alpha * 0.24)
            });
            for (index, offset, color) in [
                (0usize, 0.0, CLICK_COLOR),
                (1usize, std::f64::consts::PI, CLICK_PALETTE[3]),
            ] {
                for trail in 0..3 {
                    let trail_angle = angle + offset - trail as f64 * 0.18;
                    let particle = ClickCanvas {
                        center_x: canvas.center_x + trail_angle.cos() * radius,
                        center_y: canvas.center_y + trail_angle.sin() * radius,
                        ..canvas
                    };
                    visit_disc(
                        particle,
                        (2.2 - trail as f64 * 0.45).max(0.8) * scale,
                        |x, y, coverage| {
                            visit(
                                x,
                                y,
                                color,
                                coverage * alpha * (1.0 - trail as f64 * 0.28) / (index + 1) as f64,
                            )
                        },
                    );
                }
            }
            visit_disc(canvas, 1.7 * scale, |x, y, coverage| {
                visit(x, y, CLICK_PALETTE[6], coverage * alpha)
            });
        }
        ClickEffect::Radar => {
            let radius = (8.0 + progress * 25.0) * scale;
            let sweep = -std::f64::consts::FRAC_PI_2 + progress * std::f64::consts::TAU * 1.2;
            visit_ring(canvas, radius, (1.0 * scale).max(0.8), |x, y, coverage| {
                visit(x, y, CLICK_PALETTE[3], coverage * alpha * 0.35)
            });
            visit_disc(canvas, radius, |x, y, coverage| {
                let dx = f64::from(x) + 0.5 - canvas.center_x;
                let dy = f64::from(y) + 0.5 - canvas.center_y;
                let angle = dy.atan2(dx);
                let difference = (angle - sweep + std::f64::consts::PI)
                    .rem_euclid(std::f64::consts::TAU)
                    - std::f64::consts::PI;
                let difference = difference.abs();
                let wedge = (0.42 - difference).clamp(0.0, 1.0);
                let color = mix_color(CLICK_COLOR, CLICK_PALETTE[3], progress);
                visit(x, y, color, coverage * wedge * alpha * 0.48)
            });
            visit_segment(
                canvas,
                canvas.center_x,
                canvas.center_y,
                canvas.center_x + sweep.cos() * radius,
                canvas.center_y + sweep.sin() * radius,
                (1.3 * scale).max(1.0),
                |x, y, coverage| visit(x, y, CLICK_COLOR, coverage * alpha * 0.85),
            );
            let head = ClickCanvas {
                center_x: canvas.center_x + sweep.cos() * radius,
                center_y: canvas.center_y + sweep.sin() * radius,
                ..canvas
            };
            visit_disc(head, 2.0 * scale, |x, y, coverage| {
                visit(x, y, CLICK_PALETTE[6], coverage * alpha)
            });
        }
        ClickEffect::Firework => visit_firework(canvas, progress, scale, alpha, &mut visit),
        ClickEffect::Comet => visit_comet(canvas, progress, scale, alpha, &mut visit),
        ClickEffect::Confetti => visit_confetti(canvas, progress, scale, alpha, &mut visit),
        ClickEffect::Stardust => visit_stardust(canvas, progress, scale, alpha, &mut visit),
        ClickEffect::Rocket => visit_rocket(canvas, progress, scale, alpha, &mut visit),
        ClickEffect::Bloom => visit_glow(
            canvas,
            (11.0 + progress * 29.0) * scale,
            [CLICK_PALETTE[6], CLICK_PALETTE[4], CLICK_PALETTE[3]],
            alpha,
            &mut visit,
        ),
        ClickEffect::Aurora => {
            let colors = [CLICK_PALETTE[3], CLICK_PALETTE[4], CLICK_PALETTE[1]];
            visit_ring(
                canvas,
                (11.0 + progress * 20.0) * scale,
                (4.5 * scale).max(2.0),
                |x, y, coverage| {
                    let color = angular_color(canvas, x, y, &colors);
                    visit(x, y, color, coverage * alpha * 0.72)
                },
            );
        }
        ClickEffect::Prism => {
            let colors = [CLICK_PALETTE[2], CLICK_PALETTE[1], CLICK_PALETTE[3]];
            visit_disc(canvas, (7.0 + progress * 5.0) * scale, |x, y, coverage| {
                let color = angular_color(canvas, x, y, &colors);
                visit(x, y, color, coverage * alpha * 0.46)
            });
            visit_disc(canvas, 2.2 * scale, |x, y, coverage| {
                visit(x, y, CLICK_PALETTE[6], coverage * alpha)
            });
        }
        ClickEffect::Nebula => {
            let angle = progress * 1.4;
            let spread = (3.0 + progress * 7.0) * scale;
            for (offset, colors) in [
                (0.0, [CLICK_PALETTE[6], CLICK_PALETTE[4], CLICK_PALETTE[3]]),
                (
                    std::f64::consts::TAU / 3.0,
                    [CLICK_PALETTE[1], CLICK_PALETTE[4], CLICK_PALETTE[6]],
                ),
                (
                    std::f64::consts::TAU * 2.0 / 3.0,
                    [CLICK_PALETTE[3], CLICK_PALETTE[1], CLICK_PALETTE[2]],
                ),
            ] {
                let cloud = ClickCanvas {
                    center_x: canvas.center_x + (angle + offset).cos() * spread,
                    center_y: canvas.center_y + (angle + offset).sin() * spread,
                    ..canvas
                };
                visit_glow(
                    cloud,
                    (13.0 + progress * 20.0) * scale,
                    colors,
                    alpha * 0.72,
                    &mut visit,
                );
            }
            visit_disc(canvas, 1.8 * scale, |x, y, coverage| {
                visit(x, y, CLICK_PALETTE[6], coverage * alpha)
            });
        }
        ClickEffect::Rainbow => {
            let colors = [
                CLICK_PALETTE[1],
                CLICK_PALETTE[2],
                CLICK_PALETTE[5],
                CLICK_PALETTE[3],
                CLICK_PALETTE[4],
                CLICK_PALETTE[6],
            ];
            visit_ring(
                canvas,
                (10.0 + progress * 21.0) * scale,
                (4.0 * scale).max(2.2),
                |x, y, coverage| {
                    let color = angular_color(canvas, x, y, &colors);
                    visit(x, y, color, coverage * alpha * 0.78)
                },
            );
            visit_glow(
                canvas,
                (11.0 + progress * 12.0) * scale,
                [CLICK_PALETTE[6], CLICK_PALETTE[4], CLICK_PALETTE[3]],
                alpha * 0.35,
                &mut visit,
            );
        }
    }
}

fn mix_color(first: [u8; 3], second: [u8; 3], amount: f64) -> [u8; 3] {
    let amount = amount.clamp(0.0, 1.0);
    std::array::from_fn(|channel| {
        (f64::from(first[channel]) * (1.0 - amount) + f64::from(second[channel]) * amount).round()
            as u8
    })
}

fn palette_color(colors: &[[u8; 3]], position: f64) -> [u8; 3] {
    let scaled = position.rem_euclid(1.0) * colors.len() as f64;
    let index = scaled.floor() as usize % colors.len();
    mix_color(
        colors[index],
        colors[(index + 1) % colors.len()],
        scaled.fract(),
    )
}

fn angular_color(canvas: ClickCanvas, x: u32, y: u32, colors: &[[u8; 3]]) -> [u8; 3] {
    let angle = (f64::from(y) + 0.5 - canvas.center_y).atan2(f64::from(x) + 0.5 - canvas.center_x);
    palette_color(colors, angle / std::f64::consts::TAU + 0.5)
}

fn visit_firework(
    canvas: ClickCanvas,
    progress: f64,
    scale: f64,
    alpha: f64,
    mut visit: impl FnMut(u32, u32, [u8; 3], f64),
) {
    let inner = (5.0 + progress * 4.0) * scale;
    let outer = (15.0 + progress * 18.0) * scale;
    let thickness = (1.8 * scale).max(1.3);
    let left = (canvas.center_x - outer - thickness).floor().max(0.0) as u32;
    let top = (canvas.center_y - outer - thickness).floor().max(0.0) as u32;
    let right = (canvas.center_x + outer + thickness)
        .ceil()
        .min(canvas.width as f64) as u32;
    let bottom = (canvas.center_y + outer + thickness)
        .ceil()
        .min(canvas.height as f64) as u32;
    let colors = [
        CLICK_PALETTE[0],
        CLICK_PALETTE[1],
        CLICK_PALETTE[2],
        CLICK_PALETTE[4],
    ];
    for y in top..bottom {
        for x in left..right {
            let px = f64::from(x) + 0.5;
            let py = f64::from(y) + 0.5;
            let mut nearest = f64::INFINITY;
            let mut ray = 0;
            for index in 0..12 {
                let angle = index as f64 * std::f64::consts::TAU / 12.0 + 0.12;
                let (sin, cos) = angle.sin_cos();
                let distance = distance_to_segment(
                    px,
                    py,
                    canvas.center_x + cos * inner,
                    canvas.center_y + sin * inner,
                    canvas.center_x + cos * outer,
                    canvas.center_y + sin * outer,
                );
                if distance < nearest {
                    nearest = distance;
                    ray = index;
                }
            }
            let coverage = (thickness / 2.0 + 0.5 - nearest).clamp(0.0, 1.0);
            if coverage > 0.0 {
                visit(x, y, colors[ray % colors.len()], coverage * alpha);
            }
        }
    }
    visit_disc(canvas, 2.8 * scale, |x, y, coverage| {
        visit(x, y, CLICK_PALETTE[6], coverage * alpha)
    });
}

fn visit_comet(
    canvas: ClickCanvas,
    progress: f64,
    scale: f64,
    alpha: f64,
    mut visit: impl FnMut(u32, u32, [u8; 3], f64),
) {
    let direction = (0.78_f64, -0.63_f64);
    let colors = [CLICK_PALETTE[3], CLICK_PALETTE[4], CLICK_PALETTE[1]];
    for index in 0..6 {
        let along = (-5.0 + progress * 27.0 - index as f64 * 3.8) * scale;
        let bend = (index as f64 * 0.9).sin() * 1.3 * scale;
        let particle = ClickCanvas {
            center_x: canvas.center_x + direction.0 * along - direction.1 * bend,
            center_y: canvas.center_y + direction.1 * along + direction.0 * bend,
            ..canvas
        };
        let radius = (2.4 - index as f64 * 0.28).max(0.9) * scale;
        let particle_alpha = alpha * (1.0 - index as f64 / 7.0);
        visit_disc(particle, radius, |x, y, coverage| {
            visit(
                x,
                y,
                colors[index % colors.len()],
                coverage * particle_alpha,
            )
        });
    }
}

fn visit_confetti(
    canvas: ClickCanvas,
    progress: f64,
    scale: f64,
    alpha: f64,
    mut visit: impl FnMut(u32, u32, [u8; 3], f64),
) {
    let colors = [
        CLICK_PALETTE[1],
        CLICK_PALETTE[2],
        CLICK_PALETTE[3],
        CLICK_PALETTE[4],
        CLICK_PALETTE[5],
    ];
    for index in 0..14 {
        let angle = index as f64 * std::f64::consts::TAU / 14.0 + 0.18;
        let distance = (8.0 + progress * (13.0 + (index % 3) as f64 * 4.0)) * scale;
        let particle = ClickCanvas {
            center_x: canvas.center_x + angle.cos() * distance,
            center_y: canvas.center_y + angle.sin() * distance,
            ..canvas
        };
        let radius = (1.3 + (index % 3) as f64 * 0.35) * scale;
        visit_disc(particle, radius, |x, y, coverage| {
            visit(
                x,
                y,
                colors[index % colors.len()],
                coverage * alpha * (1.0 - progress * 0.35),
            )
        });
    }
}

fn visit_stardust(
    canvas: ClickCanvas,
    progress: f64,
    scale: f64,
    alpha: f64,
    mut visit: impl FnMut(u32, u32, [u8; 3], f64),
) {
    let colors = [
        CLICK_PALETTE[6],
        CLICK_PALETTE[2],
        CLICK_PALETTE[3],
        CLICK_PALETTE[4],
        CLICK_PALETTE[1],
        CLICK_PALETTE[5],
    ];
    for index in 0..20 {
        let angle = index as f64 * 2.399_963_229_728_653 + progress * 0.45;
        let distance = (3.0 + progress * (11.0 + (index % 5) as f64 * 3.2)) * scale;
        let particle = ClickCanvas {
            center_x: canvas.center_x + angle.cos() * distance,
            center_y: canvas.center_y + angle.sin() * distance,
            ..canvas
        };
        let twinkle = 0.55 + 0.45 * (index as f64 * 1.7 + progress * 18.0).sin();
        let radius = (0.75 + (index % 3) as f64 * 0.35) * scale;
        visit_disc(particle, radius, |x, y, coverage| {
            visit(
                x,
                y,
                colors[index % colors.len()],
                coverage * alpha * twinkle,
            )
        });
    }
}

fn visit_rocket(
    canvas: ClickCanvas,
    progress: f64,
    scale: f64,
    alpha: f64,
    mut visit: impl FnMut(u32, u32, [u8; 3], f64),
) {
    let direction = (0.84_f64, -0.54_f64);
    let along = (-13.0 + progress * 39.0) * scale;
    let head = ClickCanvas {
        center_x: canvas.center_x + direction.0 * along,
        center_y: canvas.center_y + direction.1 * along,
        ..canvas
    };
    let colors = [
        CLICK_PALETTE[6],
        CLICK_PALETTE[2],
        CLICK_COLOR,
        CLICK_PALETTE[1],
    ];
    visit_glow(
        head,
        7.0 * scale,
        [CLICK_PALETTE[6], CLICK_PALETTE[2], CLICK_COLOR],
        alpha * 0.42,
        &mut visit,
    );
    for index in 0..10 {
        let trail = index as f64 * 2.2 * scale;
        let wobble = (index as f64 * 0.85 + progress * 9.0).sin() * 0.65 * scale;
        let particle = ClickCanvas {
            center_x: head.center_x - direction.0 * trail - direction.1 * wobble,
            center_y: head.center_y - direction.1 * trail + direction.0 * wobble,
            ..canvas
        };
        let radius = (2.0 - index as f64 * 0.15).max(0.55) * scale;
        visit_disc(particle, radius, |x, y, coverage| {
            visit(
                x,
                y,
                colors[index % colors.len()],
                coverage * alpha * (1.0 - index as f64 / 11.0),
            )
        });
    }
    visit_disc(head, 2.3 * scale, |x, y, coverage| {
        visit(x, y, CLICK_PALETTE[6], coverage * alpha)
    });
}

fn visit_polygon_outline(
    canvas: ClickCanvas,
    points: &[(f64, f64)],
    thickness: f64,
    mut visit: impl FnMut(u32, u32, f64),
) {
    let left = points.iter().map(|(x, _)| *x).fold(f64::INFINITY, f64::min);
    let top = points.iter().map(|(_, y)| *y).fold(f64::INFINITY, f64::min);
    let right = points
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::NEG_INFINITY, f64::max);
    let bottom = points
        .iter()
        .map(|(_, y)| *y)
        .fold(f64::NEG_INFINITY, f64::max);
    let left = (left - thickness).floor().max(0.0) as u32;
    let top = (top - thickness).floor().max(0.0) as u32;
    let right = (right + thickness).ceil().min(canvas.width as f64) as u32;
    let bottom = (bottom + thickness).ceil().min(canvas.height as f64) as u32;
    for y in top..bottom {
        for x in left..right {
            let px = f64::from(x) + 0.5;
            let py = f64::from(y) + 0.5;
            let mut distance = f64::INFINITY;
            for index in 0..points.len() {
                let (ax, ay) = points[index];
                let (bx, by) = points[(index + 1) % points.len()];
                distance = distance.min(distance_to_segment(px, py, ax, ay, bx, by));
            }
            let coverage = (thickness / 2.0 + 0.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                visit(x, y, coverage);
            }
        }
    }
}

fn visit_segment(
    canvas: ClickCanvas,
    ax: f64,
    ay: f64,
    bx: f64,
    by: f64,
    thickness: f64,
    mut visit: impl FnMut(u32, u32, f64),
) {
    let left = (ax.min(bx) - thickness).floor().max(0.0) as u32;
    let top = (ay.min(by) - thickness).floor().max(0.0) as u32;
    let right = (ax.max(bx) + thickness).ceil().min(canvas.width as f64) as u32;
    let bottom = (ay.max(by) + thickness).ceil().min(canvas.height as f64) as u32;
    for y in top..bottom {
        for x in left..right {
            let distance =
                distance_to_segment(f64::from(x) + 0.5, f64::from(y) + 0.5, ax, ay, bx, by);
            let coverage = (thickness / 2.0 + 0.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                visit(x, y, coverage);
            }
        }
    }
}

fn visit_sparkle(canvas: ClickCanvas, radius: f64, mut visit: impl FnMut(u32, u32, f64)) {
    let left = (canvas.center_x - radius - 1.0).floor().max(0.0) as u32;
    let top = (canvas.center_y - radius - 1.0).floor().max(0.0) as u32;
    let right = (canvas.center_x + radius + 1.0)
        .ceil()
        .min(canvas.width as f64) as u32;
    let bottom = (canvas.center_y + radius + 1.0)
        .ceil()
        .min(canvas.height as f64) as u32;
    let points: [(f64, f64); 8] = std::array::from_fn(|index| {
        let angle = -std::f64::consts::FRAC_PI_2 + index as f64 * std::f64::consts::FRAC_PI_4;
        let length = if index % 2 == 0 {
            radius
        } else {
            radius * 0.17
        };
        (
            canvas.center_x + angle.cos() * length,
            canvas.center_y + angle.sin() * length,
        )
    });
    for y in top..bottom {
        for x in left..right {
            let px = f64::from(x) + 0.5;
            let py = f64::from(y) + 0.5;
            let mut edge_distance = f64::INFINITY;
            for index in 0..points.len() {
                let (ax, ay) = points[index];
                let (bx, by) = points[(index + 1) % points.len()];
                edge_distance = edge_distance.min(distance_to_segment(px, py, ax, ay, bx, by));
            }
            let inside = polygon_contains(&points, px, py);
            let coverage = if inside {
                (0.5 + edge_distance).clamp(0.0, 1.0)
            } else {
                (0.5 - edge_distance).clamp(0.0, 1.0)
            };
            if coverage > 0.0 {
                visit(x, y, coverage);
            }
        }
    }
}

fn polygon_contains(points: &[(f64, f64)], x: f64, y: f64) -> bool {
    let mut inside = false;
    for index in 0..points.len() {
        let (ax, ay) = points[index];
        let (bx, by) = points[(index + 1) % points.len()];
        if (ay > y) != (by > y) && x < (bx - ax) * (y - ay) / (by - ay) + ax {
            inside = !inside;
        }
    }
    inside
}

fn visit_glow(
    canvas: ClickCanvas,
    radius: f64,
    colors: [[u8; 3]; 3],
    alpha: f64,
    mut visit: impl FnMut(u32, u32, [u8; 3], f64),
) {
    let left = (canvas.center_x - radius).floor().max(0.0) as u32;
    let top = (canvas.center_y - radius).floor().max(0.0) as u32;
    let right = (canvas.center_x + radius).ceil().min(canvas.width as f64) as u32;
    let bottom = (canvas.center_y + radius).ceil().min(canvas.height as f64) as u32;
    for y in top..bottom {
        for x in left..right {
            let distance = ((f64::from(x) + 0.5 - canvas.center_x).powi(2)
                + (f64::from(y) + 0.5 - canvas.center_y).powi(2))
            .sqrt();
            let relative = distance / radius.max(0.001);
            if relative >= 1.0 {
                continue;
            }
            let coverage = (1.0 - relative).powi(2) * alpha * 0.58;
            let color = if relative < 0.35 {
                mix_color(colors[0], colors[1], relative / 0.35)
            } else {
                mix_color(colors[1], colors[2], (relative - 0.35) / 0.65)
            };
            visit(x, y, color, coverage);
        }
    }
}

fn visit_ring(
    canvas: ClickCanvas,
    radius: f64,
    thickness: f64,
    mut visit: impl FnMut(u32, u32, f64),
) {
    let left = (canvas.center_x - radius - thickness).floor().max(0.0) as u32;
    let top = (canvas.center_y - radius - thickness).floor().max(0.0) as u32;
    let right = (canvas.center_x + radius + thickness)
        .ceil()
        .min(canvas.width as f64) as u32;
    let bottom = (canvas.center_y + radius + thickness)
        .ceil()
        .min(canvas.height as f64) as u32;
    for y in top..bottom {
        for x in left..right {
            let distance = ((x as f64 + 0.5 - canvas.center_x).powi(2)
                + (y as f64 + 0.5 - canvas.center_y).powi(2))
            .sqrt();
            let coverage = (thickness / 2.0 + 0.5 - (distance - radius).abs()).clamp(0.0, 1.0);
            if coverage > 0.0 {
                visit(x, y, coverage);
            }
        }
    }
}

fn visit_disc(canvas: ClickCanvas, radius: f64, mut visit: impl FnMut(u32, u32, f64)) {
    let left = (canvas.center_x - radius - 1.0).floor().max(0.0) as u32;
    let top = (canvas.center_y - radius - 1.0).floor().max(0.0) as u32;
    let right = (canvas.center_x + radius + 1.0)
        .ceil()
        .min(canvas.width as f64) as u32;
    let bottom = (canvas.center_y + radius + 1.0)
        .ceil()
        .min(canvas.height as f64) as u32;
    for y in top..bottom {
        for x in left..right {
            let distance = ((x as f64 + 0.5 - canvas.center_x).powi(2)
                + (y as f64 + 0.5 - canvas.center_y).powi(2))
            .sqrt();
            let coverage = (radius + 0.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                visit(x, y, coverage);
            }
        }
    }
}

fn visit_cross(
    canvas: ClickCanvas,
    half_length: f64,
    thickness: f64,
    mut visit: impl FnMut(u32, u32, f64),
) {
    let left = (canvas.center_x - half_length - thickness).floor().max(0.0) as u32;
    let top = (canvas.center_y - half_length - thickness).floor().max(0.0) as u32;
    let right = (canvas.center_x + half_length + thickness)
        .ceil()
        .min(canvas.width as f64) as u32;
    let bottom = (canvas.center_y + half_length + thickness)
        .ceil()
        .min(canvas.height as f64) as u32;
    for y in top..bottom {
        for x in left..right {
            let dx = (x as f64 + 0.5 - canvas.center_x).abs();
            let dy = (y as f64 + 0.5 - canvas.center_y).abs();
            let horizontal = if dx <= half_length {
                dy
            } else {
                (dx - half_length).hypot(dy)
            };
            let vertical = if dy <= half_length {
                dx
            } else {
                (dy - half_length).hypot(dx)
            };
            let coverage = (thickness / 2.0 + 0.5 - horizontal.min(vertical)).clamp(0.0, 1.0);
            if coverage > 0.0 {
                visit(x, y, coverage);
            }
        }
    }
}

fn visit_burst(
    canvas: ClickCanvas,
    inner: f64,
    outer: f64,
    thickness: f64,
    mut visit: impl FnMut(u32, u32, f64),
) {
    let left = (canvas.center_x - outer - thickness).floor().max(0.0) as u32;
    let top = (canvas.center_y - outer - thickness).floor().max(0.0) as u32;
    let right = (canvas.center_x + outer + thickness)
        .ceil()
        .min(canvas.width as f64) as u32;
    let bottom = (canvas.center_y + outer + thickness)
        .ceil()
        .min(canvas.height as f64) as u32;
    let directions: [(f64, f64); 8] = std::array::from_fn(|ray| {
        let angle = ray as f64 * std::f64::consts::FRAC_PI_4;
        let (sin, cos) = angle.sin_cos();
        (sin, cos)
    });
    for y in top..bottom {
        for x in left..right {
            let px = x as f64 + 0.5;
            let py = y as f64 + 0.5;
            let mut distance = f64::INFINITY;
            for (sin, cos) in directions {
                distance = distance.min(distance_to_segment(
                    px,
                    py,
                    canvas.center_x + cos * inner,
                    canvas.center_y + sin * inner,
                    canvas.center_x + cos * outer,
                    canvas.center_y + sin * outer,
                ));
            }
            let coverage = (thickness / 2.0 + 0.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                visit(x, y, coverage);
            }
        }
    }
}

fn distance_to_segment(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let vx = bx - ax;
    let vy = by - ay;
    let wx = px - ax;
    let wy = py - ay;
    let t = ((wx * vx + wy * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
    (wx - t * vx).hypot(wy - t * vy)
}

fn needs_pointer_redraw(
    active_clicks: usize,
    last_click_count: usize,
    cursor_changed: bool,
    cursor_shown: bool,
) -> bool {
    active_clicks > 0 || last_click_count > 0 || cursor_changed || cursor_shown
}

fn blend_pixel(pixel: &mut image::Rgba<u8>, color: [u8; 3], alpha: f64) {
    let background_alpha = f64::from(pixel.0[3]) / 255.0;
    let output_alpha = alpha + background_alpha * (1.0 - alpha);
    if output_alpha <= 0.0 {
        return;
    }
    for (foreground, background) in color.iter().zip(pixel.0.iter_mut().take(3)) {
        let foreground = f64::from(*foreground) * alpha;
        let background_value = f64::from(*background) * background_alpha * (1.0 - alpha);
        *background = ((foreground + background_value) / output_alpha).round() as u8;
    }
    pixel.0[3] = (output_alpha * 255.0).round() as u8;
}

fn srgb_to_linear(channel: u8) -> f32 {
    let value = f32::from(channel) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn blend_hdr_pixel(width: u32, raw: &mut [u8], x: u32, y: u32, color: [f32; 3], alpha: f64) {
    let base = (y as usize * width as usize + x as usize) * 6;
    let alpha = alpha as f32;
    for (channel, sample) in raw[base..base + 6]
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .enumerate()
    {
        let background = f16_to_f32(u16::from_le_bytes([sample[0], sample[1]]));
        let value = color[channel] * alpha + background * (1.0 - alpha);
        sample.copy_from_slice(&f32_to_f16_bits(value).to_le_bytes());
    }
}

fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = ((bits >> 10) & 0x1f) as i32;
    let mantissa = u32::from(bits & 0x03ff);
    match exponent {
        0 => f32::from_bits(sign) * (mantissa as f32) / 16_777_216.0,
        31 => f32::from_bits(sign | 0x7f80_0000 | (mantissa << 13)),
        _ => f32::from_bits(sign | ((exponent + 112) as u32) << 23 | (mantissa << 13)),
    }
}

fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x007f_ffff;
    if exponent >= 31 {
        sign | 0x7c00
    } else if exponent <= 0 {
        sign
    } else {
        sign | ((exponent as u16) << 10) | ((mantissa >> 13) as u16)
    }
}

#[cfg(any(target_os = "windows", test))]
fn blend_cursor(image: &mut RgbaImage, cursor: &RgbaImage, left: i64, top: i64) -> bool {
    let mut drawn = false;
    for y in 0..cursor.height() as i64 {
        let target_y = top + y;
        if !(0..image.height() as i64).contains(&target_y) {
            continue;
        }
        for x in 0..cursor.width() as i64 {
            let target_x = left + x;
            if !(0..image.width() as i64).contains(&target_x) {
                continue;
            }
            let source = cursor.get_pixel(x as u32, y as u32);
            if source.0[3] == 0 {
                continue;
            }
            blend_pixel(
                image.get_pixel_mut(target_x as u32, target_y as u32),
                [source.0[0], source.0[1], source.0[2]],
                f64::from(source.0[3]) / 255.0,
            );
            drawn = true;
        }
    }
    drawn
}

#[cfg(target_os = "windows")]
fn blend_cursor_hdr(
    width: u32,
    height: u32,
    raw: &mut [u8],
    cursor: &RgbaImage,
    left: i64,
    top: i64,
    white_scale: f32,
) -> bool {
    let mut drawn = false;
    for y in 0..cursor.height() as i64 {
        let target_y = top + y;
        if !(0..height as i64).contains(&target_y) {
            continue;
        }
        for x in 0..cursor.width() as i64 {
            let target_x = left + x;
            if !(0..width as i64).contains(&target_x) {
                continue;
            }
            let source = cursor.get_pixel(x as u32, y as u32);
            if source.0[3] == 0 {
                continue;
            }
            let color = [
                srgb_to_linear(source.0[0]) * white_scale,
                srgb_to_linear(source.0[1]) * white_scale,
                srgb_to_linear(source.0[2]) * white_scale,
            ];
            blend_hdr_pixel(
                width,
                raw,
                target_x as u32,
                target_y as u32,
                color,
                f64::from(source.0[3]) / 255.0,
            );
            drawn = true;
        }
    }
    drawn
}

#[cfg(any(target_os = "windows", test))]
fn checked_dib_size(width: i32, height: i32) -> Option<usize> {
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;
    let bytes = width.checked_mul(4)?.checked_mul(height)?;
    // GDI's section size is a DWORD; do not truncate a large cursor before
    // passing it to CreateDIBSection or constructing the pixel slice.
    u32::try_from(bytes).ok().map(|bytes| bytes as usize)
}

#[cfg(target_os = "windows")]
fn windows_cursor_image(handle: isize) -> Option<(RgbaImage, (f64, f64))> {
    use std::mem::{size_of, zeroed};
    use windows::Win32::{
        Foundation::HANDLE,
        Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GetObjectW, HBITMAP, HDC, HGDIOBJ,
            SelectObject,
        },
        UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx, GetIconInfo, HICON, ICONINFO},
    };

    // Every GDI allocation in this function has one owner. In particular, an
    // invalid DIB pointer or an early SelectObject failure cannot leak the icon
    // bitmaps or DC through a hand-written cleanup branch.
    struct CursorGdiResources {
        color: Option<HBITMAP>,
        mask: Option<HBITMAP>,
        dc: Option<HDC>,
        bitmap: Option<HBITMAP>,
        previous: Option<HGDIOBJ>,
    }

    impl Drop for CursorGdiResources {
        fn drop(&mut self) {
            unsafe {
                if let (Some(dc), Some(previous)) = (self.dc, self.previous) {
                    SelectObject(dc, previous);
                }
                if let Some(bitmap) = self.bitmap {
                    let _ = DeleteObject(bitmap.into());
                }
                if let Some(dc) = self.dc {
                    let _ = DeleteDC(dc);
                }
                if let Some(color) = self.color {
                    let _ = DeleteObject(color.into());
                }
                if let Some(mask) = self.mask {
                    let _ = DeleteObject(mask.into());
                }
            }
        }
    }

    unsafe {
        let mut icon = ICONINFO::default();
        GetIconInfo(HICON(handle as *mut _), &mut icon).ok()?;
        let mut resources = CursorGdiResources {
            color: Some(icon.hbmColor),
            mask: Some(icon.hbmMask),
            dc: None,
            bitmap: None,
            previous: None,
        };
        let mut mask = BITMAP::default();
        if GetObjectW(
            resources.mask?.into(),
            size_of::<BITMAP>() as i32,
            Some((&mut mask as *mut BITMAP).cast()),
        ) == 0
        {
            return None;
        }
        let width = mask.bmWidth.max(1);
        let height = mask.bmHeight.max(1);
        let byte_len = checked_dib_size(width, height)?;
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        resources.dc = Some(dc);
        let mut bits = std::ptr::null_mut();
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..zeroed()
            },
            bmiColors: [zeroed()],
        };
        let bitmap = match CreateDIBSection(
            Some(dc),
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            Some(HANDLE::default()),
            0,
        ) {
            Ok(bitmap) => bitmap,
            Err(_) => return None,
        };
        resources.bitmap = Some(bitmap);
        if bits.is_null() {
            log::warn!("CreateDIBSection returned a cursor bitmap with null bits");
            return None;
        }
        let previous = SelectObject(dc, bitmap.into());
        if previous.is_invalid() {
            return None;
        }
        resources.previous = Some(previous);
        let bgra = std::slice::from_raw_parts_mut(bits.cast::<u8>(), byte_len);
        for pixel in bgra.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[0, 0, 0, 0]);
        }
        let result = DrawIconEx(
            HDC(dc.0),
            0,
            0,
            HICON(handle as *mut _),
            width,
            height,
            0,
            None,
            DI_NORMAL,
        );
        let image = RgbaImage::from_fn(width as u32, height as u32, |x, y| {
            let pixel = &bgra[(y as usize * width as usize + x as usize) * 4..][..4];
            if result.is_ok() {
                image::Rgba([pixel[2], pixel[1], pixel[0], pixel[3]])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        result
            .is_ok()
            .then_some((image, (f64::from(icon.xHotspot), f64::from(icon.yHotspot))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_dib_size_is_checked() {
        assert_eq!(checked_dib_size(32, 32), Some(32 * 32 * 4));
        assert_eq!(checked_dib_size(0, 32), Some(0));
        assert!(checked_dib_size(-1, 32).is_none());
        assert!(checked_dib_size(i32::MAX, i32::MAX).is_none());
    }

    #[test]
    fn hdr_blending_keeps_the_frame_in_linear_scrgb() {
        let mut raw = vec![0; 4 * 4 * 6];
        blend_hdr_pixel(4, &mut raw, 1, 1, [1.0, 0.5, 0.25], 1.0);
        let image = HdrImage::new(4, 4, raw, 203.0).with_sdr_white(203.0);
        assert_eq!(image.reference_white_nits(), 203.0);
        assert_eq!(image.sdr_white_nits(), 203.0);
        assert_eq!(image.pixel_f32(1, 1), [1.0, 0.5, 0.25]);
        assert_eq!(image.pixel_f32(0, 0), [0.0; 3]);
    }

    #[test]
    fn click_positions_map_from_global_monitor_units_to_physical_pixels() {
        let region = Region::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(
            click_center(1_920, 100, region, (1_935, 145), 200, 100),
            (10.0, 50.0)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_pointer_location_uses_a_real_event() {
        use objc2_core_foundation::CGPoint;
        use objc2_core_graphics::CGEvent;

        let event = CGEvent::new(None).expect("mouse event");
        let expected = CGPoint::new(321.5, 654.25);
        CGEvent::set_location(Some(&event), expected);
        assert_eq!(CGEvent::location(Some(&event)), expected);
    }

    fn render_effect(effect: ClickEffect, progress: f64) -> RgbaImage {
        let mut image = RgbaImage::from_pixel(64, 64, image::Rgba([0, 0, 0, 255]));
        effect.visit_preview(progress, 1.0, |x, y, color, opacity| {
            blend_pixel(image.get_pixel_mut(x, y), color, opacity);
        });
        image
    }

    #[test]
    fn click_effects_draw_distinct_shapes() {
        let ring = render_effect(ClickEffect::Ring, 0.0);
        assert_eq!(ring.get_pixel(32, 20).0, [238, 155, 44, 255]);
        assert_eq!(ring.get_pixel(32, 32).0, [0, 0, 0, 255]);

        let dot = render_effect(ClickEffect::Dot, 0.0);
        assert_eq!(dot.get_pixel(32, 32).0, [238, 155, 44, 255]);
        assert_eq!(dot.get_pixel(32, 20).0, [0, 0, 0, 255]);

        let cross = render_effect(ClickEffect::Cross, 0.0);
        assert_eq!(cross.get_pixel(32, 32).0, [238, 155, 44, 255]);
        assert_eq!(cross.get_pixel(32, 24).0, [238, 155, 44, 255]);
        assert_eq!(cross.get_pixel(40, 40).0, [0, 0, 0, 255]);

        let burst = render_effect(ClickEffect::Burst, 0.0);
        assert_eq!(burst.get_pixel(46, 32).0, [238, 155, 44, 255]);
        assert_eq!(burst.get_pixel(32, 32).0, [0, 0, 0, 255]);
    }

    #[test]
    fn every_click_effect_draws_and_fades_out() {
        for effect in ClickEffect::ALL {
            let image = render_effect(effect, 0.0);
            assert!(
                image.pixels().any(|pixel| pixel.0 != [0, 0, 0, 255]),
                "{effect:?} did not draw"
            );
            let image = render_effect(effect, 1.0);
            assert!(
                image.pixels().all(|pixel| pixel.0 == [0, 0, 0, 255]),
                "{effect:?} did not fade out"
            );
        }
    }

    #[test]
    fn particle_and_glow_effects_emit_multiple_colors() {
        for effect in [
            ClickEffect::Sparkle,
            ClickEffect::Firework,
            ClickEffect::Comet,
            ClickEffect::Confetti,
            ClickEffect::Bloom,
            ClickEffect::Aurora,
            ClickEffect::Prism,
            ClickEffect::Orbit,
            ClickEffect::Radar,
            ClickEffect::Stardust,
            ClickEffect::Rocket,
            ClickEffect::Nebula,
            ClickEffect::Rainbow,
        ] {
            let mut colors = std::collections::HashSet::new();
            effect.visit_preview(0.0, 1.0, |_, _, color, _| {
                colors.insert(color);
            });
            assert!(colors.len() > 1, "{effect:?} did not emit multiple colors");
        }
    }

    #[test]
    fn compositing_reuses_unique_allocations_and_copies_shared_frames() {
        let image = Arc::new(RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255])));
        let allocation = image.as_raw().as_ptr();
        let output = sdr_pixels_for_composite(image);
        assert_eq!(output.as_raw().as_ptr(), allocation);

        let image = Arc::new(RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255])));
        let shared = image.clone();
        let output = sdr_pixels_for_composite(shared);
        assert_ne!(output.as_raw().as_ptr(), image.as_raw().as_ptr());
        assert!(image.pixels().all(|pixel| pixel.0 == [10, 20, 30, 255]));

        let raw = vec![0; 4 * 4 * 6];
        let allocation = raw.as_ptr();
        let image = Arc::new(HdrImage::new(4, 4, raw, 203.0));
        let output = hdr_pixels_for_composite(image);
        assert_eq!(output.raw.as_ptr(), allocation);
        assert_eq!((output.width, output.height), (4, 4));
        assert_eq!(output.reference_white, 203.0);
    }

    #[test]
    fn unchanged_visible_cursor_still_needs_compositing() {
        assert!(needs_pointer_redraw(0, 0, false, true));
        assert!(!needs_pointer_redraw(0, 0, false, false));
        assert!(needs_pointer_redraw(0, 1, false, false));
        assert!(needs_pointer_redraw(1, 0, false, false));
        assert!(needs_pointer_redraw(0, 0, true, false));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_timeout_redraws_use_the_latest_clean_frame() {
        let Some(monitor) = snapit_capture::monitors()
            .ok()
            .and_then(|monitors| monitors.into_iter().next())
        else {
            return;
        };
        let region = Region::new(0.0, 0.0, 8.0, 8.0);
        let mut state = PointerState::new(monitor.id, region, true, true, ClickEffect::Dot);
        // No pointer redraw is needed yet: the incoming allocation is returned
        // directly, but its clean pixels are retained for later timeouts.
        state.cursor_dirty = false;
        let clean = Arc::new(RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255])));
        let unchanged = state.apply(FrameData::Sdr(clean.clone()), Duration::ZERO);
        assert!(matches!(unchanged, FrameData::Sdr(ref image) if Arc::ptr_eq(image, &clean)));
        assert!(
            matches!(state.base, Some(FrameData::Sdr(ref image)) if Arc::ptr_eq(image, &clean))
        );

        // Even the monitor-unavailable early return refreshes the retained base.
        let latest = Arc::new(RgbaImage::from_pixel(8, 8, image::Rgba([30, 20, 10, 255])));
        state.monitor = None;
        state.apply(FrameData::Sdr(latest.clone()), Duration::from_millis(1));
        assert!(
            matches!(state.base, Some(FrameData::Sdr(ref image)) if Arc::ptr_eq(image, &latest))
        );
        state.monitor = Some(monitor.clone());

        let mut cursor = RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        cursor.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        state.cursor = Some(CursorState {
            handle: 1,
            visible: true,
            image: Some(cursor),
            hotspot: (0.0, 0.0),
        });
        state.position = Some((monitor.x + 1, monitor.y + 1));
        state.cursor_dirty = true;
        state.pointer_dirty = true;
        let drawn = state
            .apply_timeout(Duration::from_millis(10))
            .expect("cursor movement redraw");
        let FrameData::Sdr(drawn) = drawn else {
            panic!("cursor redraw changed the frame variant");
        };
        assert_eq!(drawn.get_pixel(1, 1).0, [255, 0, 0, 255]);
        assert_eq!(drawn.get_pixel(5, 5).0, [30, 20, 10, 255]);

        // Moving again must erase the old cursor by starting from `latest`, not
        // from the previously composited timeout frame.
        state.position = Some((monitor.x + 4, monitor.y + 1));
        state.cursor_dirty = true;
        state.pointer_dirty = true;
        let moved = state
            .apply_timeout(Duration::from_millis(20))
            .expect("second cursor redraw");
        let FrameData::Sdr(moved) = moved else {
            panic!("cursor redraw changed the frame variant");
        };
        assert_eq!(moved.get_pixel(1, 1).0, [30, 20, 10, 255]);
        assert_eq!(moved.get_pixel(4, 1).0, [255, 0, 0, 255]);

        // Clicks use the same clean base, and the final redraw removes the
        // expired effect without removing the cursor.
        state.clicks.push(Click {
            position: (monitor.x + 5, monitor.y + 5),
            started: Duration::from_millis(20),
        });
        state.pointer_dirty = true;
        let clicked = state
            .apply_timeout(Duration::from_millis(20))
            .expect("click redraw");
        let FrameData::Sdr(clicked) = clicked else {
            panic!("click redraw changed the frame variant");
        };
        assert_eq!(
            clicked.get_pixel(5, 5).0,
            [CLICK_COLOR[0], CLICK_COLOR[1], CLICK_COLOR[2], 255]
        );
        assert_eq!(clicked.get_pixel(4, 1).0, [255, 0, 0, 255]);

        state.pointer_dirty = true;
        let faded = state
            .apply_timeout(Duration::from_millis(500))
            .expect("expired click redraw");
        let FrameData::Sdr(faded) = faded else {
            panic!("click redraw changed the frame variant");
        };
        assert_eq!(faded.get_pixel(5, 5).0, [30, 20, 10, 255]);
        assert_eq!(faded.get_pixel(4, 1).0, [255, 0, 0, 255]);
        assert!(
            matches!(state.base, Some(FrameData::Sdr(ref image)) if Arc::ptr_eq(image, &latest))
        );
    }

    #[test]
    fn cursor_blending_preserves_transparency() {
        let mut image = RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]));
        let mut cursor = RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 0]));
        cursor.put_pixel(0, 0, image::Rgba([255, 0, 0, 128]));
        assert!(blend_cursor(&mut image, &cursor, 0, 0));
        assert_eq!(image.get_pixel(1, 1), &image::Rgba([0, 0, 0, 255]));
        assert_eq!(image.get_pixel(0, 0).0[0], 128);
    }
}
