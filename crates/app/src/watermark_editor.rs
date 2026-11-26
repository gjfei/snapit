use std::{
    cell::Cell,
    collections::BTreeSet,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use gpui_kit::AppContext as _;
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Selectable as _, Sizable as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex,
    input::{AnyInputState, Input, InputEvent, InputState, NumberInput, Textarea, TextareaState},
    label::Label,
    scroll::ScrollableElement as _,
    searchable_list::SearchableVec,
    select::{Select, SelectEvent, SelectState},
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    switch::Switch,
    tab::{Tab, TabBar},
    toolbar::Toolbar,
    v_flex,
};
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, AsyncApp, Bounds, ClipboardEntry, Context, CursorStyle,
    DisplayId, ElementId, Entity, ExternalPaths, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    ParentElement as _, PathPromptOptions, PinchEvent, Pixels, Point, Render, RenderImage,
    ScrollEvent, SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _,
    Subscription, Task, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, actions,
    canvas, container_query, div, fill, img, point, prelude::FluentBuilder as _, px, rgb, size,
};
use snapit_capture::ElementBounds;
use snapit_store::{
    WatermarkConfig, WatermarkElement, WatermarkKind, WatermarkPlacement, WatermarkPreset,
    WatermarkTarget,
};

use crate::{appearance, controller::Controller, settings::AppSettings};

const CONTEXT: &str = "SnapItWatermarkEditor";
const PREVIEW_SCALE: f32 = 2.0;
const CHECKER_TILE_PX: f32 = 12.0;
const HIT_SLOP_PX: f32 = 4.0;
const HANDLE_HIT_RADIUS_PX: f32 = 8.0;
const ROTATE_STEM_PX: f32 = 18.0;
const SNAP_THRESHOLD_PX: f32 = 5.0;
const MAX_UNDO_STEPS: usize = 50;
const TEMPLATE_VARIABLES: [&str; 4] = ["{date}", "{time}", "{datetime}", "{year}"];

