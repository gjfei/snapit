//! Browse past captures: re-copy, pin, reveal, delete, clear.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Local;
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, ImageFormat,
    InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, Window, div, img,
    prelude::FluentBuilder as _, px,
};
use snapit_store::{EntryKind, HistoryEntry};

use crate::{
    appearance, clipboard,
    controller::Controller,
    history_store::{HistoryStore, format_bytes},
    image_editor, pin,
    settings::AppSettings,
    settings_view::SettingsPage,
};

/// Which entries the list shows. Screenshots are PNGs; recordings are GIF / APNG / MP4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HistoryFilter {
    #[default]
    All,
    Screenshot,
    Recording,
}

impl HistoryFilter {
    const ALL: [Self; 3] = [Self::All, Self::Screenshot, Self::Recording];

    fn matches(self, kind: EntryKind) -> bool {
        match self {
            Self::All => true,
            Self::Screenshot => kind == EntryKind::Png,
            Self::Recording => kind.is_media(),
        }
    }

    fn label(self) -> String {
        match self {
            Self::All => t!("history.filter.all").to_string(),
            Self::Screenshot => t!("history.filter.screenshot").to_string(),
            Self::Recording => t!("history.filter.recording").to_string(),
        }
    }
}

pub struct HistoryView {
    store: Entity<HistoryStore>,
    filter: HistoryFilter,
    cached_generation: Option<u64>,
    cached_entries: Arc<Vec<HistoryEntry>>,
    copy_serial: u64,
    _subscriptions: Vec<Subscription>,
}

