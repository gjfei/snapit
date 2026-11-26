//! Shared floating toolbar building blocks for capture, scrolling-capture
//! annotation, scroll control bar, and long-image editor.

use std::rc::Rc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    Disableable as _, Icon, IconName, Selectable as _, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    color_picker::{ColorPicker, ColorPickerState},
    h_flex,
    slider::{Slider, SliderState},
    toolbar::Toolbar,
};
use gpui_kit::{
    App, ClickEvent, Div, ElementId, Entity, Hsla, ParentElement as _, SharedString, Styled as _,
    Window, div, prelude::FluentBuilder as _, px, rgb,
};

use crate::annotate::{self, Tool};

struct ToolDescriptor {
    tool: Tool,
    slug: &'static str,
    icon: LucideIcon,
    label_key: &'static str,
}

const ANNOTATION_TOOLS: [ToolDescriptor; 7] = [
    ToolDescriptor {
        tool: Tool::Rect,
        slug: "rect",
        icon: LucideIcon::Square,
        label_key: "overlay.tool.rect",
    },
    ToolDescriptor {
        tool: Tool::Arrow,
        slug: "arrow",
        icon: LucideIcon::ArrowUpRight,
        label_key: "overlay.tool.arrow",
    },
    ToolDescriptor {
        tool: Tool::Line,
        slug: "line",
        icon: LucideIcon::Minus,
        label_key: "overlay.tool.line",
    },
    ToolDescriptor {
        tool: Tool::Pen,
        slug: "pen",
        icon: LucideIcon::Pencil,
        label_key: "overlay.tool.pen",
    },
    ToolDescriptor {
        tool: Tool::Mosaic,
        slug: "mosaic",
        icon: LucideIcon::Grid3x3,
        label_key: "overlay.tool.mosaic",
    },
    ToolDescriptor {
        tool: Tool::Text,
        slug: "text",
        icon: LucideIcon::Type,
        label_key: "overlay.tool.text",
    },
    ToolDescriptor {
        tool: Tool::Step,
        slug: "step",
        icon: LucideIcon::ListOrdered,
        label_key: "overlay.tool.step",
    },
];

/// Localized tooltips and accessibility labels for the trailing export actions.
pub struct ExportActionLabels {
    pub ocr: String,
    pub pin: String,
    pub save: String,
    pub cancel: String,
    pub copy: String,
}

impl ExportActionLabels {
    /// Standard screenshot overlay labels.
    pub fn overlay() -> Self {
        Self {
            ocr: t!("overlay.ocr").to_string(),
            pin: t!("overlay.pin").to_string(),
            save: t!("overlay.save").to_string(),
            cancel: t!("overlay.cancel").to_string(),
            copy: t!("overlay.copy").to_string(),
        }
    }

    /// Scrolling-capture bar labels (copy/save/cancel carry scroll shortcut hints).
    pub fn scroll() -> Self {
        Self {
            ocr: t!("overlay.ocr").to_string(),
            pin: t!("overlay.pin").to_string(),
            save: t!("scroll.save_hint").to_string(),
            cancel: t!("scroll.cancel_hint").to_string(),
            copy: t!("scroll.copy_hint").to_string(),
        }
    }
}

/// Converts a GPUI `Hsla` color into an opaque `0xRRGGBB` annotation color.
pub fn color_to_rgb(color: Hsla) -> u32 {
    let rgba = color.to_rgb();
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u32;
    (channel(rgba.r) << 16) | (channel(rgba.g) << 8) | channel(rgba.b)
}

/// Standard floating toolbar card shell styled from theme popover tokens.
pub fn toolbar_card(theme: &Theme) -> Div {
    div()
        .bg(theme.popover)
        .text_color(theme.popover_foreground)
        .border_1()
        .border_color(theme.border)
        .rounded(theme.radius)
        .shadow_md()
}

/// Consistent vertical separator used between toolbar command groups.
pub fn toolbar_divider(theme: &Theme) -> Div {
    div().w(px(1.)).h(px(16.)).bg(theme.border).mx_0p5()
}

/// Consistent `.small().ghost()` icon button used across capture toolbars.
pub fn toolbar_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    label: String,
    selected: bool,
    disabled: bool,
) -> Button {
    Button::new(id)
        .small()
        .ghost()
        .icon(icon.into())
        .tooltip(label.clone())
        .accessibility_label(label)
        .selected(selected)
        .disabled(disabled)
}