actions!(
    snapit_watermark_editor,
    [
        DeleteElement,
        DuplicateElement,
        UndoEdit,
        RedoEdit,
        PasteImage,
        NudgeUp,
        NudgeDown,
        NudgeLeft,
        NudgeRight,
        NudgeUpFast,
        NudgeDownFast,
        NudgeLeftFast,
        NudgeRightFast,
        CloseEditor
    ]
);

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", DeleteElement, Some(CONTEXT)),
        KeyBinding::new("delete", DeleteElement, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("d"), DuplicateElement, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("z"), UndoEdit, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("shift-z"), RedoEdit, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-y", RedoEdit, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("v"), PasteImage, Some(CONTEXT)),
        KeyBinding::new("up", NudgeUp, Some(CONTEXT)),
        KeyBinding::new("down", NudgeDown, Some(CONTEXT)),
        KeyBinding::new("left", NudgeLeft, Some(CONTEXT)),
        KeyBinding::new("right", NudgeRight, Some(CONTEXT)),
        KeyBinding::new("shift-up", NudgeUpFast, Some(CONTEXT)),
        KeyBinding::new("shift-down", NudgeDownFast, Some(CONTEXT)),
        KeyBinding::new("shift-left", NudgeLeftFast, Some(CONTEXT)),
        KeyBinding::new("shift-right", NudgeRightFast, Some(CONTEXT)),
        KeyBinding::new("escape", CloseEditor, Some(CONTEXT)),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatermarkStarter {
    Blank,
    CleanSignature,
    BrandTimestamp,
    BadgePill,
    DiagonalTiled,
}

impl WatermarkStarter {
    pub(crate) const ALL: [Self; 5] = [
        Self::Blank,
        Self::CleanSignature,
        Self::BrandTimestamp,
        Self::BadgePill,
        Self::DiagonalTiled,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Blank => "blank",
            Self::CleanSignature => "signature",
            Self::BrandTimestamp => "brand-time",
            Self::BadgePill => "badge",
            Self::DiagonalTiled => "diagonal",
        }
    }

    pub(crate) fn label(self) -> String {
        match self {
            Self::Blank => t!("settings.watermark.starter.blank").to_string(),
            Self::CleanSignature => t!("settings.watermark.starter.signature").to_string(),
            Self::BrandTimestamp => t!("settings.watermark.starter.brand_time").to_string(),
            Self::BadgePill => t!("settings.watermark.starter.badge").to_string(),
            Self::DiagonalTiled => t!("settings.watermark.starter.diagonal").to_string(),
        }
    }

    pub(crate) fn preset(self) -> WatermarkPreset {
        match self {
            Self::Blank => WatermarkPreset::default(),
            Self::CleanSignature => {
                let mut el = WatermarkElement::new_text("el-1", "SnapIt");
                el.opacity = 0.88;
                el.text.font_size = 22.0;
                el.text.bold = true;
                el.text.shadow = true;
                let mut preset = WatermarkPreset::default();
                preset.elements = vec![el];
                preset
            }
            Self::BrandTimestamp => {
                let mut brand = WatermarkElement::new_text("el-1", "SnapIt");
                brand.opacity = 0.9;
                brand.text.font_size = 22.0;
                brand.text.bold = true;
                brand.text.shadow = true;

                let mut time = WatermarkElement::new_text("el-2", "{date} {time}");
                time.y = 30.0;
                time.opacity = 0.72;
                time.text.font_size = 13.0;
                time.text.shadow = true;

                let mut preset = WatermarkPreset::default();
                preset.elements = vec![brand, time];
                preset
            }
            Self::BadgePill => {
                let mut el = WatermarkElement::new_text("el-1", "SnapIt · {date}");
                el.opacity = 0.95;
                el.text.font_size = 16.0;
                el.text.bold = true;
                el.text.background = true;
                el.text.background_color = 0x0F_17_2A;
                el.text.background_opacity = 0.72;
                let mut preset = WatermarkPreset::default();
                preset.elements = vec![el];
                preset
            }
            Self::DiagonalTiled => {
                let mut el = WatermarkElement::new_text("el-1", "SnapIt");
                el.opacity = 0.24;
                el.text.font_size = 28.0;
                el.text.bold = true;
                let mut preset = WatermarkPreset::default();
                preset.elements = vec![el];
                preset.rotation_degrees = -28.0;
                preset.tiled = true;
                preset.tile_spacing_x = 180.0;
                preset.tile_spacing_y = 110.0;
                preset
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WatermarkChoice {
    pub value: String,
    pub title: SharedString,
}

impl gpui_kit::component::searchable_list::SearchableListItem for WatermarkChoice {
    type Value = String;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

#[derive(Clone)]
enum TemplateAction {
    Load(String),
    New,
    Duplicate,
    Delete(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorMode {
    Design,
    Layout,
}

const NUMBER_FIELDS: [NumberField; 9] = [
    NumberField::FontSize,
    NumberField::ImageWidth,
    NumberField::OffsetX,
    NumberField::OffsetY,
    NumberField::StrokeWidth,
    NumberField::MarginX,
    NumberField::MarginY,
    NumberField::TileSpacingX,
    NumberField::TileSpacingY,
];

const SLIDER_FIELDS: [SliderField; 5] = [
    SliderField::Rotation,
    SliderField::Scale,
    SliderField::Opacity,
    SliderField::BackgroundOpacity,
    SliderField::CornerRadius,
];

#[derive(Clone, Copy)]
enum NumberField {
    FontSize,
    ImageWidth,
    OffsetX,
    OffsetY,
    StrokeWidth,
    MarginX,
    MarginY,
    TileSpacingX,
    TileSpacingY,
}

#[derive(Clone)]
struct LayoutPreviewDrag {
    press: Point<Pixels>,
    start_margin_x: f32,
    start_margin_y: f32,
    ratio: f32,
    placement: WatermarkPlacement,
    start_preset: WatermarkPreset,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SliderField {
    Rotation,
    Scale,
    Opacity,
    BackgroundOpacity,
    CornerRadius,
}

#[derive(Clone, Copy)]
enum AlignOp {
    Left,
    CenterH,
    CenterV,
    Row,
    Column,
}

#[derive(Clone)]
struct StampPreview {
    image: Arc<RenderImage>,
    logical_size: (f32, f32),
    origin: (f32, f32),
    element_bounds: Vec<(String, ElementBounds)>,
}

#[derive(Clone, Copy)]
struct WorldBox {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone, Copy)]
struct PreviewLayout {
    stamp_origin: Point<Pixels>,
    zoom: f32,
    camera_center: (f32, f32),
    stamp_world_origin: (f32, f32),
}

#[derive(Clone, Copy)]
struct SnapGuide {
    vertical: bool,
    world_pos: f32,
}

#[derive(Clone)]
enum CanvasGesture {
    Move {
        element_id: String,
        press: Point<Pixels>,
        start_x: f32,
        start_y: f32,
        start_world_box: WorldBox,
        siblings: Vec<WorldBox>,
        zoom: f32,
        camera_center: (f32, f32),
        start_preset: WatermarkPreset,
    },
    Resize {
        element_id: String,
        center: Point<Pixels>,
        start_dist: f32,
        start_size: f32,
        zoom: f32,
        camera_center: (f32, f32),
        start_preset: WatermarkPreset,
    },
    Rotate {
        element_id: String,
        center: Point<Pixels>,
        start_angle: f32,
        start_rotation: f32,
        zoom: f32,
        camera_center: (f32, f32),
        start_preset: WatermarkPreset,
    },
}

impl CanvasGesture {
    fn locked_camera(&self) -> (f32, (f32, f32)) {
        match self {
            Self::Move {
                zoom,
                camera_center,
                ..
            }
            | Self::Resize {
                zoom,
                camera_center,
                ..
            }
            | Self::Rotate {
                zoom,
                camera_center,
                ..
            } => (*zoom, *camera_center),
        }
    }

    fn start_preset(&self) -> &WatermarkPreset {
        match self {
            Self::Move { start_preset, .. }
            | Self::Resize { start_preset, .. }
            | Self::Rotate { start_preset, .. } => start_preset,
        }
    }
}

struct PendingWatermarkAssets {
    directory: PathBuf,
    names: Vec<String>,
    saved_references: BTreeSet<String>,
}

impl PendingWatermarkAssets {
    fn new(watermark: &snapit_store::WatermarkConfig) -> Self {
        Self {
            directory: PathBuf::new(),
            names: Vec::new(),
            saved_references: watermark
                .templates
                .values()
                .flat_map(|template| watermark_image_references(&template.preset))
                .collect(),
        }
    }

    fn track(&mut self, directory: &Path, source: String) {
        if self.directory.as_os_str().is_empty() {
            self.directory = directory.to_path_buf();
        }
        if !self.names.contains(&source) {
            self.names.push(source);
        }
    }

    fn cleanup(&mut self, retained: &BTreeSet<String>) {
        for name in self.names.drain(..) {
            if retained.contains(&name) || self.saved_references.contains(&name) {
                continue;
            }
            let path = self.directory.join(&name);
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!(
                    "failed to remove unused watermark image {}: {error}",
                    path.display()
                );
            }
        }
    }
}

impl Drop for PendingWatermarkAssets {
    fn drop(&mut self) {
        self.cleanup(&BTreeSet::new());
    }
}

fn watermark_image_references(preset: &WatermarkPreset) -> BTreeSet<String> {
    preset
        .elements
        .iter()
        .filter(|element| element.kind == WatermarkKind::Image)
        .map(|element| element.image.source.clone())
        .collect()
}

fn watermark_retained_references(
    preset: &WatermarkPreset,
    undo: &[WatermarkPreset],
    redo: &[WatermarkPreset],
    slider: Option<&WatermarkPreset>,
) -> BTreeSet<String> {
    let mut retained = watermark_image_references(preset);
    for snapshot in undo.iter().chain(redo) {
        retained.extend(watermark_image_references(snapshot));
    }
    if let Some(snapshot) = slider {
        retained.extend(watermark_image_references(snapshot));
    }
    retained
}

pub(crate) struct WatermarkEditor {
    window_handle: AnyWindowHandle,
    target: WatermarkTarget,
    template_id: Option<String>,
    fallback_name: String,
    name_input: Entity<InputState>,
    name_error: Option<String>,
    template_select: Entity<SelectState<SearchableVec<WatermarkChoice>>>,
    last_template_choices: Vec<WatermarkChoice>,
    preset: WatermarkPreset,
    undo_stack: Vec<WatermarkPreset>,
    redo_stack: Vec<WatermarkPreset>,
    slider_undo_snapshot: Option<WatermarkPreset>,
    selected_id: Option<String>,
    canvas_focus: FocusHandle,
    text_input: Entity<TextareaState>,
    text_focus: FocusHandle,
    color: Entity<ColorPickerState>,
    stroke_color: Entity<ColorPickerState>,
    background_color: Entity<ColorPickerState>,
    sliders: [Entity<SliderState>; 5],
    number_inputs: [Entity<InputState>; 9],
    shown_numbers: [f64; 9],
    syncing_controls: bool,
    mode: EditorMode,
    zoom_override: Option<f32>,
    preview: Option<StampPreview>,
    preview_revision: u64,
    requested_preview_revision: u64,
    preview_pending: bool,
    preview_error: Option<String>,
    preview_worker_running: bool,
    preview_task: Option<Task<()>>,
    layout_preview: Option<Arc<RenderImage>>,
    layout_preview_revision: u64,
    requested_layout_preview_revision: u64,
    layout_preview_error: Option<String>,
    layout_preview_worker_running: bool,
    layout_preview_task: Option<Task<()>>,
    layout_drag: Option<LayoutPreviewDrag>,
    pending_assets: PendingWatermarkAssets,
    preview_layout: Rc<Cell<Option<PreviewLayout>>>,
    gesture: Option<CanvasGesture>,
    snap_guides: Vec<SnapGuide>,
    hover_cursor: CursorStyle,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl WatermarkEditor {
    fn new(
        target: WatermarkTarget,
        template_id: Option<String>,
        fallback_name: String,
        preset: WatermarkPreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let selected_id = preset.elements.first().map(|el| el.id.clone());
        let first_text = preset.elements.first().map(|el| &el.text);
        let initial_text = first_text
            .map(|text| text.content.clone())
            .unwrap_or_default();
        let initial_color = first_text.map_or(0xFF_FF_FF, |text| text.color);
        let initial_stroke_color = first_text.map_or(0x00_00_00, |text| text.stroke_color);
        let initial_bg_color = first_text.map_or(0x0F_17_2A, |text| text.background_color);

        let canvas_focus = cx.focus_handle();
        let text_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .default_value(initial_text)
                .placeholder(t!("settings.watermark.text_content").to_string())
        });
        let text_focus = AnyInputState::Textarea(text_input.clone()).focus_handle(cx);
        let text_subscription = cx.subscribe(&text_input, |this, input, event, cx| {
            if this.syncing_controls || !matches!(event, InputEvent::Change) {
                return;
            }
            let value = input.read(cx).value().to_string();
            let changed = this
                .selected_element()
                .is_some_and(|el| el.kind == WatermarkKind::Text && el.text.content != value);
            if changed {
                this.push_undo_snapshot();
                if let Some(element) = this.selected_element_mut() {
                    element.text.content = value;
                }
                this.error = None;
                this.refresh_preview(cx);
            }
        });

        let color = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(Hsla::from(rgb(initial_color)))
        });
        let color_subscription = cx.subscribe(&color, |this, _, event, cx| {
            if this.syncing_controls {
                return;
            }
            let ColorPickerEvent::Change(Some(color)) = event else {
                return;
            };
            let rgb_value = color_to_rgb(*color);
            let changed = this
                .selected_element()
                .is_some_and(|el| el.kind == WatermarkKind::Text && el.text.color != rgb_value);
            if changed {
                this.push_undo_snapshot();
                if let Some(element) = this.selected_element_mut() {
                    element.text.color = rgb_value;
                }
                this.refresh_preview(cx);
            }
        });

        let stroke_color = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(Hsla::from(rgb(initial_stroke_color)))
        });
        let stroke_color_subscription = cx.subscribe(&stroke_color, |this, _, event, cx| {
            if this.syncing_controls {
                return;
            }
            let ColorPickerEvent::Change(Some(color)) = event else {
                return;
            };
            let rgb_value = color_to_rgb(*color);
            let changed = this.selected_element().is_some_and(|el| {
                el.kind == WatermarkKind::Text && el.text.stroke_color != rgb_value
            });
            if changed {
                this.push_undo_snapshot();
                if let Some(element) = this.selected_element_mut() {
                    element.text.stroke_color = rgb_value;
                }
                this.refresh_preview(cx);
            }
        });

        let background_color = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(Hsla::from(rgb(initial_bg_color)))
        });
        let background_color_subscription =
            cx.subscribe(&background_color, |this, _, event, cx| {
                if this.syncing_controls {
                    return;
                }
                let ColorPickerEvent::Change(Some(color)) = event else {
                    return;
                };
                let rgb_value = color_to_rgb(*color);
                let changed = this.selected_element().is_some_and(|el| {
                    el.kind == WatermarkKind::Text && el.text.background_color != rgb_value
                });
                if changed {
                    this.push_undo_snapshot();
                    if let Some(element) = this.selected_element_mut() {
                        element.text.background_color = rgb_value;
                    }
                    this.refresh_preview(cx);
                }
            });

        let sliders = std::array::from_fn(|index| {
            let field = SLIDER_FIELDS[index];
            let (min, max, step) = slider_range(field);
            let default_value = match field {
                SliderField::Rotation => preset
                    .elements
                    .first()
                    .map_or(preset.rotation_degrees, |el| el.rotation_degrees),
                SliderField::Scale => preset.scale,
                SliderField::Opacity => preset.elements.first().map_or(0.65, |el| el.opacity),
                SliderField::BackgroundOpacity => first_text.map_or(0.55, |t| t.background_opacity),
                SliderField::CornerRadius => preset
                    .elements
                    .first()
                    .map_or(0.0, |el| el.image.corner_radius),
            };
            cx.new(|_| {
                SliderState::new()
                    .min(min)
                    .max(max)
                    .step(step)
                    .default_value(default_value)
            })
        });

        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(fallback_name.clone())
                .placeholder(t!("settings.watermark.template.name").to_string())
        });
        let name_subscription = cx.subscribe(&name_input, |this, input, event, cx| {
            if this.syncing_controls || !matches!(event, InputEvent::Change) {
                return;
            }
            this.fallback_name = input.read(cx).value().to_string();
            this.name_error = None;
            cx.notify();
        });

        let choices = template_choices(&AppSettings::get(cx).watermark);
        let selected_index = template_id
            .as_deref()
            .and_then(|id| choices.iter().position(|choice| choice.value == id))
            .map(|index| IndexPath::default().row(index));
        let template_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::from(choices.clone()),
                selected_index,
                window,
                cx,
            )
            .searchable(true)
        });
        let template_subscription = cx.subscribe_in(
            &template_select,
            window,
            |this, _, event: &SelectEvent<SearchableVec<WatermarkChoice>>, window, cx| {
                let SelectEvent::Confirm(Some(id)) = event else {
                    return;
                };
                if this.template_id.as_deref() == Some(id.as_str()) {
                    return;
                }
                this.confirm_discard(window, cx, TemplateAction::Load(id.clone()));
            },
        );

        let mut subscriptions = vec![
            appearance::follow_system_theme(window),
            text_subscription,
            color_subscription,
            stroke_color_subscription,
            background_color_subscription,
            name_subscription,
            template_subscription,
        ];
        for (index, state) in sliders.iter().enumerate() {
            let field = SLIDER_FIELDS[index];
            subscriptions.push(cx.subscribe(state, move |this, _, event, cx| {
                if this.syncing_controls {
                    return;
                }
                match event {
                    SliderEvent::Change(SliderValue::Single(value)) => {
                        if this.slider_undo_snapshot.is_none() {
                            this.slider_undo_snapshot = Some(this.preset.clone());
                        }
                        this.set_slider(field, *value);
                        this.refresh_preview(cx);
                    }
                    SliderEvent::Release(SliderValue::Single(value)) => {
                        let start = this
                            .slider_undo_snapshot
                            .take()
                            .unwrap_or_else(|| this.preset.clone());
                        this.set_slider(field, *value);
                        if this.preset != start {
                            this.push_specific_undo(start);
                        }
                        this.refresh_preview(cx);
                    }
                    _ => {}
                }
            }));
        }

        let shown_numbers = [
            24.0,
            120.0,
            0.0,
            0.0,
            0.0,
            preset.margin_x as f64,
            preset.margin_y as f64,
            preset.tile_spacing_x as f64,
            preset.tile_spacing_y as f64,
        ];
        let number_inputs = std::array::from_fn(|index| {
            let field = NUMBER_FIELDS[index];
            let (min, max, step) = number_range(field);
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(shown_numbers[index].to_string())
                    .step(step)
                    .min(min)
                    .max(max)
            });
            subscriptions.push(cx.subscribe(&input, move |this, input, event, cx| {
                if this.syncing_controls || !matches!(event, InputEvent::Change) {
                    return;
                }
                let text = input.read(cx).value();
                if text == format_number(this.shown_numbers[index]) {
                    return;
                }
                let Ok(parsed) = text.parse::<f64>() else {
                    return;
                };
                if !parsed.is_finite() {
                    return;
                }
                let parsed = parsed.clamp(min, max);
                this.push_undo_snapshot();
                this.set_number(field, parsed);
                this.shown_numbers[index] = parsed;
                if field.is_layout() {
                    this.refresh_layout_preview(cx);
                } else {
                    this.refresh_preview(cx);
                }
            }));
            input
        });

        let mut editor = Self {
            window_handle: window.window_handle(),
            target,
            template_id,
            fallback_name,
            name_input,
            name_error: None,
            template_select,
            last_template_choices: choices,
            preset,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            slider_undo_snapshot: None,
            selected_id,
            canvas_focus,
            text_input,
            text_focus,
            color,
            stroke_color,
            background_color,
            sliders,
            number_inputs,
            shown_numbers,
            syncing_controls: false,
            mode: EditorMode::Design,
            zoom_override: None,
            preview: None,
            preview_revision: 0,
            requested_preview_revision: 0,
            preview_pending: false,
            preview_error: None,
            preview_worker_running: false,
            preview_task: None,
            layout_preview: None,
            layout_preview_revision: 0,
            requested_layout_preview_revision: 0,
            layout_preview_error: None,
            layout_preview_worker_running: false,
            layout_preview_task: None,
            layout_drag: None,
            pending_assets: PendingWatermarkAssets::new(&AppSettings::get(cx).watermark),
            preview_layout: Rc::new(Cell::new(None)),
            gesture: None,
            snap_guides: Vec::new(),
            hover_cursor: CursorStyle::Arrow,
            error: None,
            _subscriptions: subscriptions,
        };
        editor.sync_controls(window, cx);
        editor.refresh_preview(cx);
        editor.canvas_focus.focus(window, cx);
        editor
    }

    fn selected_index(&self) -> Option<usize> {
        let selected_id = self.selected_id.as_deref()?;
        self.preset
            .elements
            .iter()
            .position(|element| element.id == selected_id)
    }

    fn selected_element(&self) -> Option<&WatermarkElement> {
        let index = self.selected_index()?;
        self.preset.elements.get(index)
    }

    fn selected_element_mut(&mut self) -> Option<&mut WatermarkElement> {
        let index = self.selected_index()?;
        self.preset.elements.get_mut(index)
    }

    fn push_undo_snapshot(&mut self) {
        self.push_specific_undo(self.preset.clone());
    }

    fn push_specific_undo(&mut self, snapshot: WatermarkPreset) {
        if self.undo_stack.last() == Some(&snapshot) {
            return;
        }
        self.undo_stack.push(snapshot);
        if self.undo_stack.len() > MAX_UNDO_STEPS {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(previous) = self.undo_stack.pop() else {
            return;
        };
        self.redo_stack.push(self.preset.clone());
        self.preset = previous;
        self.slider_undo_snapshot = None;
        self.ensure_valid_selection();
        self.gesture = None;
        self.layout_drag = None;
        self.snap_guides.clear();
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
    }

    fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.redo_stack.pop() else {
            return;
        };
        self.undo_stack.push(self.preset.clone());
        self.preset = next;
        self.slider_undo_snapshot = None;
        self.ensure_valid_selection();
        self.gesture = None;
        self.layout_drag = None;
        self.snap_guides.clear();
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
    }

    fn ensure_valid_selection(&mut self) {
        if let Some(id) = &self.selected_id
            && self.preset.elements.iter().any(|el| &el.id == id)
        {
            return;
        }
        self.selected_id = self.preset.elements.first().map(|el| el.id.clone());
    }

    fn is_dirty(&self, cx: &App) -> bool {
        let watermark = &AppSettings::get(cx).watermark;
        match self.template_id.as_deref() {
            Some(id) => watermark.templates.get(id).is_none_or(|template| {
                self.preset != template.preset || self.fallback_name.trim() != template.name
            }),
            None => !self.preset.elements.is_empty() || !self.fallback_name.trim().is_empty(),
        }
    }

    fn apply_draft(
        &mut self,
        template_id: Option<String>,
        name: String,
        preset: WatermarkPreset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.template_id = template_id;
        self.fallback_name = name;
        self.preset = preset;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.slider_undo_snapshot = None;
        self.gesture = None;
        self.error = None;
        self.name_error = None;
        self.selected_id = self.preset.elements.first().map(|el| el.id.clone());
        self.ensure_valid_selection();
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
        if self.mode == EditorMode::Layout {
            self.refresh_layout_preview(cx);
        }
        cx.notify();
    }

    fn load_saved_template(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(template) = AppSettings::get(cx).watermark.templates.get(id).cloned() else {
            return;
        };
        self.apply_draft(
            Some(id.to_string()),
            template.name,
            template.preset,
            window,
            cx,
        );
    }

    fn apply_template_action(
        &mut self,
        action: TemplateAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            TemplateAction::Load(id) => self.load_saved_template(&id, window, cx),
            TemplateAction::New => {
                let name = AppSettings::get(cx)
                    .watermark
                    .unique_template_name("Watermark");
                self.apply_draft(None, name, WatermarkStarter::Blank.preset(), window, cx);
            }
            TemplateAction::Duplicate => {
                let preset = self.preset.clone();
                let name = AppSettings::get(cx)
                    .watermark
                    .next_duplicate_name(&self.fallback_name);
                self.apply_draft(None, name, preset, window, cx);
            }
            TemplateAction::Delete(id) => {
                AppSettings::update(cx, |config| {
                    config.watermark.delete_template(&id);
                });
                self.load_saved_template(WatermarkConfig::DEFAULT_TEMPLATE_ID, window, cx);
            }
        }
    }

    fn confirm_discard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: TemplateAction,
    ) {
        if !self.is_dirty(cx) {
            self.apply_template_action(action, window, cx);
            return;
        }
        // The select committed the new highlight before emitting Confirm. If the
        // user cancels, put it back on the template that is actually loaded.
        self.sync_controls(window, cx);
        let editor = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let editor = editor.clone();
            let action = action.clone();
            alert
                .title(t!("settings.watermark.editor.unsaved_title").to_string())
                .description(t!("settings.watermark.editor.unsaved_desc").to_string())
                .show_cancel(true)
                .ok_text(t!("settings.watermark.editor.discard").to_string())
                .on_ok(move |_, window, cx| {
                    editor.update(cx, |this, cx| {
                        this.apply_template_action(action.clone(), window, cx);
                    });
                    true
                })
        });
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.template_id.clone() else {
            return;
        };
        if id == WatermarkConfig::DEFAULT_TEMPLATE_ID {
            return;
        }
        let name = self.fallback_name.clone();
        let editor = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let editor = editor.clone();
            let id = id.clone();
            alert
                .title(
                    t!(
                        "settings.watermark.template.delete_title",
                        name = name.clone()
                    )
                    .to_string(),
                )
                .description(t!("settings.watermark.template.delete_desc").to_string())
                .confirm()
                .ok_text(t!("settings.watermark.template.delete").to_string())
                .on_ok(move |_, window, cx| {
                    editor.update(cx, |this, cx| {
                        this.apply_template_action(TemplateAction::Delete(id.clone()), window, cx);
                    });
                    true
                })
        });
    }

    fn sync_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.syncing_controls = true;
        if self.name_input.read(cx).value() != self.fallback_name {
            self.name_input.update(cx, |input, cx| {
                input.set_value(self.fallback_name.clone(), window, cx);
            });
        }
        let selected_value = self.template_id.clone().unwrap_or_default();
        if self.template_select.read(cx).selected_value() != Some(&selected_value) {
            self.template_select.update(cx, |state, cx| {
                state.set_selected_value(&selected_value, window, cx);
            });
        }
        if self.mode == EditorMode::Layout {
            let slider_values = [self.preset.rotation_degrees, self.preset.scale];
            for (index, value) in slider_values.into_iter().enumerate() {
                self.sliders[index].update(cx, |slider, cx| {
                    if (slider.value().end() - value).abs() > 0.0001 {
                        slider.set_value(value, window, cx);
                    }
                });
            }
        } else if let Some(element) = self.selected_element().cloned() {
            if self.text_input.read(cx).value() != element.text.content {
                self.text_input.update(cx, |input, cx| {
                    input.set_value(element.text.content.clone(), window, cx);
                });
            }
            let sync_picker = |picker: &Entity<ColorPickerState>,
                               rgb_u32: u32,
                               window: &mut Window,
                               cx: &mut Context<Self>| {
                let target_color = Hsla::from(rgb(rgb_u32));
                if picker.read(cx).value() != Some(target_color) {
                    picker.update(cx, |state, cx| state.set_value(target_color, window, cx));
                }
            };
            sync_picker(&self.color, element.text.color, window, cx);
            sync_picker(&self.stroke_color, element.text.stroke_color, window, cx);
            sync_picker(
                &self.background_color,
                element.text.background_color,
                window,
                cx,
            );

            let slider_values = [
                element.rotation_degrees,
                self.preset.scale,
                element.opacity,
                element.text.background_opacity,
                element.image.corner_radius,
            ];
            for (index, value) in slider_values.into_iter().enumerate() {
                self.sliders[index].update(cx, |slider, cx| {
                    if (slider.value().end() - value).abs() > 0.0001 {
                        slider.set_value(value, window, cx);
                    }
                });
            }

            let numbers = [
                element.text.font_size as f64,
                element.image.width as f64,
                element.x as f64,
                element.y as f64,
                element.text.stroke_width as f64,
            ];
            for (index, value) in numbers.into_iter().enumerate() {
                if (self.shown_numbers[index] - value).abs() > 0.001 {
                    self.shown_numbers[index] = value;
                    self.number_inputs[index].update(cx, |input, cx| {
                        input.set_value(format_number(value), window, cx);
                    });
                }
            }
        } else {
            let slider_values = [self.preset.rotation_degrees, self.preset.scale];
            for (index, value) in slider_values.into_iter().enumerate() {
                self.sliders[index].update(cx, |slider, cx| {
                    if (slider.value().end() - value).abs() > 0.0001 {
                        slider.set_value(value, window, cx);
                    }
                });
            }
        }
        let layout_numbers = [
            (5, self.preset.margin_x as f64),
            (6, self.preset.margin_y as f64),
            (7, self.preset.tile_spacing_x as f64),
            (8, self.preset.tile_spacing_y as f64),
        ];
        for (index, value) in layout_numbers {
            if (self.shown_numbers[index] - value).abs() > 0.001 {
                self.shown_numbers[index] = value;
                self.number_inputs[index].update(cx, |input, cx| {
                    input.set_value(format_number(value), window, cx);
                });
            }
        }
        self.syncing_controls = false;
    }

    fn set_mode(&mut self, mode: EditorMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.gesture = None;
        self.layout_drag = None;
        self.slider_undo_snapshot = None;
        self.snap_guides.clear();
        self.sync_controls(window, cx);
        if mode == EditorMode::Layout {
            self.refresh_layout_preview(cx);
        }
        self.canvas_focus.focus(window, cx);
        cx.notify();
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        self.requested_preview_revision = self.requested_preview_revision.wrapping_add(1);
        self.preview_pending = true;
        self.preview_error = None;
        if !self.preview_worker_running {
            self.preview_worker_running = true;
            self.preview_task = Some(cx.spawn(
                async move |this: WeakEntity<Self>, cx: &mut AsyncApp| loop {
                    let next = this.update(cx, |editor, cx| {
                        let revision = editor.requested_preview_revision;
                        let preset = editor.preset.clone();
                        let image_dir = AppSettings::watermark_dir(cx).to_path_buf();
                        (revision, preset, image_dir)
                    });
                    let Ok((revision, preset, image_dir)) = next else {
                        break;
                    };
                    let result = cx
                        .background_spawn(async move { build_stamp_preview(&preset, &image_dir) })
                        .await;
                    let keep_going = this.update(cx, |editor, cx| {
                        let up_to_date = editor.requested_preview_revision == revision;
                        if up_to_date {
                            editor.preview_pending = false;
                            editor.preview_worker_running = false;
                        }
                        match result {
                            Ok(preview) => {
                                editor.preview_revision = revision;
                                editor.preview = Some(preview);
                                editor.preview_error = None;
                            }
                            Err(error) if up_to_date => {
                                log::warn!("watermark stamp preview failed: {error}");
                                editor.preview_error = Some(error);
                            }
                            Err(_) => {}
                        }
                        cx.notify();
                        !up_to_date
                    });
                    if !matches!(keep_going, Ok(true)) {
                        break;
                    }
                },
            ));
        }
        if self.mode == EditorMode::Layout {
            self.refresh_layout_preview(cx);
        } else {
            cx.notify();
        }
    }

    fn refresh_layout_preview(&mut self, cx: &mut Context<Self>) {
        self.requested_layout_preview_revision =
            self.requested_layout_preview_revision.wrapping_add(1);
        self.layout_preview_error = None;
        if !self.layout_preview_worker_running {
            self.layout_preview_worker_running = true;
            self.layout_preview_task = Some(cx.spawn(
                async move |this: WeakEntity<Self>, cx: &mut AsyncApp| loop {
                    let next = this.update(cx, |editor, cx| {
                        let revision = editor.requested_layout_preview_revision;
                        let target = editor.target;
                        let mut preset = editor.preset.clone();
                        preset.normalize_element_origins();
                        preset.sanitize();
                        let image_dir = AppSettings::watermark_dir(cx).to_path_buf();
                        (revision, target, preset, image_dir)
                    });
                    let Ok((revision, target, preset, image_dir)) = next else {
                        break;
                    };
                    let result = cx
                        .background_spawn(async move {
                            crate::settings_view::build_watermark_preview_scaled(
                                target,
                                &preset,
                                &image_dir,
                                PREVIEW_SCALE,
                            )
                        })
                        .await;
                    let keep_going = this.update(cx, |editor, cx| {
                        let up_to_date = editor.requested_layout_preview_revision == revision;
                        if up_to_date {
                            editor.layout_preview_worker_running = false;
                        }
                        match result {
                            Ok(image) => {
                                editor.layout_preview_revision = revision;
                                editor.layout_preview = Some(image);
                                editor.layout_preview_error = None;
                            }
                            Err(error) if up_to_date => {
                                log::warn!("watermark layout preview failed: {error}");
                                editor.layout_preview_error = Some(error);
                            }
                            Err(_) => {}
                        }
                        cx.notify();
                        !up_to_date
                    });
                    if !matches!(keep_going, Ok(true)) {
                        break;
                    }
                },
            ));
        }
        cx.notify();
    }

    fn set_number(&mut self, field: NumberField, value: f64) {
        let value = value as f32;
        match field {
            NumberField::MarginX => {
                self.preset.margin_x = value.clamp(0.0, 2048.0);
                return;
            }
            NumberField::MarginY => {
                self.preset.margin_y = value.clamp(0.0, 2048.0);
                return;
            }
            NumberField::TileSpacingX => {
                self.preset.tile_spacing_x = value.clamp(8.0, 4096.0);
                return;
            }
            NumberField::TileSpacingY => {
                self.preset.tile_spacing_y = value.clamp(8.0, 4096.0);
                return;
            }
            _ => {}
        }
        let Some(element) = self.selected_element_mut() else {
            return;
        };
        match field {
            NumberField::FontSize => {
                element.text.font_size = value.clamp(8.0, 256.0);
                element.scale = 1.0;
            }
            NumberField::ImageWidth => {
                element.image.width = value.clamp(8.0, 2048.0);
                element.scale = 1.0;
            }
            NumberField::OffsetX => element.x = value.clamp(-4096.0, 4096.0),
            NumberField::OffsetY => element.y = value.clamp(-4096.0, 4096.0),
            NumberField::StrokeWidth => element.text.stroke_width = value.clamp(0.0, 12.0),
            NumberField::MarginX
            | NumberField::MarginY
            | NumberField::TileSpacingX
            | NumberField::TileSpacingY => {}
        }
    }

    fn set_slider(&mut self, field: SliderField, value: f32) {
        if self.mode == EditorMode::Layout {
            match field {
                SliderField::Rotation => self.preset.rotation_degrees = value.clamp(-180.0, 180.0),
                SliderField::Scale => self.preset.scale = value.clamp(0.1, 8.0),
                _ => {}
            }
            return;
        }
        if let Some(element) = self.selected_element_mut() {
            match field {
                SliderField::Rotation => element.rotation_degrees = value.clamp(-180.0, 180.0),
                SliderField::Scale => self.preset.scale = value.clamp(0.1, 8.0),
                SliderField::Opacity => element.opacity = value.clamp(0.0, 1.0),
                SliderField::BackgroundOpacity => {
                    element.text.background_opacity = value.clamp(0.0, 1.0)
                }
                SliderField::CornerRadius => element.image.corner_radius = value.clamp(0.0, 50.0),
            }
        } else {
            match field {
                SliderField::Rotation => self.preset.rotation_degrees = value.clamp(-180.0, 180.0),
                SliderField::Scale => self.preset.scale = value.clamp(0.1, 8.0),
                _ => {}
            }
        }
    }

    fn select_element(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_id == id {
            return;
        }
        self.selected_id = id;
        self.slider_undo_snapshot = None;
        self.gesture = None;
        self.snap_guides.clear();
        self.sync_controls(window, cx);
        cx.notify();
    }

    fn next_element_position(&self) -> (f32, f32) {
        if self.preset.elements.is_empty() {
            return (0.0, 0.0);
        }
        if let Some(preview) = &self.preview {
            let (ox, oy) = preview.origin;
            let (_, h) = preview.logical_size;
            return (ox.round(), (oy + h + 12.0).round());
        }
        let max_y = self
            .preset
            .elements
            .iter()
            .map(|el| el.y)
            .fold(f32::NEG_INFINITY, f32::max);
        (0.0, max_y + 36.0)
    }

    fn add_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preset.elements.len() >= WatermarkPreset::MAX_ELEMENTS {
            return;
        }
        self.push_undo_snapshot();
        let id = self.preset.next_element_id();
        let (x, y) = self.next_element_position();
        let mut element = WatermarkElement::new_text(id.clone(), "SnapIt");
        element.x = x;
        element.y = y;
        self.preset.elements.push(element);
        self.selected_id = Some(id);
        self.error = None;
        self.sync_controls(window, cx);
        self.text_focus.focus(window, cx);
        self.refresh_preview(cx);
    }

    fn apply_starter(
        &mut self,
        starter: WatermarkStarter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.push_undo_snapshot();
        let next = starter.preset();
        self.preset.elements = next.elements;
        self.preset.rotation_degrees = next.rotation_degrees;
        self.preset.scale = next.scale;
        self.selected_id = self.preset.elements.first().map(|el| el.id.clone());
        self.error = None;
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
    }

    fn insert_text_variable(&mut self, token: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(element) = self.selected_element() else {
            return;
        };
        if element.kind != WatermarkKind::Text {
            return;
        }
        self.push_undo_snapshot();
        self.syncing_controls = true;
        self.text_input.update(cx, |input, cx| {
            input.insert(token.to_string(), window, cx);
        });
        let updated = self.text_input.read(cx).value().to_string();
        self.syncing_controls = false;
        if let Some(element) = self.selected_element_mut() {
            element.text.content = updated;
        }
        self.error = None;
        self.refresh_preview(cx);
    }

    fn duplicate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preset.elements.len() >= WatermarkPreset::MAX_ELEMENTS {
            return;
        }
        let Some(index) = self.selected_index() else {
            return;
        };
        self.push_undo_snapshot();
        let mut cloned = self.preset.elements[index].clone();
        cloned.id = self.preset.next_element_id();
        cloned.x = (cloned.x + 16.0).clamp(-4096.0, 4096.0);
        cloned.y = (cloned.y + 16.0).clamp(-4096.0, 4096.0);
        let new_id = cloned.id.clone();
        self.preset.elements.insert(index + 1, cloned);
        self.selected_id = Some(new_id);
        self.error = None;
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
    }

    fn move_selected_layer(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let target = index as isize + delta;
        if target < 0 || target >= self.preset.elements.len() as isize {
            return;
        }
        self.push_undo_snapshot();
        self.preset.elements.swap(index, target as usize);
        self.refresh_preview(cx);
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.selected_index() else {
            return;
        };
        self.push_undo_snapshot();
        self.preset.elements.remove(index);
        self.selected_id = if self.preset.elements.is_empty() {
            None
        } else {
            let next_index = index.min(self.preset.elements.len() - 1);
            Some(self.preset.elements[next_index].id.clone())
        };
        self.gesture = None;
        self.snap_guides.clear();
        self.error = None;
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
    }

    fn nudge_selected(&mut self, dx: f32, dy: f32, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_element().is_none() {
            return;
        }
        self.push_undo_snapshot();
        if let Some(element) = self.selected_element_mut() {
            element.x = (element.x + dx).clamp(-4096.0, 4096.0);
            element.y = (element.y + dy).clamp(-4096.0, 4096.0);
        }
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
    }

    fn align_all(&mut self, op: AlignOp, window: &mut Window, cx: &mut Context<Self>) {
        if self.preset.elements.len() < 2 {
            return;
        }
        let Some(preview) = &self.preview else {
            return;
        };
        let scale = self.preset.scale.max(0.1);
        let sizes: Vec<(f32, f32)> = self
            .preset
            .elements
            .iter()
            .map(|el| {
                preview
                    .element_bounds
                    .iter()
                    .find(|(id, _)| id == &el.id)
                    .map_or((48.0, 24.0), |(_, b)| (b.width / scale, b.height / scale))
            })
            .collect();
        self.push_undo_snapshot();
        apply_align_op(&mut self.preset.elements, &sizes, op);
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
    }

    fn import_image_paths(&mut self, paths: &[std::path::PathBuf], dir: &Path) {
        let mut last_id = None;
        for path in paths {
            if self.preset.elements.len() >= WatermarkPreset::MAX_ELEMENTS {
                break;
            }
            match crate::settings_view::import_watermark_image(path, dir) {
                Ok((source, natural_w, _)) => {
                    self.pending_assets.track(dir, source.clone());
                    if last_id.is_none() {
                        self.push_undo_snapshot();
                    }
                    let id = self.preset.next_element_id();
                    let (x, y) = next_staggered_offset(self);
                    let width = initial_image_width(natural_w);
                    let mut element = WatermarkElement::new_image(id.clone(), source, width);
                    element.x = x;
                    element.y = y;
                    self.preset.elements.push(element);
                    last_id = Some(id);
                    self.error = None;
                }
                Err(err) => {
                    let message = format!("{err:#}");
                    log::warn!("watermark image import failed: {message}");
                    self.error = Some(message);
                    break;
                }
            }
        }
        if let Some(id) = last_id {
            self.selected_id = Some(id);
        }
    }

    fn on_drop_files(
        &mut self,
        dropped: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if dropped.paths().is_empty() {
            return;
        }
        let dir = AppSettings::watermark_dir(cx).to_path_buf();
        self.import_image_paths(dropped.paths(), &dir);
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
    }

    fn paste_from_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let dir = AppSettings::watermark_dir(cx).to_path_buf();
        let mut changed = false;
        for entry in item.entries() {
            if self.preset.elements.len() >= WatermarkPreset::MAX_ELEMENTS {
                break;
            }
            match entry {
                ClipboardEntry::Image(image) => {
                    match crate::settings_view::import_watermark_image_bytes(&image.bytes, &dir) {
                        Ok((source, natural_w, _)) => {
                            self.pending_assets.track(&dir, source.clone());
                            if !changed {
                                self.push_undo_snapshot();
                            }
                            let id = self.preset.next_element_id();
                            let (x, y) = next_staggered_offset(self);
                            let mut element = WatermarkElement::new_image(
                                id.clone(),
                                source,
                                initial_image_width(natural_w),
                            );
                            element.x = x;
                            element.y = y;
                            self.preset.elements.push(element);
                            self.selected_id = Some(id);
                            self.error = None;
                            changed = true;
                        }
                        Err(err) => {
                            self.error = Some(format!("{err:#}"));
                        }
                    }
                }
                ClipboardEntry::ExternalPaths(ExternalPaths(paths)) => {
                    let before = self.preset.elements.len();
                    self.import_image_paths(paths, &dir);
                    if self.preset.elements.len() != before {
                        changed = true;
                    }
                }
                ClipboardEntry::String(_) => {}
            }
        }
        if changed || self.error.is_some() {
            self.sync_controls(window, cx);
            self.refresh_preview(cx);
        }
    }

    fn preview_is_fresh(&self) -> bool {
        self.preview.is_some()
            && !self.preview_pending
            && self.preview_revision == self.requested_preview_revision
            && self.preview_error.is_none()
    }

    fn can_save(&self) -> bool {
        preset_is_valid(&self.preset) && self.preview_is_fresh()
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.preview_is_fresh() {
            cx.notify();
            return false;
        }
        if self.preset.elements.is_empty() {
            self.error = Some(t!("settings.watermark.elements.required").to_string());
            cx.notify();
            return false;
        }
        for element in &self.preset.elements {
            if element.kind == WatermarkKind::Text && element.text.content.trim().is_empty() {
                self.error = Some(t!("settings.watermark.text.required").to_string());
                cx.notify();
                return false;
            }
            if element.kind == WatermarkKind::Image && element.image.source.trim().is_empty() {
                self.error = Some(t!("settings.watermark.image.required").to_string());
                cx.notify();
                return false;
            }
        }
        let name = self.fallback_name.trim().to_string();
        if name.is_empty() {
            self.name_error = Some(t!("settings.watermark.template.name_required").to_string());
            cx.notify();
            return false;
        }
        if AppSettings::get(cx)
            .watermark
            .template_name_exists(&name, self.template_id.as_deref())
        {
            self.name_error =
                Some(t!("settings.watermark.template.name_exists", name = name).to_string());
            cx.notify();
            return false;
        }
        self.name_error = None;

        let design = self.preset.clone();
        let template_id = self.template_id.clone();
        let mut saved_id = template_id.clone();
        AppSettings::update(cx, |config| {
            if let Some(id) = template_id.as_deref() {
                config.watermark.rename_template(id, name.clone());
            }
            saved_id = Some(
                config
                    .watermark
                    .save_template_design(template_id, name, design),
            );
        });
        self.template_id = saved_id.clone();
        if let Some(id) = saved_id.as_deref()
            && let Some(saved) = AppSettings::get(cx).watermark.templates.get(id).cloned()
        {
            self.fallback_name = saved.name;
            self.preset = saved.preset;
            self.sync_controls(window, cx);
        }
        let retained = AppSettings::get(cx)
            .watermark
            .templates
            .values()
            .flat_map(|template| watermark_image_references(&template.preset))
            .collect();
        self.pending_assets.cleanup(&retained);
        log::info!(
            "watermark stamp design saved: target={} template={:?} elements={}",
            self.target.as_str(),
            self.template_id,
            self.preset.elements.len()
        );
        true
    }

    fn selected_screen_rect(
        &self,
        layout: &PreviewLayout,
        preview: &StampPreview,
    ) -> Option<Bounds<Pixels>> {
        let selected_id = self.selected_id.as_deref()?;
        let (_, b) = preview
            .element_bounds
            .iter()
            .find(|(id, _)| id == selected_id)?;
        Some(Bounds::new(
            point(
                layout.stamp_origin.x + px(b.x * layout.zoom),
                layout.stamp_origin.y + px(b.y * layout.zoom),
            ),
            size(px(b.width * layout.zoom), px(b.height * layout.zoom)),
        ))
    }

    fn hit_test_handles(&self, pos: Point<Pixels>, rect: Bounds<Pixels>) -> HandleHit {
        let rot_pt = point(
            rect.origin.x + rect.size.width * 0.5,
            rect.origin.y - px(ROTATE_STEM_PX),
        );
        let r_dx = (pos.x - rot_pt.x).as_f32();
        let r_dy = (pos.y - rot_pt.y).as_f32();
        if (r_dx * r_dx + r_dy * r_dy).sqrt() <= HANDLE_HIT_RADIUS_PX {
            return HandleHit::Rotate;
        }
        let corners = [
            (rect.origin.x, rect.origin.y, true),
            (rect.right(), rect.origin.y, false),
            (rect.origin.x, rect.bottom(), false),
            (rect.right(), rect.bottom(), true),
        ];
        for (cx, cy, nwse) in corners {
            if (pos.x - cx).as_f32().abs() <= HANDLE_HIT_RADIUS_PX
                && (pos.y - cy).as_f32().abs() <= HANDLE_HIT_RADIUS_PX
            {
                return HandleHit::Corner(nwse);
            }
        }
        HandleHit::None
    }

    fn begin_canvas_gesture(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.canvas_focus.focus(window, cx);
        let Some(layout) = self.preview_layout.get() else {
            return;
        };
        let Some(preview) = self.preview.clone() else {
            return;
        };

        if let Some(rect) = self.selected_screen_rect(&layout, &preview)
            && let Some(element) = self.selected_element().cloned()
        {
            let center = point(
                rect.origin.x + rect.size.width * 0.5,
                rect.origin.y + rect.size.height * 0.5,
            );
            match self.hit_test_handles(event.position, rect) {
                HandleHit::Rotate => {
                    let start_angle = (event.position.y - center.y)
                        .as_f32()
                        .atan2((event.position.x - center.x).as_f32())
                        .to_degrees();
                    self.gesture = Some(CanvasGesture::Rotate {
                        element_id: element.id,
                        center,
                        start_angle,
                        start_rotation: element.rotation_degrees,
                        zoom: layout.zoom,
                        camera_center: layout.camera_center,
                        start_preset: self.preset.clone(),
                    });
                    cx.stop_propagation();
                    return;
                }
                HandleHit::Corner(_) => {
                    let start_dist = (event.position.x - center.x)
                        .as_f32()
                        .hypot((event.position.y - center.y).as_f32())
                        .max(8.0);
                    let start_size = match element.kind {
                        WatermarkKind::Text => element.text.font_size * element.scale,
                        WatermarkKind::Image => element.image.width * element.scale,
                    };
                    self.gesture = Some(CanvasGesture::Resize {
                        element_id: element.id,
                        center,
                        start_dist,
                        start_size,
                        zoom: layout.zoom,
                        camera_center: layout.camera_center,
                        start_preset: self.preset.clone(),
                    });
                    cx.stop_propagation();
                    return;
                }
                HandleHit::None => {}
            }
        }

        let local_x = (event.position.x - layout.stamp_origin.x).as_f32() / layout.zoom;
        let local_y = (event.position.y - layout.stamp_origin.y).as_f32() / layout.zoom;
        let slop = HIT_SLOP_PX / layout.zoom;
        let hit_id = preview
            .element_bounds
            .iter()
            .rev()
            .find(|(_, bounds)| {
                local_x >= bounds.x - slop
                    && local_x <= bounds.x + bounds.width + slop
                    && local_y >= bounds.y - slop
                    && local_y <= bounds.y + bounds.height + slop
            })
            .map(|(id, _)| id.clone());

        let Some(hit_id) = hit_id else {
            self.select_element(None, window, cx);
            cx.stop_propagation();
            return;
        };

        self.select_element(Some(hit_id.clone()), window, cx);
        if event.click_count >= 2
            && self
                .selected_element()
                .is_some_and(|el| el.kind == WatermarkKind::Text)
        {
            self.text_focus.focus(window, cx);
            cx.stop_propagation();
            return;
        }
        if let Some(element) = self.selected_element().cloned() {
            let mut start_world_box = None;
            let mut siblings = Vec::new();
            for (id, b) in &preview.element_bounds {
                let wb = WorldBox {
                    x: layout.stamp_world_origin.0 + b.x,
                    y: layout.stamp_world_origin.1 + b.y,
                    width: b.width,
                    height: b.height,
                };
                if id == &hit_id {
                    start_world_box = Some(wb);
                } else {
                    siblings.push(wb);
                }
            }
            if let Some(start_world_box) = start_world_box {
                self.gesture = Some(CanvasGesture::Move {
                    element_id: hit_id,
                    press: event.position,
                    start_x: element.x,
                    start_y: element.y,
                    start_world_box,
                    siblings,
                    zoom: layout.zoom,
                    camera_center: layout.camera_center,
                    start_preset: self.preset.clone(),
                });
            }
        }
        cx.stop_propagation();
    }

    fn move_canvas_gesture(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(gesture) = self.gesture.clone() else {
            self.update_hover_cursor(event.position, cx);
            return;
        };
        self.snap_guides.clear();
        match gesture {
            CanvasGesture::Move {
                element_id,
                press,
                start_x,
                start_y,
                start_world_box,
                siblings,
                zoom,
                ..
            } => {
                let screen_dx = (event.position.x - press.x).as_f32();
                let screen_dy = (event.position.y - press.y).as_f32();
                let (mut dx, mut dy) = inverse_stamp_delta(
                    screen_dx,
                    screen_dy,
                    zoom,
                    self.preset.scale,
                    self.preset.rotation_degrees,
                );
                if self.preset.rotation_degrees.abs() <= 0.01 && !siblings.is_empty() {
                    let scale = self.preset.scale.max(0.1);
                    let threshold = SNAP_THRESHOLD_PX / zoom.max(0.1);
                    let cand_x = start_world_box.x + dx * scale;
                    let cand_y = start_world_box.y + dy * scale;
                    let (snap_dx, x_guide) =
                        snap_axis(cand_x, start_world_box.width, &siblings, true, threshold);
                    let (snap_dy, y_guide) =
                        snap_axis(cand_y, start_world_box.height, &siblings, false, threshold);
                    dx += snap_dx / scale;
                    dy += snap_dy / scale;
                    if let Some(pos) = x_guide {
                        self.snap_guides.push(SnapGuide {
                            vertical: true,
                            world_pos: pos,
                        });
                    }
                    if let Some(pos) = y_guide {
                        self.snap_guides.push(SnapGuide {
                            vertical: false,
                            world_pos: pos,
                        });
                    }
                }
                if let Some(element) = self.preset.elements.iter_mut().find(|e| e.id == element_id)
                {
                    element.x = (start_x + dx).round().clamp(-4096.0, 4096.0);
                    element.y = (start_y + dy).round().clamp(-4096.0, 4096.0);
                }
                self.hover_cursor = CursorStyle::ClosedHand;
            }
            CanvasGesture::Resize {
                element_id,
                center,
                start_dist,
                start_size,
                ..
            } => {
                let dist = (event.position.x - center.x)
                    .as_f32()
                    .hypot((event.position.y - center.y).as_f32())
                    .max(4.0);
                let ratio = (dist / start_dist).clamp(0.1, 10.0);
                let next = (start_size * ratio).round();
                if let Some(element) = self.preset.elements.iter_mut().find(|e| e.id == element_id)
                {
                    match element.kind {
                        WatermarkKind::Text => {
                            element.text.font_size = next.clamp(8.0, 256.0);
                        }
                        WatermarkKind::Image => {
                            element.image.width = next.clamp(8.0, 2048.0);
                        }
                    }
                    element.scale = 1.0;
                }
            }
            CanvasGesture::Rotate {
                element_id,
                center,
                start_angle,
                start_rotation,
                ..
            } => {
                let current_angle = (event.position.y - center.y)
                    .as_f32()
                    .atan2((event.position.x - center.x).as_f32())
                    .to_degrees();
                let mut next = normalize_angle(start_rotation + (current_angle - start_angle));
                if event.modifiers.shift {
                    next = (next / 15.0).round() * 15.0;
                }
                if let Some(element) = self.preset.elements.iter_mut().find(|e| e.id == element_id)
                {
                    element.rotation_degrees = next.round().clamp(-180.0, 180.0);
                }
                self.hover_cursor = CursorStyle::ClosedHand;
            }
        }
        self.sync_controls(window, cx);
        self.refresh_preview(cx);
        cx.stop_propagation();
    }

    fn end_canvas_gesture(
        &mut self,
        _event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(gesture) = self.gesture.take() {
            let start = gesture.start_preset().clone();
            if self.preset != start {
                self.push_specific_undo(start);
            }
            self.snap_guides.clear();
            self.hover_cursor = CursorStyle::OpenHand;
            self.sync_controls(window, cx);
            cx.notify();
            cx.stop_propagation();
        }
    }

    fn update_hover_cursor(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let mut next = CursorStyle::Arrow;
        if let (Some(layout), Some(preview)) = (self.preview_layout.get(), &self.preview) {
            if let Some(rect) = self.selected_screen_rect(&layout, preview) {
                match self.hit_test_handles(pos, rect) {
                    HandleHit::Rotate => next = CursorStyle::OpenHand,
                    HandleHit::Corner(true) => next = CursorStyle::ResizeUpLeftDownRight,
                    HandleHit::Corner(false) => next = CursorStyle::ResizeUpRightDownLeft,
                    HandleHit::None => {}
                }
            }
            if next == CursorStyle::Arrow {
                let local_x = (pos.x - layout.stamp_origin.x).as_f32() / layout.zoom;
                let local_y = (pos.y - layout.stamp_origin.y).as_f32() / layout.zoom;
                let slop = HIT_SLOP_PX / layout.zoom;
                if preview.element_bounds.iter().any(|(_, b)| {
                    local_x >= b.x - slop
                        && local_x <= b.x + b.width + slop
                        && local_y >= b.y - slop
                        && local_y <= b.y + b.height + slop
                }) {
                    next = CursorStyle::OpenHand;
                }
            }
        }
        if next != self.hover_cursor {
            self.hover_cursor = next;
            cx.notify();
        }
    }

    fn on_canvas_scroll(
        &mut self,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(window.line_height()).y.as_f32();
        if delta.abs() < 0.01 {
            return;
        }
        let base = self
            .zoom_override
            .unwrap_or_else(|| self.preview_layout.get().map_or(1.0, |layout| layout.zoom));
        let next = (base * (delta * 0.004).exp()).clamp(0.25, 4.0);
        self.zoom_override = Some(next);
        cx.notify();
        cx.stop_propagation();
    }

    /// Trackpad pinch (macOS `magnifyWithEvent:`, Windows DirectManipulation).
    /// `delta` is a relative step: 0.1 means 10 % larger.
    fn on_canvas_pinch(&mut self, event: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        // The camera is locked while an element is being dragged.
        if self.gesture.is_some() || !event.delta.is_finite() || event.delta.abs() < 0.0001 {
            return;
        }
        let base = self
            .zoom_override
            .unwrap_or_else(|| self.preview_layout.get().map_or(1.0, |layout| layout.zoom));
        let next = pinch_zoom(base, event.delta);
        if (next - base).abs() > f32::EPSILON {
            self.zoom_override = Some(next);
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn adjust_zoom(&mut self, factor: Option<f32>, cx: &mut Context<Self>) {
        match factor {
            None => self.zoom_override = None,
            Some(mult) => {
                let base = self
                    .zoom_override
                    .unwrap_or_else(|| self.preview_layout.get().map_or(1.0, |layout| layout.zoom));
                self.zoom_override = Some((base * mult).clamp(0.25, 4.0));
            }
        }
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Toolbar {
        let editor = cx.entity();
        let count = self.preset.elements.len();
        let at_limit = count >= WatermarkPreset::MAX_ELEMENTS;
        let has_selection = self.selected_index().is_some();
        let can_align = count >= 2 && self.preview.is_some();

        let align_button = |id: &'static str, icon: LucideIcon, key: &'static str, op: AlignOp| {
            let editor = editor.clone();
            Button::new(id)
                .ghost()
                .icon(icon)
                .tooltip(t!(key).to_string())
                .accessibility_label(t!(key).to_string())
                .disabled(!can_align)
                .on_click(move |_, window, cx| {
                    editor.update(cx, |this, cx| this.align_all(op, window, cx));
                })
        };

        Toolbar::new("watermark-design-tools")
            .child(
                Button::new("watermark-add-text")
                    .icon(LucideIcon::Type)
                    .label(t!("settings.watermark.editor.add_text").to_string())
                    .disabled(at_limit)
                    .on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            editor.update(cx, |this, cx| this.add_text(window, cx));
                        }
                    }),
            )
            .child(
                Button::new("watermark-add-image")
                    .icon(LucideIcon::ImagePlus)
                    .label(t!("settings.watermark.editor.add_image").to_string())
                    .disabled(at_limit)
                    .on_click({
                        let editor = editor.clone();
                        move |_, _, cx| add_images(editor.clone(), cx)
                    }),
            )
            .content(div().w(px(1.)).h(px(20.)).bg(cx.theme().border).mx_1())
            .child(
                Button::new("watermark-undo")
                    .ghost()
                    .icon(LucideIcon::Undo2)
                    .tooltip(t!("settings.watermark.editor.undo").to_string())
                    .accessibility_label(t!("settings.watermark.editor.undo").to_string())
                    .disabled(self.undo_stack.is_empty())
                    .on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            editor.update(cx, |this, cx| this.undo(window, cx));
                        }
                    }),
            )
            .child(
                Button::new("watermark-redo")
                    .ghost()
                    .icon(LucideIcon::Redo2)
                    .tooltip(t!("settings.watermark.editor.redo").to_string())
                    .accessibility_label(t!("settings.watermark.editor.redo").to_string())
                    .disabled(self.redo_stack.is_empty())
                    .on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            editor.update(cx, |this, cx| this.redo(window, cx));
                        }
                    }),
            )
            .content(div().w(px(1.)).h(px(20.)).bg(cx.theme().border).mx_1())
            .child(
                Button::new("watermark-duplicate-element")
                    .ghost()
                    .icon(IconName::Copy)
                    .tooltip(t!("settings.watermark.editor.duplicate").to_string())
                    .accessibility_label(t!("settings.watermark.editor.duplicate").to_string())
                    .disabled(!has_selection || at_limit)
                    .on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            editor.update(cx, |this, cx| this.duplicate_selected(window, cx));
                        }
                    }),
            )
            .child(
                Button::new("watermark-delete-element")
                    .ghost()
                    .icon(LucideIcon::Trash)
                    .tooltip(t!("settings.watermark.editor.delete").to_string())
                    .accessibility_label(t!("settings.watermark.editor.delete").to_string())
                    .disabled(!has_selection)
                    .on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            editor.update(cx, |this, cx| this.delete_selected(window, cx));
                        }
                    }),
            )
            .content(div().w(px(1.)).h(px(20.)).bg(cx.theme().border).mx_1())
            .child(align_button(
                "watermark-align-left",
                LucideIcon::AlignStartVertical,
                "settings.watermark.editor.align_left",
                AlignOp::Left,
            ))
            .child(align_button(
                "watermark-align-center-h",
                LucideIcon::AlignCenterVertical,
                "settings.watermark.editor.align_center_h",
                AlignOp::CenterH,
            ))
            .child(align_button(
                "watermark-align-center-v",
                LucideIcon::AlignCenterHorizontal,
                "settings.watermark.editor.align_center_v",
                AlignOp::CenterV,
            ))
            .child(align_button(
                "watermark-arrange-row",
                LucideIcon::AlignHorizontalSpaceBetween,
                "settings.watermark.editor.arrange_row",
                AlignOp::Row,
            ))
            .child(align_button(
                "watermark-arrange-column",
                LucideIcon::AlignVerticalSpaceBetween,
                "settings.watermark.editor.arrange_column",
                AlignOp::Column,
            ))
    }

    fn render_layout_toolbar(&self, cx: &mut Context<Self>) -> Toolbar {
        let editor = cx.entity();
        Toolbar::new("watermark-layout-tools")
            .child(
                Button::new("watermark-layout-undo")
                    .ghost()
                    .icon(LucideIcon::Undo2)
                    .tooltip(t!("settings.watermark.editor.undo").to_string())
                    .accessibility_label(t!("settings.watermark.editor.undo").to_string())
                    .disabled(self.undo_stack.is_empty())
                    .on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            editor.update(cx, |this, cx| this.undo(window, cx));
                        }
                    }),
            )
            .child(
                Button::new("watermark-layout-redo")
                    .ghost()
                    .icon(LucideIcon::Redo2)
                    .tooltip(t!("settings.watermark.editor.redo").to_string())
                    .accessibility_label(t!("settings.watermark.editor.redo").to_string())
                    .disabled(self.redo_stack.is_empty())
                    .on_click(move |_, window, cx| {
                        editor.update(cx, |this, cx| this.redo(window, cx));
                    }),
            )
    }

    fn render_layout_preview_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.target;
        let image = self.layout_preview.clone();
        let error = self.layout_preview_error.clone();
        let drag_enabled =
            !self.preset.tiled && self.preset.placement != WatermarkPlacement::Center;
        let start_margin_x = self.preset.margin_x;
        let start_margin_y = self.preset.margin_y;
        let placement = self.preset.placement;
        let editor = cx.entity();
        let theme = cx.theme();

        v_flex()
            .id("watermark-layout-preview-pane")
            .key_context(CONTEXT)
            .track_focus(&self.canvas_focus)
            .flex_1()
            .min_w_0()
            .min_h_0()
            .p_3()
            .gap_2()
            .on_action(cx.listener(|this, _: &UndoEdit, window, cx| {
                this.undo(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RedoEdit, window, cx| {
                this.redo(window, cx);
            }))
            .on_action(cx.listener(|_, _: &CloseEditor, window, _| {
                window.remove_window();
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                let Some(drag) = this.layout_drag.as_ref() else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left) {
                    let start = drag.start_preset.clone();
                    this.layout_drag = None;
                    if this.preset != start {
                        this.push_specific_undo(start);
                    }
                    cx.notify();
                    return;
                }
                let (x_dir, y_dir) = crate::settings_view::preview_margin_axes(drag.placement);
                let dx = f32::from(event.position.x - drag.press.x) * drag.ratio;
                let dy = f32::from(event.position.y - drag.press.y) * drag.ratio;
                let mut changed = false;
                if x_dir != 0.0 {
                    let next = (drag.start_margin_x + dx * x_dir)
                        .round()
                        .clamp(0.0, 2048.0);
                    if (this.preset.margin_x - next).abs() > 0.1 {
                        this.preset.margin_x = next;
                        changed = true;
                    }
                }
                if y_dir != 0.0 {
                    let next = (drag.start_margin_y + dy * y_dir)
                        .round()
                        .clamp(0.0, 2048.0);
                    if (this.preset.margin_y - next).abs() > 0.1 {
                        this.preset.margin_y = next;
                        changed = true;
                    }
                }
                if changed {
                    this.sync_controls(window, cx);
                    this.refresh_layout_preview(cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(drag) = this.layout_drag.take() {
                        if this.preset != drag.start_preset {
                            this.push_specific_undo(drag.start_preset);
                        }
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if let Some(drag) = this.layout_drag.take() {
                        if this.preset != drag.start_preset {
                            this.push_specific_undo(drag.start_preset);
                        }
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .min_h(px(240.))
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.muted)
                    .overflow_hidden()
                    .p_4()
                    .child(container_query(move |avail, _, cx| {
                        let (width, height) = crate::settings_view::watermark_preview_size(
                            target,
                            f32::from(avail.width).max(1.0),
                            f32::from(avail.height).max(1.0),
                        );
                        let logical_width = match target {
                            WatermarkTarget::Animation => 240.0,
                            WatermarkTarget::Screenshot | WatermarkTarget::Video => 480.0,
                        };
                        let ratio = if width > 0.0 {
                            logical_width / width
                        } else {
                            1.0
                        };
                        let down_editor = editor.clone();

                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .children(image.clone().map(|image| {
                                div()
                                    .id("watermark-layout-preview-canvas")
                                    .w(px(width))
                                    .h(px(height))
                                    .flex_none()
                                    .rounded(cx.theme().radius)
                                    .overflow_hidden()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .when(drag_enabled, |this| this.cursor(CursorStyle::OpenHand))
                                    .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                                        if !drag_enabled {
                                            return;
                                        }
                                        down_editor.update(cx, |this, cx| {
                                            this.layout_drag = Some(LayoutPreviewDrag {
                                                press: event.position,
                                                start_margin_x,
                                                start_margin_y,
                                                ratio,
                                                placement,
                                                start_preset: this.preset.clone(),
                                            });
                                            cx.notify();
                                        });
                                    })
                                    .child(img(image).size_full().object_fit(ObjectFit::Fill))
                            }))
                            .children(error.clone().map(|error| {
                                div()
                                    .p_4()
                                    .text_sm()
                                    .text_color(cx.theme().danger)
                                    .child(error)
                            }))
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if drag_enabled {
                        format!(
                            "{} {}",
                            t!("settings.watermark.preview.desc"),
                            t!("settings.watermark.preview.drag_hint")
                        )
                    } else {
                        t!("settings.watermark.preview.desc").to_string()
                    }),
            )
            .into_any_element()
    }

    fn render_layout_inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        const ROWS: [[WatermarkPlacement; 3]; 3] = [
            [
                WatermarkPlacement::TopLeft,
                WatermarkPlacement::TopCenter,
                WatermarkPlacement::TopRight,
            ],
            [
                WatermarkPlacement::MiddleLeft,
                WatermarkPlacement::Center,
                WatermarkPlacement::MiddleRight,
            ],
            [
                WatermarkPlacement::BottomLeft,
                WatermarkPlacement::BottomCenter,
                WatermarkPlacement::BottomRight,
            ],
        ];
        let theme = cx.theme().clone();
        let current = self.preset.placement;
        let tiled = self.preset.tiled;

        v_flex()
            .w(px(300.))
            .flex_none()
            .h_full()
            .min_h_0()
            .border_l_1()
            .border_color(theme.border)
            .p_3()
            .gap_3()
            .overflow_y_scrollbar()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("settings.watermark.editor.layout_desc").to_string()),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(Label::new(t!("settings.watermark.placement").to_string()).text_sm())
                    .child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .child(
                                v_flex()
                                    .gap(px(3.))
                                    .p(px(4.))
                                    .rounded_md()
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(theme.muted)
                                    .when(tiled, |this| this.opacity(0.5))
                                    .children(ROWS.into_iter().map(|row| {
                                        h_flex().gap(px(3.)).children(row.into_iter().map(|cell| {
                                            let active = cell == current;
                                            let label = placement_label(cell);
                                            Button::new(SharedString::from(format!(
                                                "wm-editor-place-{}",
                                                cell.as_str()
                                            )))
                                            .ghost()
                                            .p_0()
                                            .w(px(34.))
                                            .h(px(26.))
                                            .selected(active)
                                            .disabled(tiled)
                                            .tooltip(label.clone())
                                            .accessibility_label(label)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if this.preset.placement == cell {
                                                    return;
                                                }
                                                this.push_undo_snapshot();
                                                this.preset.placement = cell;
                                                this.refresh_layout_preview(cx);
                                            }))
                                            .child(
                                                div()
                                                    .size_full()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(div().size(px(7.)).rounded_full().bg(
                                                        if active {
                                                            theme.primary
                                                        } else {
                                                            theme.muted_foreground.opacity(0.45)
                                                        },
                                                    )),
                                            )
                                        }))
                                    })),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(if tiled {
                                        theme.muted_foreground
                                    } else {
                                        theme.foreground
                                    })
                                    .child(placement_label(current)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(Label::new(t!("settings.watermark.margin_x").to_string()).text_sm())
                    .child(self.compact_number_input(
                        5,
                        t!("settings.watermark.margin_x").to_string(),
                        px(110.),
                    )),
            )
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(Label::new(t!("settings.watermark.margin_y").to_string()).text_sm())
                    .child(self.compact_number_input(
                        6,
                        t!("settings.watermark.margin_y").to_string(),
                        px(110.),
                    )),
            )
            .child(div().h(px(1.)).w_full().bg(theme.border))
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(Label::new(t!("settings.watermark.tiled").to_string()).text_sm())
                    .child(
                        Switch::new("watermark-editor-tiled")
                            .small()
                            .checked(tiled)
                            .accessibility_label(t!("settings.watermark.tiled").to_string())
                            .on_change(cx.listener(|this, next: &bool, window, cx| {
                                if this.preset.tiled == *next {
                                    return;
                                }
                                this.push_undo_snapshot();
                                this.preset.tiled = *next;
                                this.sync_controls(window, cx);
                                this.refresh_layout_preview(cx);
                            })),
                    ),
            )
            .when(tiled, |this| {
                this.child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(Label::new(t!("settings.watermark.spacing_x").to_string()).text_sm())
                        .child(self.compact_number_input(
                            7,
                            t!("settings.watermark.spacing_x").to_string(),
                            px(110.),
                        )),
                )
                .child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(Label::new(t!("settings.watermark.spacing_y").to_string()).text_sm())
                        .child(self.compact_number_input(
                            8,
                            t!("settings.watermark.spacing_y").to_string(),
                            px(110.),
                        )),
                )
            })
            .child(div().h(px(1.)).w_full().bg(theme.border))
            .child(
                self.slider_field(
                    0,
                    t!("settings.watermark.editor.whole_rotation")
                        .to_string()
                        .into(),
                    self.preset.rotation_degrees,
                    cx,
                ),
            )
            .child(
                self.slider_field(
                    1,
                    t!("settings.watermark.editor.whole_scale")
                        .to_string()
                        .into(),
                    self.preset.scale,
                    cx,
                ),
            )
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_sm().text_color(theme.danger).child(error)),
            )
            .into_any_element()
    }

    fn render_layers_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.preset.elements.len();
        let selected_ix = self.selected_index();
        let can_move_up = selected_ix.is_some_and(|ix| ix + 1 < count);
        let can_move_down = selected_ix.is_some_and(|ix| ix > 0);
        let whole_selected = self.selected_id.is_none();

        v_flex()
            .w(px(208.))
            .flex_none()
            .h_full()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        Label::new(
                            t!("settings.watermark.editor.layers", count = count).to_string(),
                        )
                        .text_sm(),
                    )
                    .child(
                        Button::new("watermark-select-whole")
                            .xsmall()
                            .ghost()
                            .selected(whole_selected)
                            .toggled(whole_selected)
                            .label(t!("settings.watermark.editor.whole").to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.select_element(None, window, cx);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("watermark-elements-list")
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .p_1()
                    .gap_1()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().muted)
                    .overflow_y_scrollbar()
                    .children(self.preset.elements.iter().rev().map(|element| {
                        let id = element.id.clone();
                        let selected = self.selected_id.as_deref() == Some(id.as_str());
                        let (icon, title, meta) = match element.kind {
                            WatermarkKind::Text => {
                                let first_line =
                                    element.text.content.lines().next().unwrap_or("").trim();
                                let label = if first_line.is_empty() {
                                    t!("settings.watermark.kind.text").to_string()
                                } else {
                                    first_line.to_string()
                                };
                                (
                                    LucideIcon::Type,
                                    label,
                                    format!("{:.0}px", element.text.font_size),
                                )
                            }
                            WatermarkKind::Image => {
                                let label = if element.image.source.trim().is_empty() {
                                    t!("settings.watermark.image.none").to_string()
                                } else {
                                    element.image.source.clone()
                                };
                                (
                                    LucideIcon::Images,
                                    label,
                                    format!("{:.0}px", element.image.width),
                                )
                            }
                        };
                        let click_id = id.clone();
                        h_flex()
                            .id(ElementId::from(SharedString::from(format!(
                                "watermark-layer-{id}"
                            ))))
                            .w_full()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(selected, |row| row.bg(cx.theme().selection))
                            .when(!selected, |row| {
                                row.hover(|style| style.bg(cx.theme().background.opacity(0.6)))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_element(Some(click_id.clone()), window, cx);
                            }))
                            .child(Icon::new(icon).small())
                            .child(div().flex_1().min_w_0().truncate().text_sm().child(title))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(meta),
                            )
                    })),
            )
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .gap_1()
                    .child(
                        Button::new("watermark-layer-up")
                            .flex_1()
                            .xsmall()
                            .outline()
                            .icon(LucideIcon::ArrowUp)
                            .label(t!("settings.watermark.editor.move_up").to_string())
                            .disabled(!can_move_up)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.move_selected_layer(1, cx);
                            })),
                    )
                    .child(
                        Button::new("watermark-layer-down")
                            .flex_1()
                            .xsmall()
                            .outline()
                            .icon(LucideIcon::ArrowDown)
                            .label(t!("settings.watermark.editor.move_down").to_string())
                            .disabled(!can_move_down)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.move_selected_layer(-1, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let stamp = self.preview.clone();
        let error = self.preview_error.clone();
        let preview_layout = self.preview_layout.clone();
        let selected_id = self.selected_id.clone();
        let drag_camera = self.gesture.as_ref().map(|g| g.locked_camera());
        let zoom_override = self.zoom_override;
        let stamp_scale = self.preset.scale;
        let show_stamp_outline =
            self.preset.elements.len() > 1 || self.preset.rotation_degrees.abs() > 0.01;
        let is_empty = self.preset.elements.is_empty();
        let snap_guides = self.snap_guides.clone();

        div()
            .id("watermark-design-preview")
            .key_context(CONTEXT)
            .track_focus(&self.canvas_focus)
            .relative()
            .flex_1()
            .min_h(px(240.))
            .w_full()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(cx.theme().border)
            .overflow_hidden()
            .cursor(self.hover_cursor)
            .on_action(cx.listener(|this, _: &DeleteElement, window, cx| {
                this.delete_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DuplicateElement, window, cx| {
                this.duplicate_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &UndoEdit, window, cx| {
                this.undo(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RedoEdit, window, cx| {
                this.redo(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PasteImage, window, cx| {
                this.paste_from_clipboard(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NudgeUp, window, cx| {
                this.nudge_selected(0.0, -1.0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NudgeDown, window, cx| {
                this.nudge_selected(0.0, 1.0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NudgeLeft, window, cx| {
                this.nudge_selected(-1.0, 0.0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NudgeRight, window, cx| {
                this.nudge_selected(1.0, 0.0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NudgeUpFast, window, cx| {
                this.nudge_selected(0.0, -10.0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NudgeDownFast, window, cx| {
                this.nudge_selected(0.0, 10.0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NudgeLeftFast, window, cx| {
                this.nudge_selected(-10.0, 0.0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NudgeRightFast, window, cx| {
                this.nudge_selected(10.0, 0.0, window, cx);
            }))
            .on_action(cx.listener(|_, _: &CloseEditor, window, _| {
                window.remove_window();
            }))
            .on_drop::<ExternalPaths>(cx.listener(Self::on_drop_files))
            .on_scroll(cx.listener(Self::on_canvas_scroll))
            .on_pinch(cx.listener(Self::on_canvas_pinch))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_canvas_gesture))
            .on_mouse_move(cx.listener(Self::move_canvas_gesture))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::end_canvas_gesture))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::end_canvas_gesture))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let (light, dark) =
                            checkerboard_colors(cx.theme().background, cx.theme().foreground);
                        window.paint_quad(fill(bounds, light));
                        let cols = (bounds.size.width.as_f32() / CHECKER_TILE_PX).ceil() as usize;
                        let rows = (bounds.size.height.as_f32() / CHECKER_TILE_PX).ceil() as usize;
                        for row in 0..rows {
                            for col in 0..cols {
                                if (row + col).is_multiple_of(2) {
                                    continue;
                                }
                                let x = bounds.origin.x + px(col as f32 * CHECKER_TILE_PX);
                                let y = bounds.origin.y + px(row as f32 * CHECKER_TILE_PX);
                                let w =
                                    px(CHECKER_TILE_PX.min((bounds.right() - x).as_f32().max(0.0)));
                                let h =
                                    px(CHECKER_TILE_PX
                                        .min((bounds.bottom() - y).as_f32().max(0.0)));
                                if w > px(0.) && h > px(0.) {
                                    window.paint_quad(fill(
                                        Bounds::new(point(x, y), size(w, h)),
                                        dark,
                                    ));
                                }
                            }
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .when(!is_empty, |this| {
                this.child(
                    container_query(move |bounds, _, cx| {
                        let Some(stamp) = stamp.clone() else {
                            preview_layout.set(None);
                            return div().size_full().into_any_element();
                        };
                        let viewport_w = bounds.width.as_f32();
                        let viewport_h = bounds.height.as_f32();
                        let world_left = stamp.origin.0 * stamp_scale;
                        let world_top = stamp.origin.1 * stamp_scale;
                        let (zoom, camera_center) = drag_camera.unwrap_or_else(|| {
                            let avail_w = (viewport_w - 56.0).max(40.0);
                            let avail_h = (viewport_h - 56.0).max(40.0);
                            let fit = (avail_w / stamp.logical_size.0.max(1.0))
                                .min(avail_h / stamp.logical_size.1.max(1.0));
                            let zoom = zoom_override.unwrap_or_else(|| fit.clamp(0.25, 2.0));
                            let center = (
                                world_left + stamp.logical_size.0 * 0.5,
                                world_top + stamp.logical_size.1 * 0.5,
                            );
                            (zoom, center)
                        });
                        let stamp_w = stamp.logical_size.0 * zoom;
                        let stamp_h = stamp.logical_size.1 * zoom;
                        let rel_x = viewport_w * 0.5 + (world_left - camera_center.0) * zoom;
                        let rel_y = viewport_h * 0.5 + (world_top - camera_center.1) * zoom;

                        let selected_bounds = selected_id.as_deref().and_then(|id| {
                            stamp
                                .element_bounds
                                .iter()
                                .find(|(candidate, _)| candidate == id)
                                .map(|(_, b)| *b)
                        });
                        let border_color = cx.theme().border;
                        let primary_color = cx.theme().primary;
                        let handle_fill = cx.theme().background;
                        let layout_capture = preview_layout.clone();
                        let snap_guides = snap_guides.clone();

                        div()
                            .relative()
                            .size_full()
                            .child(
                                div()
                                    .absolute()
                                    .left(px(rel_x))
                                    .top(px(rel_y))
                                    .w(px(stamp_w))
                                    .h(px(stamp_h))
                                    .child(
                                        img(stamp.image).size_full().object_fit(ObjectFit::Fill),
                                    ),
                            )
                            .child(
                                canvas(
                                    |_, _, _| (),
                                    move |canvas_bounds, _, window, _| {
                                        let stamp_origin = point(
                                            canvas_bounds.origin.x + px(rel_x),
                                            canvas_bounds.origin.y + px(rel_y),
                                        );
                                        let stamp_size = size(px(stamp_w), px(stamp_h));
                                        layout_capture.set(Some(PreviewLayout {
                                            stamp_origin,
                                            zoom,
                                            camera_center,
                                            stamp_world_origin: (world_left, world_top),
                                        }));
                                        if show_stamp_outline {
                                            paint_rect_outline(
                                                Bounds::new(stamp_origin, stamp_size),
                                                border_color,
                                                window,
                                            );
                                        }
                                        for guide in &snap_guides {
                                            if guide.vertical {
                                                let sx = canvas_bounds.origin.x
                                                    + px(viewport_w * 0.5
                                                        + (guide.world_pos - camera_center.0)
                                                            * zoom);
                                                window.paint_quad(fill(
                                                    Bounds::new(
                                                        point(sx, canvas_bounds.origin.y),
                                                        size(px(1.), canvas_bounds.size.height),
                                                    ),
                                                    primary_color.opacity(0.85),
                                                ));
                                            } else {
                                                let sy = canvas_bounds.origin.y
                                                    + px(viewport_h * 0.5
                                                        + (guide.world_pos - camera_center.1)
                                                            * zoom);
                                                window.paint_quad(fill(
                                                    Bounds::new(
                                                        point(canvas_bounds.origin.x, sy),
                                                        size(canvas_bounds.size.width, px(1.)),
                                                    ),
                                                    primary_color.opacity(0.85),
                                                ));
                                            }
                                        }
                                        if let Some(el_bounds) = selected_bounds {
                                            let rect = Bounds::new(
                                                point(
                                                    stamp_origin.x + px(el_bounds.x * zoom),
                                                    stamp_origin.y + px(el_bounds.y * zoom),
                                                ),
                                                size(
                                                    px(el_bounds.width * zoom),
                                                    px(el_bounds.height * zoom),
                                                ),
                                            );
                                            paint_selection_handles(
                                                rect,
                                                primary_color,
                                                handle_fill,
                                                window,
                                            );
                                        }
                                    },
                                )
                                .absolute()
                                .inset_0(),
                            )
                            .into_any_element()
                    })
                    .absolute()
                    .inset_0(),
                )
            })
            .when(is_empty, |this| {
                this.child(
                    v_flex()
                        .absolute()
                        .inset_0()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .p_4()
                        .child(
                            v_flex()
                                .px_4()
                                .py_3()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().background.opacity(0.92))
                                .border_1()
                                .border_color(cx.theme().border)
                                .items_center()
                                .gap_2()
                                .child(
                                    Label::new(
                                        t!("settings.watermark.editor.empty_title").to_string(),
                                    )
                                    .text_sm(),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(
                                            t!("settings.watermark.editor.empty_desc").to_string(),
                                        ),
                                )
                                .child(h_flex().flex_wrap().justify_center().gap_1().children(
                                    WatermarkStarter::ALL.into_iter().skip(1).map(|starter| {
                                        Button::new(format!("empty-starter-{}", starter.as_str()))
                                            .xsmall()
                                            .outline()
                                            .label(starter.label())
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.apply_starter(starter, window, cx);
                                            }))
                                    }),
                                )),
                        ),
                )
            })
            .children(error.map(|error| {
                div()
                    .absolute()
                    .bottom_3()
                    .left_3()
                    .right_3()
                    .px_3()
                    .py_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().background.opacity(0.92))
                    .border_1()
                    .border_color(cx.theme().danger)
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error)
            }))
    }

    fn render_center_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        let stamp_size = self.preview.as_ref().map(|preview| {
            format!(
                "{:.0} × {:.0} px",
                preview.logical_size.0, preview.logical_size.1
            )
        });
        let zoom_pct = self
            .zoom_override
            .or_else(|| self.preview_layout.get().map(|layout| layout.zoom))
            .map_or(100, |z| (z * 100.0).round() as u32);
        let zoom_label = format!("{zoom_pct}%");

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .when_some(stamp_size, |this, size_text| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(size_text),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("watermark-zoom-out")
                                    .xsmall()
                                    .ghost()
                                    .icon(LucideIcon::ZoomOut)
                                    .tooltip(t!("settings.watermark.editor.zoom_out").to_string())
                                    .accessibility_label(
                                        t!("settings.watermark.editor.zoom_out").to_string(),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.adjust_zoom(Some(0.8), cx);
                                    })),
                            )
                            .child(
                                Button::new("watermark-zoom-fit")
                                    .xsmall()
                                    .ghost()
                                    .label(zoom_label)
                                    .tooltip(t!("settings.watermark.editor.zoom_fit").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.adjust_zoom(None, cx);
                                    })),
                            )
                            .child(
                                Button::new("watermark-zoom-in")
                                    .xsmall()
                                    .ghost()
                                    .icon(LucideIcon::ZoomIn)
                                    .tooltip(t!("settings.watermark.editor.zoom_in").to_string())
                                    .accessibility_label(
                                        t!("settings.watermark.editor.zoom_in").to_string(),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.adjust_zoom(Some(1.25), cx);
                                    })),
                            ),
                    ),
            )
            .child(self.render_preview(cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("settings.watermark.editor.drag_hint").to_string()),
            )
            .into_any_element()
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected_element().cloned();
        let editor = cx.entity();

        v_flex()
            .w(px(300.))
            .flex_none()
            .h_full()
            .min_h_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .p_3()
            .gap_3()
            .overflow_y_scrollbar()
            .map(|panel| match selected {
                Some(element) => match element.kind {
                    WatermarkKind::Text => panel
                        .child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new(t!("settings.watermark.text_content").to_string())
                                        .text_sm(),
                                )
                                .child(
                                    Textarea::new(&self.text_input)
                                        .h(px(62.))
                                        .w_full()
                                        .aria_label(
                                            t!("settings.watermark.text_content").to_string(),
                                        ),
                                )
                                .child(h_flex().items_center().gap_1().flex_wrap().children(
                                    TEMPLATE_VARIABLES.into_iter().map(|token| {
                                        Button::new(format!("var-{token}"))
                                            .xsmall()
                                            .ghost()
                                            .label(token)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.insert_text_variable(token, window, cx);
                                            }))
                                    }),
                                )),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            Label::new(
                                                t!("settings.watermark.font_size").to_string(),
                                            )
                                            .text_sm(),
                                        )
                                        .child(self.compact_number_input(
                                            0,
                                            t!("settings.watermark.font_size").to_string(),
                                            px(84.),
                                        )),
                                )
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            ColorPicker::new(&self.color)
                                                .with_size(px(24.))
                                                .accessibility_label(
                                                    t!("settings.watermark.color").to_string(),
                                                ),
                                        )
                                        .child(
                                            Button::new("watermark-text-bold")
                                                .xsmall()
                                                .outline()
                                                .selected(element.text.bold)
                                                .toggled(element.text.bold)
                                                .icon(LucideIcon::Bold)
                                                .tooltip(
                                                    t!("settings.watermark.text.bold").to_string(),
                                                )
                                                .accessibility_label(
                                                    t!("settings.watermark.text.bold").to_string(),
                                                )
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.push_undo_snapshot();
                                                    if let Some(el) = this.selected_element_mut() {
                                                        el.text.bold = !el.text.bold;
                                                    }
                                                    this.refresh_preview(cx);
                                                })),
                                        ),
                                ),
                        )
                        .child(self.slider_field(
                            2,
                            t!("settings.watermark.opacity").to_string().into(),
                            element.opacity,
                            cx,
                        ))
                        .child(self.slider_field(
                            0,
                            t!("settings.watermark.rotation").to_string().into(),
                            element.rotation_degrees,
                            cx,
                        ))
                        .child(div().h(px(1.)).w_full().bg(cx.theme().border))
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    Label::new(t!("settings.watermark.text.shadow").to_string())
                                        .text_sm(),
                                )
                                .child(
                                    Switch::new("watermark-text-shadow")
                                        .small()
                                        .checked(element.text.shadow)
                                        .accessibility_label(
                                            t!("settings.watermark.text.shadow").to_string(),
                                        )
                                        .on_change(cx.listener(|this, next: &bool, _, cx| {
                                            this.push_undo_snapshot();
                                            if let Some(el) = this.selected_element_mut() {
                                                el.text.shadow = *next;
                                            }
                                            this.refresh_preview(cx);
                                        })),
                                ),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    Label::new(t!("settings.watermark.text.stroke").to_string())
                                        .text_sm(),
                                )
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .child(self.compact_number_input(
                                            4,
                                            t!("settings.watermark.text.stroke").to_string(),
                                            px(84.),
                                        ))
                                        .child(
                                            ColorPicker::new(&self.stroke_color)
                                                .with_size(px(22.))
                                                .accessibility_label(
                                                    t!("settings.watermark.text.stroke_color")
                                                        .to_string(),
                                                ),
                                        ),
                                ),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    Label::new(
                                        t!("settings.watermark.text.background").to_string(),
                                    )
                                    .text_sm(),
                                )
                                .child(
                                    h_flex()
                                        .items_center()
                                        .gap_2()
                                        .when(element.text.background, |row| {
                                            row.child(
                                                ColorPicker::new(&self.background_color)
                                                    .with_size(px(22.))
                                                    .accessibility_label(
                                                        t!("settings.watermark.text.background")
                                                            .to_string(),
                                                    ),
                                            )
                                        })
                                        .child(
                                            Switch::new("watermark-text-background")
                                                .small()
                                                .checked(element.text.background)
                                                .accessibility_label(
                                                    t!("settings.watermark.text.background")
                                                        .to_string(),
                                                )
                                                .on_change(cx.listener(
                                                    |this, next: &bool, _, cx| {
                                                        this.push_undo_snapshot();
                                                        if let Some(el) =
                                                            this.selected_element_mut()
                                                        {
                                                            el.text.background = *next;
                                                        }
                                                        this.refresh_preview(cx);
                                                    },
                                                )),
                                        ),
                                ),
                        )
                        .when(element.text.background, |this| {
                            this.child(
                                self.slider_field(
                                    3,
                                    t!("settings.watermark.text.background_opacity")
                                        .to_string()
                                        .into(),
                                    element.text.background_opacity,
                                    cx,
                                ),
                            )
                        })
                        .child(div().h(px(1.)).w_full().bg(cx.theme().border))
                        .child(self.xy_row()),
                    WatermarkKind::Image => panel
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(if element.image.source.trim().is_empty() {
                                            t!("settings.watermark.image.none").to_string()
                                        } else {
                                            element.image.source.clone()
                                        }),
                                )
                                .child(
                                    Button::new("watermark-replace-image")
                                        .small()
                                        .outline()
                                        .label(t!("settings.watermark.image.replace").to_string())
                                        .on_click({
                                            let editor = editor.clone();
                                            let element_id = element.id.clone();
                                            move |_, _, cx| {
                                                replace_image(
                                                    editor.clone(),
                                                    element_id.clone(),
                                                    cx,
                                                );
                                            }
                                        }),
                                ),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .child(
                                    Label::new(t!("settings.watermark.image_width_px").to_string())
                                        .text_sm(),
                                )
                                .child(self.compact_number_input(
                                    1,
                                    t!("settings.watermark.image_width_px").to_string(),
                                    px(110.),
                                )),
                        )
                        .child(
                            self.slider_field(
                                4,
                                t!("settings.watermark.image.corner_radius")
                                    .to_string()
                                    .into(),
                                element.image.corner_radius,
                                cx,
                            ),
                        )
                        .child(self.slider_field(
                            2,
                            t!("settings.watermark.opacity").to_string().into(),
                            element.opacity,
                            cx,
                        ))
                        .child(self.slider_field(
                            0,
                            t!("settings.watermark.rotation").to_string().into(),
                            element.rotation_degrees,
                            cx,
                        ))
                        .child(div().h(px(1.)).w_full().bg(cx.theme().border))
                        .child(self.xy_row()),
                },
                None => panel
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("settings.watermark.editor.whole_desc").to_string()),
                    )
                    .child(
                        self.slider_field(
                            0,
                            t!("settings.watermark.editor.whole_rotation")
                                .to_string()
                                .into(),
                            self.preset.rotation_degrees,
                            cx,
                        ),
                    )
                    .child(
                        self.slider_field(
                            1,
                            t!("settings.watermark.editor.whole_scale")
                                .to_string()
                                .into(),
                            self.preset.scale,
                            cx,
                        ),
                    ),
            })
            .children(
                self.error
                    .clone()
                    .map(|error| div().text_sm().text_color(cx.theme().danger).child(error)),
            )
            .into_any_element()
    }

    fn xy_row(&self) -> AnyElement {
        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(
                h_flex()
                    .flex_1()
                    .items_center()
                    .gap_1()
                    .child(Label::new("X").text_xs())
                    .child(self.compact_number_input(
                        2,
                        t!("settings.watermark.offset_x").to_string(),
                        px(92.),
                    )),
            )
            .child(
                h_flex()
                    .flex_1()
                    .items_center()
                    .gap_1()
                    .child(Label::new("Y").text_xs())
                    .child(self.compact_number_input(
                        3,
                        t!("settings.watermark.offset_y").to_string(),
                        px(92.),
                    )),
            )
            .into_any_element()
    }

    fn compact_number_input(&self, index: usize, label: String, width: Pixels) -> AnyElement {
        NumberInput::new(&self.number_inputs[index])
            .small()
            .w(width)
            .accessibility_label(label)
            .into_any_element()
    }

    fn slider_field(&self, index: usize, label: SharedString, value: f32, cx: &App) -> AnyElement {
        let field = SLIDER_FIELDS[index];
        let caption = match field {
            SliderField::Opacity | SliderField::BackgroundOpacity => {
                format!("{:.0}%", value * 100.0)
            }
            SliderField::Scale => format!("{value:.1}×"),
            SliderField::Rotation => format!("{value:.0}°"),
            SliderField::CornerRadius => {
                if value >= 49.5 {
                    t!("settings.watermark.image.corner_circle").to_string()
                } else {
                    format!("{value:.0}%")
                }
            }
        };
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(Label::new(label).text_sm())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(caption),
                    ),
            )
            .child(Slider::new(&self.sliders[index]).w_full())
            .into_any_element()
    }
}