impl HistoryView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = HistoryStore::global(cx);
        Self {
            _subscriptions: vec![
                cx.observe(&store, |_, _, cx| cx.notify()),
                cx.observe_global::<AppSettings>(|_, cx| cx.notify()),
                appearance::follow_system_theme(window),
            ],
            store,
            filter: HistoryFilter::default(),
            cached_generation: None,
            cached_entries: Arc::new(Vec::new()),
            copy_serial: 0,
        }
    }

    fn confirm_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let store = store.clone();
            dialog
                .title(t!("history.clear.title").to_string())
                .description(t!("history.clear.desc").to_string())
                .confirm()
                .ok_text(t!("history.clear.ok").to_string())
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("history.cancel").to_string())
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, cx| store.clear(cx));
                    true
                })
        });
    }

    fn render_header(&self, count: usize, bytes: u64, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let config = &AppSettings::get(cx).history;
        let summary = if !config.enabled {
            t!("history.summary.disabled", count = count).to_string()
        } else if config.max_items == 0 {
            // Zero keeps everything, so quoting a numeric cap would be a lie.
            t!(
                "history.summary.unlimited",
                count = count,
                size = format_bytes(bytes)
            )
            .to_string()
        } else {
            t!(
                "history.summary",
                count = count,
                size = format_bytes(bytes),
                items = config.max_items
            )
            .to_string()
        };
        let dir = store.dir().to_path_buf();
        // Clearing wipes every entry, not just the ones the filter shows.
        let is_empty = store.len() == 0;

        h_flex()
            .justify_between()
            .gap_3()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("history.title").to_string()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("reveal-history")
                            .ghost()
                            .small()
                            .icon(IconName::FolderOpen)
                            .label(t!("history.open_folder").to_string())
                            .on_click(move |_, _, cx| cx.reveal_path(&dir)),
                    )
                    .child(
                        Button::new("history-settings")
                            .ghost()
                            .small()
                            .icon(IconName::Settings)
                            .tooltip(t!("history.settings").to_string())
                            .accessibility_label(t!("history.settings").to_string())
                            .on_click(|_, _, cx| {
                                Controller::global(cx).update(cx, |controller, cx| {
                                    controller.open_settings_page(SettingsPage::History, cx)
                                })
                            }),
                    )
                    .child(
                        Button::new("clear-history")
                            .danger()
                            .small()
                            .icon(IconName::Delete)
                            .label(t!("history.clear.ok").to_string())
                            .disabled(is_empty)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.confirm_clear(window, cx)),
                            ),
                    ),
            )
    }

    fn render_filter_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.filter;
        h_flex().gap_1().children(HistoryFilter::ALL.map(|filter| {
            let selected = current == filter;
            Button::new(SharedString::from(format!("history-filter-{filter:?}")))
                .small()
                .label(filter.label())
                .with_variant(if selected {
                    ButtonVariant::Secondary
                } else {
                    ButtonVariant::Ghost
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.filter = filter;
                    cx.notify();
                }))
        }))
    }

    fn copy_image(&mut self, path: PathBuf, kind: EntryKind, cx: &mut Context<Self>) {
        let format = match kind {
            EntryKind::Png | EntryKind::Apng => ImageFormat::Png,
            EntryKind::Gif => ImageFormat::Gif,
            EntryKind::Mp4 => return,
        };
        self.copy_serial = self.copy_serial.wrapping_add(1);
        let serial = self.copy_serial;
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_spawn({
                    let path = path.clone();
                    async move { clipboard::read_image_file(&path, format) }
                })
                .await;
            view.update(cx, |this, cx| {
                if this.copy_serial != serial {
                    return;
                }
                match result {
                    Ok(image) => {
                        clipboard::write_image(image, cx);
                        Controller::global(cx).update(cx, |controller, cx| {
                            controller.inform(t!("history.copied").to_string(), cx)
                        });
                    }
                    Err(err) => Controller::global(cx).update(cx, |controller, cx| {
                        controller.inform(t!("history.copy_failed", error = err).to_string(), cx)
                    }),
                }
            });
        })
        .detach();
    }

    fn render_entry(&self, entry: &HistoryEntry, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let store = self.store.read(cx);
        let file = store.file_path(entry);
        let thumbnail = store.thumbnail_path(entry);
        let created = entry
            .created_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let details = format!(
            "{} × {} · {}",
            entry.width,
            entry.height,
            format_bytes(entry.bytes)
        );
        let kind = entry.kind;
        let ocr_preview = entry
            .ocr_text
            .as_deref()
            .map(|text| text.lines().next().unwrap_or_default().to_string());
        let id = SharedString::from(entry.id.clone());
        let hover = theme.list_hover;
        let (pin_path, pin_scale) = store.pin_source(entry);
        let crop_path = file.clone();
        let crop_scale = entry.scale_factor;
        let crop_hdr = entry.hdr;
        // Clicking the thumbnail or the details opens the original file in the
        // system's default app. The buttons sit outside those two regions, so
        // they never trigger an open (GPUI dispatches a parent's click before
        // its children's, which makes stop_propagation unreliable here).
        let open_file = file.clone();
        let open_details = file.clone();
        h_flex()
            .id(SharedString::from(format!("history-{id}")))
            .gap_3()
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .hover(move |this| this.bg(hover))
            .child(
                div()
                    .id(SharedString::from(format!("{id}-thumb")))
                    .flex_none()
                    .w(px(128.))
                    .h(px(80.))
                    .rounded(theme.radius)
                    .overflow_hidden()
                    .bg(theme.muted)
                    .cursor_pointer()
                    .on_click(move |_, _, _| open_with_system(&open_file))
                    .child(img(thumbnail).size_full().object_fit(ObjectFit::Contain)),
            )
            .child(
                v_flex()
                    .id(SharedString::from(format!("{id}-details")))
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(move |_, _, _| open_with_system(&open_details))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded(theme.radius)
                                    .bg(theme.secondary)
                                    .text_xs()
                                    .child(match kind {
                                        EntryKind::Png => "PNG",
                                        EntryKind::Gif => "GIF",
                                        EntryKind::Apng => "APNG",
                                        EntryKind::Mp4 => "MP4",
                                    }),
                            )
                            .child(div().text_sm().child(created)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(details),
                    )
                    .when_some(ocr_preview, |this, preview| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(preview),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .when(kind == EntryKind::Png, |this| {
                        this.child(
                            Button::new(SharedString::from(format!("{id}-crop")))
                                .ghost()
                                .small()
                                .icon(LucideIcon::Crop)
                                .tooltip(t!("history.crop").to_string())
                                .accessibility_label(t!("history.crop").to_string())
                                .on_click(move |_, window, cx| {
                                    let display_id = window.display(cx).map(|d| d.id());
                                    image_editor::open_file(
                                        crop_path.clone(),
                                        crop_scale,
                                        crop_hdr,
                                        display_id,
                                        cx,
                                    );
                                }),
                        )
                    })
                    .when(kind != EntryKind::Mp4, |this| {
                        this.child(
                            Button::new(SharedString::from(format!("{id}-copy")))
                                .ghost()
                                .small()
                                .icon(IconName::Copy)
                                .tooltip(t!("history.copy_image").to_string())
                                .on_click({
                                    let file = file.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.copy_image(file.clone(), kind, cx)
                                    })
                                }),
                        )
                    })
                    .when(kind == EntryKind::Mp4, |this| {
                        this.child(
                            Button::new(SharedString::from(format!("{id}-copy-file")))
                                .ghost()
                                .small()
                                .icon(IconName::Copy)
                                .tooltip(t!("history.copy_file").to_string())
                                .on_click({
                                    let file = file.clone();
                                    move |_, window, cx| copy_file_to_clipboard(&file, window, cx)
                                }),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("{id}-pin")))
                            .ghost()
                            .small()
                            .icon(LucideIcon::Pin)
                            .tooltip(t!("history.pin").to_string())
                            .on_click(move |_, _, cx| {
                                pin::open_file(pin_path.clone(), pin_scale, cx)
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id}-reveal")))
                            .ghost()
                            .small()
                            .icon(IconName::FolderOpen)
                            .tooltip(t!("history.reveal").to_string())
                            .on_click(move |_, _, cx| cx.reveal_path(&file)),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id}-delete")))
                            .ghost()
                            .small()
                            .icon(IconName::Delete)
                            .tooltip(t!("history.delete").to_string())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.store.update(cx, |store, cx| store.remove(&id, cx))
                            })),
                    ),
            )
            .into_any_element()
    }
}