/// Appends the 7 annotation tools plus Undo and Clear onto `toolbar`.
#[allow(clippy::too_many_arguments)]
pub fn append_annotation_tools(
    mut toolbar: Toolbar,
    id_prefix: &'static str,
    current: Option<Tool>,
    can_undo: bool,
    disabled: bool,
    theme: &Theme,
    on_select: impl Fn(Tool, &ClickEvent, &mut Window, &mut App) + 'static,
    on_undo: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_clear: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Toolbar {
    let on_select = Rc::new(on_select);
    for spec in ANNOTATION_TOOLS {
        let on_select = on_select.clone();
        let tool = spec.tool;
        let label = t!(spec.label_key).to_string();
        toolbar = toolbar.child(
            toolbar_button(
                SharedString::from(format!("{id_prefix}-{}", spec.slug)),
                spec.icon,
                label,
                current == Some(tool),
                disabled,
            )
            .on_click(move |event, window, cx| on_select(tool, event, window, cx)),
        );
    }
    toolbar
        .content(toolbar_divider(theme))
        .child(
            toolbar_button(
                SharedString::from(format!("{id_prefix}-undo")),
                LucideIcon::Undo2,
                t!("overlay.undo").to_string(),
                false,
                disabled || !can_undo,
            )
            .on_click(on_undo),
        )
        .child(
            toolbar_button(
                SharedString::from(format!("{id_prefix}-clear")),
                LucideIcon::Eraser,
                t!("overlay.clear").to_string(),
                false,
                disabled || !can_undo,
            )
            .on_click(on_clear),
        )
}

/// Appends the trailing OCR, Pin, Save, Cancel, and primary Copy actions.
#[allow(clippy::too_many_arguments)]
pub fn append_export_actions(
    toolbar: Toolbar,
    id_prefix: &'static str,
    labels: ExportActionLabels,
    export_disabled: bool,
    cancel_disabled: bool,
    on_ocr: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_pin: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_save: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Toolbar {
    toolbar
        .child(
            toolbar_button(
                SharedString::from(format!("{id_prefix}-ocr")),
                LucideIcon::ScanText,
                labels.ocr,
                false,
                export_disabled,
            )
            .on_click(on_ocr),
        )
        .child(
            toolbar_button(
                SharedString::from(format!("{id_prefix}-pin")),
                LucideIcon::Pin,
                labels.pin,
                false,
                export_disabled,
            )
            .on_click(on_pin),
        )
        .child(
            toolbar_button(
                SharedString::from(format!("{id_prefix}-save")),
                LucideIcon::Save,
                labels.save,
                false,
                export_disabled,
            )
            .on_click(on_save),
        )
        .child(
            toolbar_button(
                SharedString::from(format!("{id_prefix}-cancel")),
                IconName::Close,
                labels.cancel,
                false,
                cancel_disabled,
            )
            .on_click(on_cancel),
        )
        .child(
            Button::new(SharedString::from(format!("{id_prefix}-copy")))
                .small()
                .primary()
                .icon(IconName::Copy)
                .tooltip(labels.copy.clone())
                .accessibility_label(labels.copy)
                .disabled(export_disabled)
                .on_click(on_copy),
        )
}

/// Compact inline style strip matching the live scroll-annotation bar:
/// ColorPicker + 88 px Slider + value label + optional Delete button.
pub type DeleteHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

pub fn render_annotation_style_bar(
    id_prefix: &'static str,
    active_tool: Tool,
    size_value: f32,
    color_picker: &Entity<ColorPickerState>,
    size_slider: &Entity<SliderState>,
    on_delete: Option<DeleteHandler>,
    theme: &Theme,
) -> Div {
    let picker = ColorPicker::new(color_picker)
        .with_size(px(14.))
        .accessibility_label(t!("overlay.style").to_string())
        .featured_colors(
            annotate::COLORS
                .into_iter()
                .map(|color| Hsla::from(rgb(color)))
                .collect(),
        );
    let value_label = t!("overlay.size_value", value = size_value.round() as u32).to_string();
    let delete_label = t!("overlay.delete").to_string();

    h_flex()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .when(active_tool.uses_color(), |this| this.child(picker))
        .child(div().w(px(88.)).flex_none().child(Slider::new(size_slider)))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(value_label),
        )
        .when_some(on_delete, |this, on_delete| {
            this.child(
                Button::new(SharedString::from(format!("{id_prefix}-delete")))
                    .small()
                    .ghost()
                    .icon(LucideIcon::Trash)
                    .tooltip(delete_label.clone())
                    .accessibility_label(delete_label)
                    .on_click(move |event, window, cx| on_delete(event, window, cx)),
            )
        })
}