impl Render for WatermarkEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_save = self.can_save();
        let editor = cx.entity();

        let choices = template_choices(&AppSettings::get(cx).watermark);
        let selected_value = self.template_id.clone().unwrap_or_default();
        self.template_select.update(cx, |state, cx| {
            if choices != self.last_template_choices {
                self.last_template_choices = choices.clone();
                state.set_items(choices.into(), window, cx);
            }
            if state.selected_value() != Some(&selected_value) {
                state.set_selected_value(&selected_value, window, cx);
            }
        });
        let can_delete = self
            .template_id
            .as_deref()
            .is_some_and(|id| id != WatermarkConfig::DEFAULT_TEMPLATE_ID);

        let template_bar = h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(Label::new(
                t!("settings.watermark.editor.template").to_string(),
            ))
            .child(div().w(px(190.)).child(Select::new(&self.template_select)))
            .child(div().w(px(170.)).child(Input::new(&self.name_input)))
            .children(
                self.name_error
                    .clone()
                    .map(|error| div().text_sm().text_color(cx.theme().danger).child(error)),
            )
            .child(
                Button::new("watermark-template-new")
                    .small()
                    .label(t!("settings.watermark.template.new").to_string())
                    .on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            editor.update(cx, |this, cx| {
                                this.confirm_discard(window, cx, TemplateAction::New);
                            });
                        }
                    }),
            )
            .child(
                Button::new("watermark-template-duplicate")
                    .small()
                    .label(t!("settings.watermark.template.duplicate").to_string())
                    .on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            editor.update(cx, |this, cx| {
                                this.confirm_discard(window, cx, TemplateAction::Duplicate);
                            });
                        }
                    }),
            )
            .child(
                Button::new("watermark-template-delete")
                    .small()
                    .label(t!("settings.watermark.template.delete").to_string())
                    .disabled(!can_delete)
                    .on_click({
                        let editor = editor.clone();
                        move |_, window, cx| {
                            editor.update(cx, |this, cx| this.confirm_delete(window, cx));
                        }
                    }),
            );

        let selected_mode_ix = match self.mode {
            EditorMode::Design => 0,
            EditorMode::Layout => 1,
        };
        let mode_tabs = TabBar::new("watermark-editor-mode")
            .segmented()
            .selected_index(selected_mode_ix)
            .children([
                Tab::new().label(t!("settings.watermark.editor.mode.design").to_string()),
                Tab::new().label(t!("settings.watermark.editor.mode.layout").to_string()),
            ])
            .on_click(move |ix, window, cx| {
                let next = match *ix {
                    0 => EditorMode::Design,
                    1 => EditorMode::Layout,
                    _ => return,
                };
                editor.update(cx, |this, cx| this.set_mode(next, window, cx));
            });

        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(template_bar)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(mode_tabs)
                    .child(match self.mode {
                        EditorMode::Design => self.render_toolbar(cx),
                        EditorMode::Layout => self.render_layout_toolbar(cx),
                    }),
            )
            .child(match self.mode {
                EditorMode::Design => h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_layers_pane(cx))
                    .child(self.render_center_pane(cx))
                    .child(self.render_inspector(cx)),
                EditorMode::Layout => h_flex()
                    .items_stretch()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_layout_preview_pane(cx))
                    .child(self.render_layout_inspector(cx)),
            })
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("watermark-editor-cancel")
                            .label(t!("settings.watermark.editor.cancel").to_string())
                            .on_click(|_, window, _| window.remove_window()),
                    )
                    .child(
                        Button::new("watermark-editor-save")
                            .label(t!("settings.watermark.editor.save").to_string())
                            .primary()
                            .disabled(!can_save)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.save(window, cx) {
                                    window.remove_window();
                                }
                            })),
                    ),
            )
    }
}