/// Hand the original file to whatever the OS registered for it: Preview.app,
/// the default GIF viewer, the default video player and so on.
fn open_with_system(path: &Path) {
    #[cfg(target_os = "macos")]
    if let Err(err) = std::process::Command::new("open").arg(path).spawn() {
        log::error!("cannot open {}: {err}", path.display());
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt as _;
        use std::ptr;
        use windows_sys::Win32::{
            Foundation::GetLastError,
            UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        };

        let file = path
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect::<Vec<_>>();
        // ShellExecuteW treats the path as data; cmd.exe would parse shell metacharacters.
        let opened = unsafe {
            ShellExecuteW(
                ptr::null_mut(),
                ptr::null(),
                file.as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            )
        } as usize;
        if opened <= 32 {
            log::error!(
                "cannot open {}: ShellExecuteW returned {opened}, error {}",
                path.display(),
                unsafe { GetLastError() }
            );
        }
    }
}

fn copy_file_to_clipboard(path: &Path, window: &mut Window, cx: &mut App) {
    let message = match clipboard::copy_file_reference(path, cx) {
        Ok(clipboard::FileCopy::File) => t!("history.copied_file").to_string(),
        Ok(clipboard::FileCopy::PathText) => t!("history.copied_path").to_string(),
        Err(err) => {
            log::error!("copy file: {err:#}");
            t!("history.copy_file_failed", error = err).to_string()
        }
    };
    window.push_notification(message, cx);
}

impl Render for HistoryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.filter;
        let (count, bytes, error) = {
            let store = self.store.read(cx);
            if self.cached_generation != Some(store.generation()) {
                self.cached_entries = Arc::new(store.entries().cloned().collect());
                self.cached_generation = Some(store.generation());
            }
            let entries = self
                .cached_entries
                .iter()
                .filter(|entry| filter.matches(entry.kind));
            let count = entries.clone().count();
            let bytes = entries.map(|entry| entry.bytes).sum();
            let error = store.error().map(str::to_string);
            (count, bytes, error)
        };
        let theme = cx.theme();
        let (background, foreground, muted, danger) = (
            theme.background,
            theme.foreground,
            theme.muted_foreground,
            theme.danger,
        );

        let body = if count == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child(if filter == HistoryFilter::All {
                    t!("history.empty").to_string()
                } else {
                    t!("history.empty.filtered").to_string()
                })
                .into_any_element()
        } else {
            v_flex()
                // Key the scroll container by filter so switching tabs starts
                // at the top instead of keeping an offset from a longer list.
                .id(SharedString::from(format!("history-list-{filter:?}")))
                .flex_1()
                .min_h_0()
                .gap_2()
                .children(
                    self.cached_entries
                        .iter()
                        .filter(|entry| filter.matches(entry.kind))
                        .map(|entry| self.render_entry(entry, cx)),
                )
                .overflow_y_scrollbar()
                .into_any_element()
        };

        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(background)
            .text_color(foreground)
            .child(self.render_header(count, bytes, cx))
            .child(self.render_filter_bar(cx))
            .when_some(error, |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(danger)
                        .child(t!("history.unavailable", error = error).to_string()),
                )
            })
            .child(body)
    }
}
