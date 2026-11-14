# SnapIt — notes for coding agents

Screenshot app for macOS and Windows. UI is [gpui-kit](https://github.com/longbridge/gpui-kit) on Zed GPUI. Capture freezes every monitor, opens one borderless overlay per display, then copy / save / OCR / GIF / pin.

Do not bump `gpui-kit` or the Rust toolchain without an explicit request. Prefer extending existing crates over rewriting the MVP.

`vendor/xcap` is a git submodule (fork `gjfei/xcap`) added to the workspace `exclude` and consumed as a path dependency, so changes made there need their own commit inside the submodule plus an updated gitlink in this repo. Clone with `--recursive`; CI/release checkouts must use `submodules: recursive`.

User docs: `README.md` (Chinese) and `README.en.md` (English) mirror each other — update both together. `CLAUDE.md` only points here.

## Crate map

| Crate | Path | Role |
| --- | --- | --- |
| `snapit-capture` | `crates/capture/src/lib.rs` | `xcap` multi-monitor capture, monitor-unit geometry, crop, PNG/JPEG, window enumeration (`windows.rs`) |
| `snapit-recorder` | `crates/recorder/src/lib.rs` | Region GIF/MP4 (`vendor/xcap` live frames + `gif` / AVFoundation / Media Foundation; frame dedupe, duration cap, optional cursor) |
| `snapit-ocr` | `crates/ocr/src/lib.rs` | `OcrEngine` trait; Vision / ocrs / Windows.Media.Ocr |
| `snapit-store` | `crates/store/src/{config,history,paths,fs}.rs` | TOML settings, history index, `AppDirs` |
| `snapit` (bin) | `crates/app/src/main.rs` | gpui-kit shell |

App modules (`crates/app/src/`):

- `controller.rs` — global workflow: freeze → overlay → copy/save/OCR/GIF/pin
- `session.rs` — frozen frames, region selection (single, or hold `Shift` to collect several; releasing combines same-display regions into one selection), `SessionCommand` / `CaptureIntent`, window-snap hover/click (`capture.window_snap`)
- `annotate.rs` — overlay annotation model, GPUI canvas paint, tiny-skia bake
- `overlay.rs` — fullscreen selection UI + toolbar (`F` full display, `C` copies the pixel color, hold `Shift` to collect regions); the toolbar can be dragged and keeps that position; turns into the recording outline after the region is committed
- `recording_bar.rs` — in-recording stop bar (window equals the visible card; opens before the recorder's first frame and shows 准备中); draggable via the native window drag (`start_window_move` on macOS, `WM_NCLBUTTONDOWN` on Windows)
- `pin.rs` — always-on-top pin windows
- `tray.rs` — resident tray menu
- `settings.rs` / `settings_view.rs` — live `Config` + settings window
- `history_store.rs` / `history_view.rs` — history entity + UI
- `hotkey.rs` — global shortcut parsing / validation and OS registration (`global-hotkey`); pressed events are delivered by `events.rs`
- `events.rs` — one event bus: `set_event_handler` callbacks from `global-hotkey` / `tray-icon` (platform threads) plus forwarded launch requests all post `AppEvent`s into an unbounded channel, drained by one `cx.spawn` task on the GPUI main thread
- `i18n.rs` — UI locale resolution (`general.language`: `auto` / `zh-CN` / `en`)
- `ocr_view.rs` — small OCR results panel (opened after region OCR; not a launcher)
- `updater.rs` / `update_view.rs` — GitHub Releases check, download + SHA-256 verify, self-install, update window (`[update]` in `config.toml`)
- `appearance.rs` — theme + macOS Dock / Accessory (Dock icon only while a panel window is open)
- `clipboard.rs` — shared clipboard writes: image data (PNG / GIF) and file references (`FileCopy::File` vs the `PathText` fallback)
- `instance.rs` — single-instance file lock + launch-request forwarding (`snapit.lock` / `snapit.request`); the only remaining 50 ms poll, because a file has no notification mechanism
- `autostart.rs` — launch at login (`--background`; same tray-only as default launch)
- `assets.rs` — extra Lucide icons (`Crop`, `Pin`, `Images`, …)

OCR backends: `crates/ocr/src/vision.rs` (macOS), `ocrs_engine.rs`, `windows_ocr.rs` (Windows.Media.Ocr).

Recording pipeline: `snapit_capture::LiveRecorder` wraps `xcap::VideoRecorder` with `VideoRecorderOptions` (`vendor/xcap/src/video_recorder.rs`): region in monitor units, fps cap, a cursor flag, own-window exclusion, and frames stamped with `Duration` since the recorder was created. macOS uses ScreenCaptureKit (`SCStream` with a `sourceRect` and pixel dimensions from the monitor's scale factor, so no post-crop is needed) and excludes SnapIt's own windows by filtering the process's `SCWindow`s out of the `SCContentFilter`; this path needs macOS 13 because the framework is linked strongly. Windows crops in the DXGI sender thread and throttles there. `Recorder::start` blocks until the first live frame (5 s timeout) so the UI timer and the sink share an origin, then creates the GIF/MP4 sink on that frame's real dimensions. The recording outline re-uses the recorded display's overlay window (`OverlayView::enter_recording`); the bar and that window both call `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` on Windows.

App mark: `assets/snapit-icon.png` (1024), `assets/snapit-tray-template.png` (menu-bar template), `assets/AppIcon.icns`, `assets/snapit.ico` (Windows exe/window icon; `crates/app/build.rs` embeds it as resource 1, which gpui-pre-windows loads for every window and Explorer shows on the exe).

## Hard invariants

- **gpui-kit `=0.9.1`** in the workspace `Cargo.toml`. It pins GPUI (`gpui-pre` 0.3.12). Do not float the version.
- **Rust 1.99.1** via `rust-toolchain.toml` (edition 2024).
- **BGRA for `RenderImage`.** Captures are RGBA; `session::render_image` swaps R/B before GPUI. Reuse that helper; do not feed RGBA frames to `img()`.
- **Window coordinates.** Overlay/pin `WindowOptions` must set both `display_id` and `window_bounds` from GPUI `PlatformDisplay::bounds()` (overlay slots from `SessionSlot`). macOS bounds are per-display (origin often `0,0` on that screen). Windows bounds are the global virtual desktop. Do not place windows from raw `xcap` `MonitorInfo::{x,y}`.
- **Overlay pre-placement (macOS only).** The overlay windows are created while the freeze runs on a worker: `CaptureSession::new` starts with one `SessionSlot` per display and no frames, then `set_frames` matches captured monitors to slots and fills them. Pre-placed windows use `window_background: Transparent` and an empty frame, so they contribute nothing to the freeze. On macOS they are created with `show: false` and then ordered front at zero alpha (`overlay::pre_place` → `setAlphaValue: 0`, `ignoresMouseEvents`): GPUI warms the surface during the freeze, but AppKit's opaque default background never shows. `OverlayView::activate` restores alpha 1 only after the next rendered frame (`Window::on_next_frame`) — revealing an undrawn fullscreen layer flashes black — then makes the window key. A Windows window with `show: true` is on screen before its first transparent frame, so Windows opens overlays with the frames already set instead of pre-placing them.
- **xcap units.** `snapit_capture::Region` is in *monitor units* (points on macOS, physical pixels on Windows). Images are always physical pixels; use `Screenshot::pixels_per_unit` / `SessionFrame::to_region`.
- **`WindowKind::PopUp`** for overlays, recording bar, and pins (always-on-top, no titlebar chrome).
- **The recording region re-uses the capture overlay; the bar is its own plain `cx.open_window`.** Closing the overlay and opening a second frame window made the selection border blink away during the hand-off, so recording keeps the recorded display's overlay and turns it into the live indicator: `Controller::start_recording` takes that window out of `ActiveSelection` (closing the other displays' overlays), then `OverlayView::enter_recording` drops the frozen frame and toolbar and paints `recording_frame` — the same look as the selection stage, the viewport dimmed with `theme.overlay` outside the region plus a 1 px `theme.primary` border — and makes the window click-through (`ignoresMouseEvents` on macOS; `WS_EX_TRANSPARENT | WS_EX_NOACTIVATE` on Windows), and the recorder leaves it out of the frames (own-window exclusion in ScreenCaptureKit on macOS, `SetWindowDisplayAffinity` on Windows). The mask and border are drawn outside the captured bounds, so the file never contains them. `gpui_kit::open_window` wraps the view in `gpui_base::Root`, whose window-state plugin paints `theme.background` on the whole surface, so `enter_recording` overrides `Root`'s own background with `transparent_black` — that refinement is applied after the plugin styles; that is why the display-sized layer does not black out the screen. Recording starts only after the window has presented a frame with the live content (`Controller::after_presented_frame`, two `on_next_frame` steps because callbacks run before the frame is drawn); otherwise the first recorded frame would still show the frozen overlay. The recording bar is opened right away in a "starting" state (`ActiveRecording.started = false`, `recording.starting`), and the recorder joins the same state when its first live frame lands, so the bar does not wait for `Recorder::start`. Stopping during that window cancels: `stop_recording` clears the session, and `spawn_recorder` stops the recorder it was handed on a worker thread and deletes the empty file. The bar window is exactly `BAR_SIZE` with no transparent margin — a margin exposed the system window's own border around the card — and keeps using plain `cx.open_window`, because an opaque Root card would paint the corners.
- **`events::install()` must run before `Application::run`.** Both `global-hotkey` and `muda` keep their event handler in a `OnceLock` that their first `send()` initialises to `None`; after that, `set_event_handler` silently fails and events go to a channel nobody reads. Registering from `main()` before the hotkey manager and tray icon exist is the only safe ordering. Callbacks only push into the bus — GPUI state is `!Send` and must stay on the main thread. `muda` is used only by `tray-icon` here, so a global menu handler cannot steal GPUI's own menu events.
- **During a recording, a hotkey stops it and queues its own action** (`Controller::queued_capture`), which runs after encoding clears `busy`. `Controller::fail` clears the queue so actions never chain onto a failure.
- **`gpui_kit::open_window` via `cx.defer`** when the caller still holds `Controller` (or any entity the new view will `read`). `open_window` renders immediately. Overlays are opened after `CaptureSession` exists and the controller update that created it has ended; panel/pin/tray attach already defer.
- **`KeyBinding` rejects `secondary-`.** Use `crate::shortcut!("c")` (`cmd-` / `ctrl-`), never `secondary-c`.
- Keep `cargo check --workspace`, `cargo build`, and `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Config and data

`AppDirs` (`crates/store/src/paths.rs`) uses `directories::ProjectDirs::from("dev", "SnapIt", "SnapIt")`. `SNAPIT_HOME` puts config and data in one directory. Discover failure falls back to `$TMP/snapit`.

| File | macOS | Windows |
| --- | --- | --- |
| `config.toml` | `~/Library/Application Support/dev.SnapIt.SnapIt/config.toml` | `%APPDATA%\SnapIt\SnapIt\config\config.toml` |
| History | `…/dev.SnapIt.SnapIt/history/` | `%APPDATA%\SnapIt\SnapIt\data\history\` |

`[update]` holds `check` (background check on launch, default true) and `last_check` (Unix time; automatic checks are at most once per 24 h). It is written whenever a check starts, so a hand-edited file only needs `check`.

`[recording]` holds `fps` (1–30), `max_duration_secs` (0 = unlimited, max 600), `last_format` (`gif`/`mp4`) and `captures_cursor` (default false; macOS-only, since Windows DXGI never includes the pointer).

Corrupt TOML is moved to `config.toml.bak` and replaced with defaults. OCR models (non-macOS): `$SNAPIT_OCR_MODELS`, `models/` next to the binary, then `~/.snapit/models`.

UI strings come from rust-i18n: `crates/app/locales/{en,zh-CN}.yml` (flat dotted keys, `%{name}` placeholders, `_version: 1`, both files must keep the same keys). `#[macro_use] extern crate rust_i18n;` and `rust_i18n::i18n!("locales", fallback = "en");` must stay at the crate root (`main.rs`) because `t!` expands to `crate::_rust_i18n_t!`. `general.language` is `auto` (sys-locale; zh* → `zh-CN`, else `en`) or an explicit locale. Switching it at runtime re-renders windows, relabels the tray, and rebuilds app menus and open panel titles.

## Build, test, smoke

```bash
cargo check --workspace --all-targets
cargo build                 # binary: snapit
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace      # store persistence/prune, hotkeys, tray icon bytes
cargo run -p snapit                 # tray + hotkeys only (no launcher window)
cargo run -p snapit -- --capture    # also --settings --history --background
```

Examples: `snapit-capture` `capture_once`, `snapit-ocr` `ocr_image`, `snapit-recorder` `record_gif`. Launch is tray + hotkeys only (`QuitMode::Explicit`, macOS Accessory). A lock file in the data directory keeps one instance (`instance.rs`): a second launch without flags logs and exits; `--capture` / `--settings` / `--history` are written to `snapit.request`, polled by the owner every 50 ms and posted onto the event bus, so CLI actions are never dropped. Stale requests are removed when the lock is acquired. There is no home/launcher window; OCR opens a small results panel. macOS screen capture needs Screen Recording permission on the host Terminal/IDE.

`[profile.dev] opt-level = 1` (workspace crates; dependencies stay at 2) keeps `cargo run` interactive — the overlay repaints on every pointer move, and `-O0` makes that visibly slower. Do not drop it back to 0.

macOS `.app` (does not replace `cargo run -p snapit`):

```bash
./scripts/bundle-macos.sh                  # prefers an already-built debug binary
./scripts/bundle-macos.sh --bin path/to/snapit
./scripts/bundle-macos.sh --target aarch64-apple-darwin --out dist   # CI / release
```

Output: `target/macos/SnapIt.app` (or `--out dist` → `dist/SnapIt.app`). Bundle id `dev.SnapIt.SnapIt` (same as `AppDirs`). Ad-hoc signed (`codesign --sign - --force --deep`). Rebuilds change the code signature; Screen Recording may need to be re-granted.

macOS DMG follows SwitchHostsLite (not create-dmg): `scripts/package-macos.sh` writes `dist/SnapIt-<version>-macos-<arch>.{dmg,zip}` (`ditto` zip, `hdiutil` ULMO + `/Applications` symlink, retries on "Resource busy"). Developer ID / `notarytool` / `stapler` use the same secret names as SwitchHostsLite (`MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD`). Missing secrets keep the ad-hoc signature and still emit the DMG; do not fail and do not invent certificates.

Windows setup: `./scripts/bundle.sh windows|all` (`scripts/bundle-windows.sh`) compiles `scripts/windows/snapit.nsi` with NSIS (`makensis`, or `MAKENSIS=/path`). Output: `dist/SnapIt-<version>-windows-<arch>-setup.exe` (all users, `Program Files\SnapIt`; setup prompts to close a running SnapIt and reuses the last install directory) plus the staged `dist/windows/SnapIt/SnapIt.exe`. `--target` defaults to the host triple on Windows (the MSVC triple elsewhere); a missing Rust target, linker or NSIS prints `skip:` and exits 0 — do not install cross compilers from these scripts. `dist/` is gitignored.

Release packaging is `.github/workflows/release.yml`, matching SwitchHostsLite: `v*` tags or `workflow_dispatch` with that tag. It checks the tag against `[workspace.package] version`, installs the `rust-toolchain.toml` pin (1.99.1; do not float to stable), `rustup target add`, then a 4-target matrix (`macos-aarch64`, `macos-x86_64` cross-compiled on `macos-latest`, `windows-x86_64`, `windows-aarch64` cross-compiled on `windows-latest`). macOS: `scripts/bundle-macos.sh --target … --out dist` then `scripts/package-macos.sh --name SnapIt-<version>-<key>`. Windows: the job installs NSIS if the image lacks it, then `scripts/bundle-windows.sh --bin … --target … --setup-out out`. Artifacts: `SnapIt-<version>-macos-<arch>.{dmg,zip}`, `SnapIt-<version>-windows-<arch>-setup.exe`, `latest.json`, `SHA256SUMS.txt`. `scripts/release-manifest.py` writes the manifest; the macos zip and the Windows setup.exe are the updater packages, the DMG is release-only. A GitHub Release is created for the tag. macOS sign/notarize/staple only when the SwitchHostsLite secrets are present. `.github/workflows/ci.yml` runs `cargo test --locked` and clippy on macOS and Windows.

Updates follow SwitchHostsLite too: the app fetches `https://github.com/<repo>/releases/latest/download/latest.json` (the repo comes from `SNAPIT_UPDATE_REPO` at build time, set by the workflow, else `repository` in `Cargo.toml`). Auto-check runs 5 s after launch when `[update] check = true` (default) and the last check was over 24 h ago; finding a newer version opens the update window. `SNAPIT_UPDATE_MANIFEST_URL` is a debug-build override for a local server. macOS installs the `.app` in place (admin prompt when `/Applications` is not writable) and relaunches; Windows copies itself to `%TEMP%` as an update helper, waits for the app to exit, runs the setup with `/S` elevated and restarts. The installer kills a running SnapIt with `taskkill` (after a prompt when interactive, directly under `/S`), so do not add in-app state that is only flushed on a normal quit. Before tagging a stable release, run the ignored `simulate_update` test against a locally served manifest and `.app`: `SNAPIT_TEST_MANIFEST_URL=… SNAPIT_TEST_BUNDLE=… cargo test simulate_update -- --ignored`.

`QuitMode::Explicit`: closing settings / history / OCR panel / pin / overlay does not quit; tray **退出** or `⌘Q`/`Ctrl+Q` does. The Dock icon appears only while a panel (settings / history / OCR / permissions / update) is open; overlays, the recording bar, and pins stay Dock-less. Legacy `open_window_on_launch` and `show_dock_icon` in `config.toml` are ignored and stripped on load.

## Stubbed or limited

- **Window snap** (`crates/capture/src/windows.rs`): on-screen window rects are snapshotted at freeze time, front-to-back — macOS `CGWindowListCopyWindowInfo`, Windows `EnumWindows` + DWM extended frame bounds. Fullscreen apps can split one visual window into several platform windows (Chrome: tab strip, toolbar, content); `SessionFrame::window_at` (`session.rs`) merges touching same-process windows that line up on an axis and cover the display, so the wheel reaches the whole app. Accessibility element chains (`elements_at` in `snapit-capture`) run on the background executor via `ElementQuery` / `PendingHover`: the window highlight shows immediately and the element chain replaces it when the lookup lands, with a generation guard dropping stale results. Toggle: `capture.window_snap` (default on).

- **Windows.Media.Ocr** (`crates/ocr/src/windows_ocr.rs`) uses one recognizer language (`TryCreateFromLanguage` when the setting is supported, else user-profile languages). Needs an installed OCR language pack; no per-line confidence; `engine_with` falls back to ocrs if WinRT creation fails.
- **ocrs** is Latin-only; the OCR language setting applies to macOS Vision and Windows.Media.Ocr.
- **GIF pin** uses the PNG thumbnail (`HistoryStore::pin_source`), not a GIF decoder (`image` features are `png`+`jpeg` only).
- **Recording cursor and bar exclusion**: ScreenCaptureKit filters by window object, so the recorder drops every on-screen `SCWindow` owned by SnapIt's own process; the overlay and the bar are excluded even when the bar sits over the region (pins are left out too, since they are SnapIt windows). `open_recording_bar` still prefers placing the bar outside the selection, but the user may drag it anywhere. The dim mask and border sit outside the captured bounds, so they never show in the recording either. Windows excludes both windows with `WDA_EXCLUDEFROMCAPTURE`, but DXGI never includes the pointer, so `captures_cursor` only does something on macOS.
- **Annotations** live on the frozen overlay (`crates/app/src/annotate.rs`, wired through `session.rs` / `overlay.rs`): rectangle, arrow, pen, mosaic, text, numbered steps; GPUI canvas while editing; tiny-skia bake (real mosaic pixelation) on Copy / Save / Pin. Undo is Cmd/Ctrl+Z; OCR/GIF use the unbaked crop. Text bake uses a system font when one is found, else 5×7 digits.
- **Multiple regions** (`session.rs` `collecting`, Xnip-style): holding `Shift` enters collect mode — hovering lights the target, clicking or dragging adds a region, clicking an existing region removes it (danger border, bottom-left hint). Releasing `Shift` composes same-display regions into one selection (`Composed`): the toolbar and annotations work on the combined image, Copy / Save / Pin / OCR use it, resizing and GIF recording are disabled, and pressing `Shift` again restores the original regions for further edits. The modifier listener is registered per element via `div().on_modifiers_changed` because `Window::on_modifiers_changed` panics outside the paint phase. Regions on the same display keep their on-screen positions over a transparent canvas (`compose_at_positions`); a collection spanning displays keeps the separate-region behavior and is stitched top-to-bottom (`stitch_vertical`) without annotations.
- **Tray** `MenuEvent` and `GlobalHotKeyEvent` arrive through callbacks registered in `events::install()` (no polling). Create the icon on the GPUI main thread after launch (`tray::init` defers).
- Unbundled binary process name is **`snapit`** (`cargo run -p snapit`); system notifications are disabled outside an app bundle. Local macOS bundle: `scripts/bundle-macos.sh` → `target/macos/SnapIt.app` (`dev.SnapIt.SnapIt`, ad-hoc signed); distribution DMG/zip: `scripts/package-macos.sh`. Windows setup: `scripts/bundle.sh` (NSIS, all users). No `.msi`.

## How to extend

**New global hotkey**

1. Field on `snapit_store::Hotkeys` (`crates/store/src/config.rs`).
2. Variant on `HotkeyAction` (`hotkey.rs`): `ALL`, `label`, `binding`, `set_binding`. Settings fields come from `ALL`.
3. Branch in `Controller::on_hotkey`. Syntax is `global-hotkey` (`Alt+Shift+A`); empty string disables.

**New selection toolbar action**

1. `SessionCommand` in `session.rs`.
2. `Controller::handle_command` in `controller.rs`.
3. Button + `actions!` in `overlay.rs`; bump `TOOLBAR_WIDTH`. Optional `CaptureIntent` if it should run as soon as the region is committed.

**New UI string**

1. Add the key to both `crates/app/locales/en.yml` and `zh-CN.yml` (same key set).
2. Use `t!("group.key")` in code; it returns `Cow<str>`, so call `.to_string()` for `SharedString` / gpui `IntoElement` parameters.

**New OCR backend**

1. `impl OcrEngine` under `crates/ocr/src/`.
2. Select it in `engine_with` (`crates/ocr/src/lib.rs`). Changing OCR settings calls `Controller::reset_ocr_engine`.