pub(crate) fn open(
    window: &mut Window,
    cx: &mut App,
    target: WatermarkTarget,
    template_id: Option<String>,
    initial_preset: WatermarkPreset,
) {
    let display_id = window.display(cx).map(|display| display.id());
    Controller::global(cx).update(cx, |controller, cx| {
        controller.open_watermark_editor(target, template_id, initial_preset, display_id, cx);
    });
}

pub(crate) fn open_window(
    target: WatermarkTarget,
    template_id: Option<String>,
    initial_preset: WatermarkPreset,
    display_id: Option<DisplayId>,
    cx: &mut App,
) -> anyhow::Result<AnyWindowHandle> {
    let watermark = AppSettings::get(cx).watermark.clone();
    let preset = template_id
        .as_ref()
        .and_then(|id| watermark.templates.get(id))
        .map(|template| template.preset.clone())
        .unwrap_or(initial_preset);

    let fallback_name = template_id
        .as_deref()
        .and_then(|id| watermark.templates.get(id))
        .map(|template| template.name.clone())
        .unwrap_or_else(|| watermark.unique_template_name("Watermark"));

    let window_size = size(px(1080.), px(700.));
    let min_size = size(px(860.), px(580.));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            display_id,
            window_size,
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(t!("settings.watermark.editor.title").to_string().into()),
            ..Default::default()
        }),
        window_min_size: Some(min_size),
        display_id,
        ..Default::default()
    };
    let handle = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| WatermarkEditor::new(target, template_id, fallback_name, preset, window, cx))
    })?
    .0;
    Ok(handle)
}

fn add_images(editor: Entity<WatermarkEditor>, cx: &mut App) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some(t!("settings.watermark.image.pick").to_string().into()),
    });
    cx.spawn(async move |cx| {
        let Some(paths) = picked.await.ok().and_then(Result::ok).flatten() else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        cx.update(|cx| {
            let dir = AppSettings::watermark_dir(cx).to_path_buf();
            let window_handle = editor.read(cx).window_handle;
            let _ = window_handle.update(cx, |_, window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.import_image_paths(&paths, &dir);
                    editor.sync_controls(window, cx);
                    editor.refresh_preview(cx);
                });
            });
        });
    })
    .detach();
}

fn replace_image(editor: Entity<WatermarkEditor>, element_id: String, cx: &mut App) {
    let editor = editor.downgrade();
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some(t!("settings.watermark.image.pick").to_string().into()),
    });
    cx.spawn(async move |cx| {
        let Some(path) = picked
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
            .and_then(|paths| paths.into_iter().next())
        else {
            return;
        };
        let dir = cx.update(|cx| AppSettings::watermark_dir(cx).to_path_buf());
        let import = cx.background_spawn({
            let path = path.clone();
            let dir = dir.clone();
            async move { crate::settings_view::import_watermark_image(&path, &dir) }
        });
        let result = import.await;
        if let Ok((source, _, _)) = &result
            && editor
                .update(cx, |editor, _| {
                    editor.pending_assets.track(&dir, source.clone());
                })
                .is_err()
        {
            let _ = std::fs::remove_file(dir.join(source));
            return;
        }
        cx.update(|cx| {
            let Ok(window_handle) = editor.update(cx, |editor, _| editor.window_handle) else {
                return;
            };
            if !window_handle
                .update(cx, |_, window, cx| {
                    editor
                        .update(cx, |editor, cx| match result {
                            Ok((source, natural_w, _)) => {
                                editor.push_undo_snapshot();
                                if let Some(element) = editor
                                    .preset
                                    .elements
                                    .iter_mut()
                                    .find(|el| el.id == element_id)
                                {
                                    element.kind = WatermarkKind::Image;
                                    element.image.source = source;
                                    element.image.width = initial_image_width(natural_w);
                                    element.scale = 1.0;
                                }
                                editor.error = None;
                                editor.sync_controls(window, cx);
                                editor.refresh_preview(cx);
                            }
                            Err(err) => {
                                let message = format!("{err:#}");
                                log::warn!("watermark image import failed: {message}");
                                editor.error = Some(message);
                                cx.notify();
                            }
                        })
                        .is_ok()
                })
                .is_ok_and(|applied| applied)
            {
                let _ = editor.update(cx, |editor, _| {
                    let retained = watermark_retained_references(
                        &editor.preset,
                        &editor.undo_stack,
                        &editor.redo_stack,
                        editor.slider_undo_snapshot.as_ref(),
                    );
                    editor.pending_assets.cleanup(&retained);
                });
            }
        });
    })
    .detach();
}

fn next_staggered_offset(editor: &WatermarkEditor) -> (f32, f32) {
    if editor.preset.elements.is_empty() {
        return (0.0, 0.0);
    }
    let count = editor.preset.elements.len() as f32;
    let (_, base_y) = editor.next_element_position();
    ((count - 1.0) * 8.0, base_y)
}

fn initial_image_width(natural_width: u32) -> f32 {
    if natural_width <= 160 {
        (natural_width as f32).clamp(16.0, 160.0)
    } else {
        120.0
    }
}

fn build_stamp_preview(
    preset: &WatermarkPreset,
    image_dir: &Path,
) -> std::result::Result<StampPreview, String> {
    if preset.elements.is_empty() {
        return Err(t!("settings.watermark.elements.required").to_string());
    }
    let spec = crate::export::watermark_spec(preset, image_dir).map_err(|err| err.to_string())?;
    let stamp = spec
        .compose_stamp(PREVIEW_SCALE)
        .map_err(|err| err.to_string())?;
    let logical_size = (
        stamp.image.width() as f32 / PREVIEW_SCALE,
        stamp.image.height() as f32 / PREVIEW_SCALE,
    );
    let element_bounds = preset
        .elements
        .iter()
        .zip(stamp.rendered_element_bounds)
        .map(|(el, bounds)| (el.id.clone(), bounds))
        .collect();
    Ok(StampPreview {
        image: crate::session::render_image(&stamp.image),
        logical_size,
        origin: (stamp.logical_origin_x, stamp.logical_origin_y),
        element_bounds,
    })
}

fn checkerboard_colors(background: Hsla, _foreground: Hsla) -> (Hsla, Hsla) {
    if background.l < 0.5 {
        let light = Hsla {
            l: (background.l + 0.14).clamp(0.18, 0.34),
            s: 0.0,
            a: 1.0,
            ..background
        };
        let dark = Hsla {
            l: (background.l + 0.02).clamp(0.08, 0.20),
            s: 0.0,
            a: 1.0,
            ..background
        };
        (light, dark)
    } else {
        let light = Hsla {
            l: (background.l - 0.24).clamp(0.56, 0.75),
            s: 0.0,
            a: 1.0,
            ..background
        };
        let dark = Hsla {
            l: (background.l - 0.44).clamp(0.38, 0.55),
            s: 0.0,
            a: 1.0,
            ..background
        };
        (light, dark)
    }
}

fn paint_rect_outline(rect: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let t = px(1.);
    let x = rect.origin.x;
    let y = rect.origin.y;
    let w = rect.size.width;
    let h = rect.size.height;
    for edge in [
        Bounds::new(point(x, y), size(w, t)),
        Bounds::new(point(x, y + h - t), size(w, t)),
        Bounds::new(point(x, y), size(t, h)),
        Bounds::new(point(x + w - t, y), size(t, h)),
    ] {
        window.paint_quad(fill(edge, color));
    }
}

fn paint_selection_handles(
    rect: Bounds<Pixels>,
    primary: Hsla,
    handle_fill: Hsla,
    window: &mut Window,
) {
    paint_rect_outline(rect, primary, window);
    let stem_x = rect.origin.x + rect.size.width * 0.5;
    let stem_top = rect.origin.y - px(ROTATE_STEM_PX);
    window.paint_quad(fill(
        Bounds::new(
            point(stem_x - px(0.5), stem_top),
            size(px(1.), px(ROTATE_STEM_PX)),
        ),
        primary,
    ));
    let rot_size = px(8.);
    let rot_box = Bounds::new(
        point(stem_x - rot_size * 0.5, stem_top - rot_size * 0.5),
        size(rot_size, rot_size),
    );
    window.paint_quad(fill(rot_box, handle_fill));
    paint_rect_outline(rot_box, primary, window);

    let handle_size = px(7.);
    let half = handle_size * 0.5;
    for (cx, cy) in [
        (rect.origin.x, rect.origin.y),
        (rect.right(), rect.origin.y),
        (rect.origin.x, rect.bottom()),
        (rect.right(), rect.bottom()),
    ] {
        let box_bounds = Bounds::new(point(cx - half, cy - half), size(handle_size, handle_size));
        window.paint_quad(fill(box_bounds, handle_fill));
        paint_rect_outline(box_bounds, primary, window);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HandleHit {
    None,
    Rotate,
    Corner(bool),
}

fn snap_axis(
    cand_start: f32,
    cand_size: f32,
    siblings: &[WorldBox],
    horizontal: bool,
    threshold: f32,
) -> (f32, Option<f32>) {
    let cand_edges = [
        cand_start,
        cand_start + cand_size * 0.5,
        cand_start + cand_size,
    ];
    let mut best_dist = threshold;
    let mut best_delta = 0.0f32;
    let mut best_guide = None;

    for sib in siblings {
        let (s0, s_size) = if horizontal {
            (sib.x, sib.width)
        } else {
            (sib.y, sib.height)
        };
        let sib_edges = [s0, s0 + s_size * 0.5, s0 + s_size];
        for &ce in &cand_edges {
            for &se in &sib_edges {
                let delta = se - ce;
                let dist = delta.abs();
                if dist < best_dist {
                    best_dist = dist;
                    best_delta = delta;
                    best_guide = Some(se);
                }
            }
        }
    }
    (best_delta, best_guide)
}

fn apply_align_op(elements: &mut [WatermarkElement], sizes: &[(f32, f32)], op: AlignOp) {
    if elements.len() < 2 || elements.len() != sizes.len() {
        return;
    }
    let min_x = elements.iter().map(|e| e.x).fold(f32::INFINITY, f32::min);
    let max_right = elements
        .iter()
        .zip(sizes)
        .map(|(e, (w, _))| e.x + w)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = elements.iter().map(|e| e.y).fold(f32::INFINITY, f32::min);
    let max_bottom = elements
        .iter()
        .zip(sizes)
        .map(|(e, (_, h))| e.y + h)
        .fold(f32::NEG_INFINITY, f32::max);

    match op {
        AlignOp::Left => {
            for el in elements {
                el.x = min_x.round();
            }
        }
        AlignOp::CenterH => {
            let cx = (min_x + max_right) * 0.5;
            for (el, (w, _)) in elements.iter_mut().zip(sizes) {
                el.x = (cx - w * 0.5).round();
            }
        }
        AlignOp::CenterV => {
            let cy = (min_y + max_bottom) * 0.5;
            for (el, (_, h)) in elements.iter_mut().zip(sizes) {
                el.y = (cy - h * 0.5).round();
            }
        }
        AlignOp::Row => {
            let cy = (min_y + max_bottom) * 0.5;
            let mut order: Vec<usize> = (0..elements.len()).collect();
            order.sort_by(|&a, &b| elements[a].x.total_cmp(&elements[b].x));
            let mut cursor_x = min_x;
            for ix in order {
                let (w, h) = sizes[ix];
                elements[ix].x = cursor_x.round();
                elements[ix].y = (cy - h * 0.5).round();
                cursor_x += w + 12.0;
            }
        }
        AlignOp::Column => {
            let cx = (min_x + max_right) * 0.5;
            let mut order: Vec<usize> = (0..elements.len()).collect();
            order.sort_by(|&a, &b| elements[a].y.total_cmp(&elements[b].y));
            let mut cursor_y = min_y;
            for ix in order {
                let (w, h) = sizes[ix];
                elements[ix].x = (cx - w * 0.5).round();
                elements[ix].y = cursor_y.round();
                cursor_y += h + 10.0;
            }
        }
    }
}

fn pinch_zoom(base: f32, delta: f32) -> f32 {
    (base * (1.0 + delta).max(0.01)).clamp(0.25, 4.0)
}

fn normalize_angle(mut angle: f32) -> f32 {
    while angle > 180.0 {
        angle -= 360.0;
    }
    while angle < -180.0 {
        angle += 360.0;
    }
    angle
}

fn inverse_stamp_delta(
    screen_dx: f32,
    screen_dy: f32,
    zoom: f32,
    stamp_scale: f32,
    rotation_degrees: f32,
) -> (f32, f32) {
    let factor = (zoom * stamp_scale).max(0.01);
    let sx = screen_dx / factor;
    let sy = screen_dy / factor;
    let (sin, cos) = rotation_degrees.to_radians().sin_cos();
    (sx * cos + sy * sin, -sx * sin + sy * cos)
}

fn preset_is_valid(preset: &WatermarkPreset) -> bool {
    !preset.elements.is_empty()
        && preset.elements.iter().all(|element| match element.kind {
            WatermarkKind::Text => !element.text.content.trim().is_empty(),
            WatermarkKind::Image => !element.image.source.trim().is_empty(),
        })
}

fn format_number(value: f64) -> String {
    if (value - value.round()).abs() < 0.001 {
        format!("{:.0}", value.round())
    } else {
        format!("{value:.1}")
    }
}

pub(crate) fn template_choices(watermark: &WatermarkConfig) -> Vec<WatermarkChoice> {
    let mut templates: Vec<_> = watermark.templates.iter().collect();
    templates.sort_by(|(_, left), (_, right)| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
    });
    templates
        .into_iter()
        .map(|(id, template)| WatermarkChoice {
            value: id.clone(),
            title: template.name.clone().into(),
        })
        .collect()
}

pub(crate) fn placement_label(placement: WatermarkPlacement) -> String {
    match placement {
        WatermarkPlacement::TopLeft => t!("settings.watermark.placement.top_left"),
        WatermarkPlacement::TopCenter => t!("settings.watermark.placement.top_center"),
        WatermarkPlacement::TopRight => t!("settings.watermark.placement.top_right"),
        WatermarkPlacement::MiddleLeft => t!("settings.watermark.placement.middle_left"),
        WatermarkPlacement::Center => t!("settings.watermark.placement.center"),
        WatermarkPlacement::MiddleRight => t!("settings.watermark.placement.middle_right"),
        WatermarkPlacement::BottomLeft => t!("settings.watermark.placement.bottom_left"),
        WatermarkPlacement::BottomCenter => t!("settings.watermark.placement.bottom_center"),
        WatermarkPlacement::BottomRight => t!("settings.watermark.placement.bottom_right"),
    }
    .to_string()
}

impl NumberField {
    fn is_layout(self) -> bool {
        matches!(
            self,
            Self::MarginX | Self::MarginY | Self::TileSpacingX | Self::TileSpacingY
        )
    }
}

fn number_range(field: NumberField) -> (f64, f64, f64) {
    match field {
        NumberField::FontSize => (8.0, 256.0, 1.0),
        NumberField::ImageWidth => (8.0, 2048.0, 4.0),
        NumberField::OffsetX | NumberField::OffsetY => (-4096.0, 4096.0, 1.0),
        NumberField::StrokeWidth => (0.0, 12.0, 0.5),
        NumberField::MarginX | NumberField::MarginY => (0.0, 2048.0, 4.0),
        NumberField::TileSpacingX | NumberField::TileSpacingY => (8.0, 4096.0, 8.0),
    }
}

fn slider_range(field: SliderField) -> (f32, f32, f32) {
    match field {
        SliderField::Rotation => (-180.0, 180.0, 1.0),
        SliderField::Scale => (0.1, 8.0, 0.1),
        SliderField::Opacity | SliderField::BackgroundOpacity => (0.0, 1.0, 0.01),
        SliderField::CornerRadius => (0.0, 50.0, 1.0),
    }
}

fn color_to_rgb(color: Hsla) -> u32 {
    let rgba = color.to_rgb();
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    (channel(rgba.r) << 16) | (channel(rgba.g) << 8) | channel(rgba.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkerboard_keeps_clear_lightness_separation_in_light_and_dark_themes() {
        let light_bg = Hsla {
            h: 0.0,
            s: 0.0,
            l: 1.0,
            a: 1.0,
        };
        let dark_bg = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.1,
            a: 1.0,
        };
        let (ll, ld) = checkerboard_colors(light_bg, dark_bg);
        assert!(ll.l - ld.l >= 0.08);
        assert!(ll.l <= 0.75 && ld.l >= 0.45);

        let (dl, dd) = checkerboard_colors(dark_bg, light_bg);
        assert!(dl.l - dd.l >= 0.06);
        assert!(dl.l <= 0.35 && dd.l >= 0.10);
    }

    #[test]
    fn inverse_stamp_delta_accounts_for_zoom_scale_and_rotation() {
        let (dx, dy) = inverse_stamp_delta(0.0, 20.0, 2.0, 1.0, 90.0);
        assert!((dx - 10.0).abs() < 1e-4);
        assert!(dy.abs() < 1e-4);

        let (dx, dy) = inverse_stamp_delta(30.0, -15.0, 1.5, 2.0, 0.0);
        assert!((dx - 10.0).abs() < 1e-4);
        assert!((dy - -5.0).abs() < 1e-4);
    }

    #[test]
    fn pinch_zoom_scales_relatively_and_stays_in_range() {
        assert!((pinch_zoom(1.0, 0.1) - 1.1).abs() < 1e-5);
        assert!((pinch_zoom(2.0, -0.5) - 1.0).abs() < 1e-5);
        assert_eq!(pinch_zoom(3.9, 0.5), 4.0);
        assert_eq!(pinch_zoom(0.3, -0.9), 0.25);
        assert_eq!(pinch_zoom(1.0, -5.0), 0.25);
    }

    #[test]
    fn snap_axis_snaps_within_threshold_and_ignores_distant_edges() {
        let siblings = vec![WorldBox {
            x: 100.0,
            y: 50.0,
            width: 80.0,
            height: 40.0,
        }];
        let (delta, guide) = snap_axis(103.0, 40.0, &siblings, true, 5.0);
        assert!((delta - -3.0).abs() < 1e-4);
        assert_eq!(guide, Some(100.0));

        let (delta_far, guide_far) = snap_axis(110.0, 40.0, &siblings, true, 5.0);
        assert_eq!(delta_far, 0.0);
        assert_eq!(guide_far, None);
    }

    #[test]
    fn align_operations_arrange_elements_accurately() {
        let mut e1 = WatermarkElement::new_text("el-1", "A");
        e1.x = 20.0;
        e1.y = 50.0;
        let mut e2 = WatermarkElement::new_text("el-2", "B");
        e2.x = 60.0;
        e2.y = 10.0;
        let mut elements = vec![e1, e2];
        let sizes = vec![(40.0, 20.0), (80.0, 40.0)];

        apply_align_op(&mut elements, &sizes, AlignOp::CenterH);
        assert_eq!(elements[0].x, 60.0);
        assert_eq!(elements[1].x, 40.0);

        apply_align_op(&mut elements, &sizes, AlignOp::Column);
        assert_eq!(elements[1].y, 10.0);
        assert_eq!(elements[0].y, 60.0);
    }

    #[test]
    fn pending_imports_are_removed_on_drop_but_saved_references_survive() {
        let directory = std::env::temp_dir().join(format!(
            "snapit-watermark-assets-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let pending = directory.join("pending.png");
        let existing = directory.join("existing.png");
        std::fs::write(&pending, b"pending").unwrap();
        std::fs::write(&existing, b"existing").unwrap();

        let mut watermark = snapit_store::WatermarkConfig::default();
        let mut preset = WatermarkPreset::default();
        preset.elements = vec![snapit_store::WatermarkElement::new_image(
            "saved-image",
            "existing.png",
            120.0,
        )];
        let mut saved_template = snapit_store::WatermarkTemplate::default();
        saved_template.name = "Saved".into();
        saved_template.preset = preset;
        watermark.templates.insert("saved".into(), saved_template);
        let mut assets = PendingWatermarkAssets::new(&watermark);
        assets.track(&directory, "pending.png".into());
        assets.track(&directory, "existing.png".into());
        drop(assets);
        assert!(!pending.exists());
        assert!(existing.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn saving_retains_referenced_imports_and_clears_pending_cleanup() {
        let directory = std::env::temp_dir().join(format!(
            "snapit-watermark-saved-assets-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        for name in ["used.png", "unused.png"] {
            std::fs::write(directory.join(name), b"image").unwrap();
        }
        let mut assets = PendingWatermarkAssets::new(&snapit_store::WatermarkConfig::default());
        assets.track(&directory, "used.png".into());
        assets.track(&directory, "unused.png".into());
        assets.cleanup(&BTreeSet::from(["used.png".into()]));
        assert!(directory.join("used.png").exists());
        assert!(!directory.join("unused.png").exists());
        assert!(assets.names.is_empty());
        drop(assets);
        assert!(directory.join("used.png").exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn successful_replacement_keeps_its_managed_image() {
        let directory = std::env::temp_dir().join(format!(
            "snapit-watermark-replaced-assets-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        for name in ["replacement.png", "undo.png", "redo.png", "slider.png"] {
            std::fs::write(directory.join(name), b"image").unwrap();
        }
        let mut assets = PendingWatermarkAssets::new(&snapit_store::WatermarkConfig::default());
        for name in ["replacement.png", "undo.png", "redo.png", "slider.png"] {
            assets.track(&directory, name.into());
        }
        let image_preset = |name: &str| {
            let mut preset = WatermarkPreset::default();
            preset.elements = vec![snapit_store::WatermarkElement::new_image(
                "image", name, 120.0,
            )];
            preset
        };
        let preset = image_preset("replacement.png");
        let undo = image_preset("undo.png");
        let redo = image_preset("redo.png");
        let slider = image_preset("slider.png");
        assets.cleanup(&watermark_retained_references(
            &preset,
            &[undo],
            &[redo],
            Some(&slider),
        ));
        drop(assets);
        for name in ["replacement.png", "undo.png", "redo.png", "slider.png"] {
            assert!(directory.join(name).exists());
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn starters_and_preset_validation_are_valid() {
        for starter in WatermarkStarter::ALL {
            assert!(preset_is_valid(&starter.preset()));
        }
        let mut preset = WatermarkPreset::default();
        preset.elements[0].text.content = "   ".into();
        assert!(!preset_is_valid(&preset));
    }
}
