//! Update checks and self-updates from this app's GitHub releases.
//!
//! Every release carries a `latest.json` manifest naming one package per
//! platform with its size and SHA-256 (see `.github/workflows/release.yml`).
//! An update downloads that package and verifies it, then waits for the user:
//! Windows installs the `-setup.exe` over the running app and restarts, macOS
//! saves the disk image and leaves the installation to them. Pre-releases are
//! checked too, so a beta build can see the next beta.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "windows")]
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(any(target_os = "windows", test))]
use anyhow::Context as _;
use anyhow::Result;
use gpui_kit::{App, Global};
use rust_i18n::t;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::controller::{Controller, Panel};
use crate::settings::AppSettings;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const STARTUP_DELAY: Duration = Duration::from_secs(5);
const CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;
/// How often download progress is redrawn.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_MANIFEST_BYTES: u64 = 1 << 20;
const MAX_NOTES_CHARS: usize = 4000;
#[cfg(target_os = "windows")]
const PACKAGE_BINARY: &str = "snapit.exe";

// -- errors --------------------------------------------------------------------

/// An error whose message is a locale key, optionally followed by OS detail.
#[derive(Debug)]
struct UpdateError {
    key: &'static str,
    detail: Option<String>,
}

impl UpdateError {
    fn new(key: &'static str) -> Self {
        Self { key, detail: None }
    }

    fn detail(mut self, detail: impl fmt::Display) -> Self {
        let detail = detail.to_string();
        self.detail = (!detail.trim().is_empty()).then_some(detail);
        self
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(t!(self.key).as_ref())?;
        if let Some(detail) = &self.detail {
            write!(f, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for UpdateError {}

trait Localize<T> {
    fn localize(self, key: &'static str) -> Result<T>;
}

impl<T, E: fmt::Display> Localize<T> for Result<T, E> {
    fn localize(self, key: &'static str) -> Result<T> {
        self.map_err(|err| UpdateError::new(key).detail(err).into())
    }
}

fn error_text(err: &anyhow::Error) -> String {
    match err.downcast_ref::<UpdateError>() {
        Some(err) => err.to_string(),
        None => format!("{err:#}"),
    }
}

// -- update source -------------------------------------------------------------

/// Where releases are looked up.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Source {
    manifest_url: String,
    page_url: String,
}

/// `owner/repo` from a GitHub URL, `None` for anything else or for the
/// `OWNER` placeholder that ships in Cargo.toml.
fn parse_repo(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('/');
    let value = value.strip_suffix(".git").unwrap_or(value);
    let path = ["https://github.com/", "http://github.com/", "github.com/"]
        .iter()
        .find_map(|prefix| value.strip_prefix(prefix))
        .unwrap_or(value);
    let mut parts = path.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let valid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    (parts.next().is_none() && valid(owner) && valid(name) && !owner.eq_ignore_ascii_case("OWNER"))
        .then(|| format!("{owner}/{name}"))
}

/// The repository named at build time by `SNAPIT_UPDATE_REPO` or, failing
/// that, by `repository` in Cargo.toml.
fn repo() -> Option<String> {
    let configured = option_env!("SNAPIT_UPDATE_REPO").filter(|s| !s.trim().is_empty());
    parse_repo(configured.unwrap_or(env!("CARGO_PKG_REPOSITORY")))
}

/// Public web page of the source repository, when one is configured.
pub fn repo_url() -> Option<String> {
    repo().map(|repo| format!("https://github.com/{repo}"))
}

/// Lets a debug build test updates against a local server.
fn override_source() -> Option<Source> {
    #[cfg(debug_assertions)]
    if let Ok(manifest_url) = std::env::var("SNAPIT_UPDATE_MANIFEST_URL") {
        let page_url = repo().map_or_else(|| manifest_url.clone(), |repo| release_page(&repo));
        return Some(Source {
            manifest_url,
            page_url,
        });
    }
    None
}

/// Whether this build knows where to look for updates.
pub fn is_configured() -> bool {
    override_source().is_some() || repo().is_some()
}

/// Resolve the newest release's manifest, pre-releases included:
/// `releases/latest` skips them, so beta builds would never see an update.
/// The API is rate-limited for anonymous callers, so a failure falls back to
/// github.com's own release pages.
fn resolve_source() -> Result<Source> {
    if let Some(source) = override_source() {
        return Ok(source);
    }
    let Some(repo) = repo() else {
        return Err(UpdateError::new("update.not_configured").into());
    };
    api_source(&repo).or_else(|_| web_source(&repo))
}

fn api_source(repo: &str) -> Result<Source> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=20");
    let text = get_text(&url)?;
    let entries: Vec<ReleaseEntry> =
        serde_json::from_str(&text).localize("update.error.releases")?;
    newest_release(&entries).ok_or_else(|| UpdateError::new("update.error.releases").into())
}

/// The Atom feed and a release's asset fragment are ordinary web pages, so
/// they do not share the API's anonymous rate limit.
fn web_source(repo: &str) -> Result<Source> {
    let feed = get_text(&format!("https://github.com/{repo}/releases.atom"))?;
    let tags = atom_tags(&feed);
    let tag = newest_tag(&tags).ok_or_else(|| UpdateError::new("update.error.releases"))?;
    let html = get_text(&format!(
        "https://github.com/{repo}/releases/expanded_assets/{tag}"
    ))?;
    let manifest_url = expanded_manifest_url(&html, tag)
        .ok_or_else(|| UpdateError::new("update.error.releases"))?;
    Ok(Source {
        manifest_url,
        page_url: format!("https://github.com/{repo}/releases/tag/{tag}"),
    })
}

fn get_text(url: &str) -> Result<String> {
    let cancel = http::CancelToken::default();
    let mut body = http::get(url, MANIFEST_TIMEOUT, MAX_MANIFEST_BYTES, &cancel)
        .localize("update.error.network")?;
    let mut text = String::new();
    body.read_to_string(&mut text)
        .localize("update.error.network")?;
    body.finish().localize("update.error.network")?;
    Ok(text)
}

/// One entry of the GitHub release list.
#[derive(Debug, Deserialize)]
struct ReleaseEntry {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<EntryAsset>,
}

#[derive(Debug, Deserialize)]
struct EntryAsset {
    name: String,
    browser_download_url: String,
}

/// The highest-versioned published release carrying a `latest.json`; drafts,
/// unparsable tags and releases without the manifest are skipped.
fn newest_release(entries: &[ReleaseEntry]) -> Option<Source> {
    let mut best: Option<(&ReleaseEntry, &str)> = None;
    for entry in entries {
        if entry.draft || parse_version(&entry.tag_name).is_none() {
            continue;
        }
        let Some(asset) = entry
            .assets
            .iter()
            .find(|asset| asset.name == "latest.json")
        else {
            continue;
        };
        if best.is_none_or(|(best, _)| is_newer(&entry.tag_name, &best.tag_name)) {
            best = Some((entry, &asset.browser_download_url));
        }
    }
    best.map(|(entry, url)| Source {
        manifest_url: url.to_string(),
        page_url: entry.html_url.clone(),
    })
}

/// Release tags linked by the Atom feed, in feed order.
fn atom_tags(feed: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut rest = feed;
    while let Some(at) = rest.find("/releases/tag/") {
        rest = &rest[at + "/releases/tag/".len()..];
        let end = rest.find('"').unwrap_or(rest.len());
        let tag = &rest[..end];
        if parse_version(tag).is_some() && !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_string());
        }
        rest = &rest[end..];
    }
    tags
}

fn newest_tag(tags: &[String]) -> Option<&String> {
    tags.iter()
        .reduce(|best, tag| if is_newer(tag, best) { tag } else { best })
}

/// The `latest.json` link inside a release's expanded-assets fragment, whose
/// hrefs are root-relative.
fn expanded_manifest_url(html: &str, tag: &str) -> Option<String> {
    let needle = format!("/releases/download/{tag}/latest.json");
    let at = html.find(&needle)?;
    let start = html[..at].rfind('"')? + 1;
    let url = &html[start..at + needle.len()];
    if url.starts_with('/') {
        Some(format!("https://github.com{url}"))
    } else if url.starts_with("https://github.com/") {
        Some(url.to_string())
    } else {
        None
    }
}

#[cfg(debug_assertions)]
fn release_page(repo: &str) -> String {
    format!("https://github.com/{repo}/releases/latest")
}

/// Key of this platform's archive in the manifest, such as `macos-aarch64`.
fn platform_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

// -- versions and manifest -----------------------------------------------------

/// `major.minor.patch` and the pre-release part of a version such as
/// `v1.2.3-beta.1+build`.
fn parse_version(version: &str) -> Option<([u64; 3], Option<&str>)> {
    let version = version.trim();
    let version = version.strip_prefix('v').unwrap_or(version);
    let version = version.split('+').next()?;
    let (core, pre) = match version.split_once('-') {
        // Semver identifiers only: the pre-release part ends up in a temp
        // directory name, so it must not carry path separators.
        Some((core, pre))
            if !pre.is_empty()
                && pre.split('.').all(|id| {
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                }) =>
        {
            (core, Some(pre))
        }
        Some(_) => return None,
        None => (version, None),
    };
    let mut numbers = core.split('.').map(|n| n.parse::<u64>().ok());
    let parsed = [numbers.next()??, numbers.next()??, numbers.next()??];
    numbers.next().is_none().then_some((parsed, pre))
}

/// Semver precedence of two pre-release parts.
fn compare_pre(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let mut a = a.split('.');
    let mut b = b.split('.');
    loop {
        let ordering = match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => match (x.parse::<u64>(), y.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => x.cmp(y),
            },
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
}

/// Whether `candidate` is a later version than `current`; unparsable
/// versions never are.
fn is_newer(candidate: &str, current: &str) -> bool {
    use std::cmp::Ordering;
    let (Some((a, a_pre)), Some((b, b_pre))) = (parse_version(candidate), parse_version(current))
    else {
        return false;
    };
    match a.cmp(&b) {
        Ordering::Equal => match (a_pre, b_pre) {
            (None, Some(_)) => true,
            (Some(x), Some(y)) => compare_pre(x, y) == Ordering::Greater,
            _ => false,
        },
        ordering => ordering == Ordering::Greater,
    }
}

#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    assets: HashMap<String, Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct Asset {
    pub(crate) url: String,
    pub(crate) sha256: String,
    pub(crate) size: u64,
}

/// A release newer than the running app.
#[derive(Clone, Debug)]
pub(crate) struct Release {
    pub(crate) version: String,
    pub(crate) notes: String,
    /// The archive for this platform, `None` when the release has none.
    pub(crate) asset: Option<Asset>,
}

fn parse_manifest(text: &str) -> Result<Manifest> {
    let manifest: Manifest = serde_json::from_str(text).localize("update.error.manifest")?;
    if parse_version(&manifest.version).is_none() {
        return Err(UpdateError::new("update.error.manifest")
            .detail(&manifest.version)
            .into());
    }
    Ok(manifest)
}

/// The usable archive for `key`: an http(s) URL and a well-formed checksum.
fn select_asset(manifest: &Manifest, key: &str) -> Option<Asset> {
    let asset = manifest.assets.get(key)?;
    let checksum_ok =
        asset.sha256.len() == 64 && asset.sha256.chars().all(|c| c.is_ascii_hexdigit());
    let url_ok = asset.url.starts_with("https://") || asset.url.starts_with("http://");
    (checksum_ok && url_ok && asset.size > 0).then(|| asset.clone())
}

/// macOS installs by hand from a disk image; older manifests only carry the
/// zip, which a user can still unpack themselves.
fn update_asset(manifest: &Manifest, key: &str) -> Option<Asset> {
    #[cfg(target_os = "macos")]
    if let Some(dmg) = select_asset(manifest, &format!("{key}-dmg")) {
        return Some(dmg);
    }
    select_asset(manifest, key)
}

/// Whether the version carries a pre-release part such as `-beta.1`.
fn is_pre(version: &str) -> bool {
    parse_version(version).is_some_and(|(_, pre)| pre.is_some())
}

/// A stable build moving to a pre-release confirms once before downloading;
/// every other combination downloads directly.
pub(crate) fn needs_confirm(current: &str, target: &str) -> bool {
    parse_version(current).is_some_and(|(_, pre)| pre.is_none()) && is_pre(target)
}

/// The release in `manifest` when it is newer than `current`.
fn evaluate(manifest: Manifest, current: &str, key: &str) -> Option<Release> {
    if !is_newer(&manifest.version, current) {
        return None;
    }
    Some(Release {
        asset: update_asset(&manifest, key),
        version: manifest.version.trim().trim_start_matches('v').to_string(),
        notes: truncate_notes(manifest.notes.trim(), MAX_NOTES_CHARS),
    })
}

fn truncate_notes(notes: &str, max_chars: usize) -> String {
    match notes.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", notes[..end].trim_end()),
        None => notes.to_string(),
    }
}

fn due(last_check: Option<i64>, now: i64) -> bool {
    last_check.is_none_or(|last| now - last >= CHECK_INTERVAL_SECS || last > now)
}

// -- network -------------------------------------------------------------------

const USER_AGENT: &str = concat!("SnapIt/", env!("CARGO_PKG_VERSION"));

#[cfg(target_os = "windows")]
fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// HTTP(S) GET through the curl that ships with macOS and Windows 10+, which
/// keeps a TLS stack out of the binary.
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod http {
    use std::io::{self, Read};
    use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct CancelState {
        cancelled: std::sync::atomic::AtomicBool,
        child: Mutex<Option<Child>>,
    }

    /// Cancellation owns the curl process handle, so it interrupts a blocked
    /// read instead of merely asking the download loop to stop later.
    #[derive(Clone, Default)]
    pub struct CancelToken {
        state: Arc<CancelState>,
    }

    impl CancelToken {
        pub fn is_cancelled(&self) -> bool {
            self.state
                .cancelled
                .load(std::sync::atomic::Ordering::Acquire)
        }

        pub fn check(&self) -> io::Result<()> {
            if self.is_cancelled() {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(())
            }
        }

        pub fn cancel(&self) {
            self.state
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
            if let Ok(mut child) = self.state.child.lock()
                && let Some(child) = child.as_mut()
            {
                let _ = child.kill();
            }
        }
    }

    /// A response body streamed from curl's stdout.
    pub struct Body {
        child: Arc<Mutex<Option<Child>>>,
        stdout: ChildStdout,
        stderr: ChildStderr,
        received: u64,
        max_len: u64,
        cancel: CancelToken,
    }

    #[cfg(target_os = "macos")]
    fn curl() -> Command {
        Command::new("/usr/bin/curl")
    }

    #[cfg(target_os = "windows")]
    fn curl() -> Command {
        let system = std::env::var_os("SystemRoot")
            .map(|root| {
                std::path::PathBuf::from(root)
                    .join("System32")
                    .join("curl.exe")
            })
            .filter(|curl| curl.is_file());
        super::hidden_command(system.as_deref().unwrap_or("curl.exe".as_ref()))
    }

    /// Start a GET of `url` that fails on an HTTP error status, after
    /// `timeout` or when the body is longer than `max_len`.
    pub fn get(
        url: &str,
        timeout: Duration,
        max_len: u64,
        cancel: &CancelToken,
    ) -> io::Result<Body> {
        cancel.check()?;
        let mut child = curl()
            // Must come first: ignores the user's .curlrc.
            .arg("--disable")
            .args([
                "--silent",
                "--show-error",
                "--fail",
                "--location",
                "--max-redirs",
                "10",
            ])
            .args(["--proto", "=http,https", "--proto-redir", "=http,https"])
            .arg("--connect-timeout")
            .arg(super::CONNECT_TIMEOUT.as_secs().to_string())
            .arg("--max-time")
            .arg(timeout.as_secs().to_string())
            .arg("--max-filesize")
            .arg(max_len.to_string())
            .arg("--user-agent")
            .arg(super::USER_AGENT)
            .arg("--url")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| io::Error::new(err.kind(), format!("curl: {err}")))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let child = Arc::new(Mutex::new(Some(child)));
        if cancel.is_cancelled()
            && let Some(child) = child.lock().unwrap().as_mut()
        {
            let _ = child.kill();
        }
        Ok(Body {
            child: child.clone(),
            stdout,
            stderr,
            received: 0,
            max_len,
            cancel: cancel.clone(),
        })
    }

    impl Read for Body {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.cancel.check()?;
            let n = self.stdout.read(buf)?;
            self.cancel.check()?;
            self.received += n as u64;
            if self.received > self.max_len {
                return Err(io::Error::other(format!(
                    "response longer than {} bytes",
                    self.max_len
                )));
            }
            Ok(n)
        }
    }

    impl Body {
        /// Once the body was read to the end: curl's error if the transfer failed.
        pub fn finish(mut self) -> io::Result<()> {
            self.cancel.check()?;
            // Keep the child in the shared slot while reading stderr: cancellation
            // must still be able to kill curl if it blocks at this boundary.
            let mut stderr = String::new();
            let _ = self.stderr.read_to_string(&mut stderr);
            self.cancel.check()?;
            let mut child = self.child.lock().unwrap().take().expect("curl child");
            let status = child.wait()?;
            self.cancel.check()?;
            if status.success() {
                return Ok(());
            }
            Err(io::Error::other(format!(
                "curl: {status} {}",
                stderr.trim()
            )))
        }
    }

    impl Drop for Body {
        fn drop(&mut self) {
            if let Ok(mut child) = self.child.lock()
                && let Some(mut child) = child.take()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn output_command() -> Command {
            #[cfg(target_os = "macos")]
            {
                let mut command = Command::new("/usr/bin/printf");
                command.arg("abc");
                command
            }
            #[cfg(target_os = "windows")]
            {
                let mut command = super::super::hidden_command("cmd.exe");
                command.args(["/D", "/C", "echo", "abc"]);
                command
            }
        }

        #[test]
        fn finish_rechecks_cancellation_after_eof() {
            let mut child = output_command()
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let cancel = CancelToken::default();
            let mut body = Body {
                child: Arc::new(Mutex::new(Some(child))),
                stdout,
                stderr,
                received: 0,
                max_len: 1024,
                cancel: cancel.clone(),
            };
            let mut bytes = Vec::new();
            body.read_to_end(&mut bytes).unwrap();
            assert!(bytes.windows(3).any(|window| window == b"abc"));
            cancel.cancel();
            assert_eq!(
                body.finish().unwrap_err().kind(),
                io::ErrorKind::Interrupted
            );
        }
    }
}

fn fetch_manifest(url: &str) -> Result<Manifest> {
    let cancel = http::CancelToken::default();
    let mut body = http::get(url, MANIFEST_TIMEOUT, MAX_MANIFEST_BYTES, &cancel)
        .localize("update.error.network")?;
    let mut text = String::new();
    body.read_to_string(&mut text)
        .localize("update.error.network")?;
    body.finish().localize("update.error.network")?;
    parse_manifest(&text)
}

/// The download was cancelled by the user; not shown as an error.
#[derive(Debug)]
struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Check a downloaded archive against the manifest.
fn verify(size: u64, sha256: &str, asset: &Asset) -> Result<()> {
    verify_values(size, sha256, asset.size, &asset.sha256)
}

fn verify_values(size: u64, sha256: &str, expected_size: u64, expected_sha256: &str) -> Result<()> {
    if size != expected_size {
        let detail = format!("size {size}, expected {expected_size}");
        return Err(UpdateError::new("update.error.checksum")
            .detail(detail)
            .into());
    }
    if !sha256.eq_ignore_ascii_case(expected_sha256) {
        let detail = format!("sha256 {sha256}, expected {expected_sha256}");
        return Err(UpdateError::new("update.error.checksum")
            .detail(detail)
            .into());
    }
    Ok(())
}

#[cfg(test)]
fn verify_file(path: &Path, expected_size: u64, expected_sha256: &str) -> Result<()> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    verify_open_file(file, expected_size, expected_sha256).map(|_| ())
}

#[cfg(any(target_os = "windows", test))]
fn verify_open_file(
    mut file: fs::File,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<fs::File> {
    let size = file.metadata()?.len();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    verify_values(
        size,
        &hex(&hasher.finalize()),
        expected_size,
        expected_sha256,
    )?;
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0))?;
    Ok(file)
}

#[cfg(target_os = "windows")]
fn verify_and_lock(path: &Path, expected_size: u64, expected_sha256: &str) -> Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    // Keep the verified object open without write/delete sharing until UAC has
    // created the installer process; reopening by path after hashing is a TOCTOU.
    let file = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x0000_0001)
        .open(path)
        .with_context(|| format!("failed to lock {}", path.display()))?;
    verify_open_file(file, expected_size, expected_sha256)
}

fn check_cancel(cancel: &http::CancelToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Cancelled.into())
    } else {
        Ok(())
    }
}

/// Download `asset` to `dest`, counting bytes in `progress`, then verify it.
/// `dest` must be a newly reserved staging path owned by this job.
fn download(
    asset: &Asset,
    dest: &Path,
    progress: &AtomicU64,
    cancel: &http::CancelToken,
) -> Result<()> {
    let mut created = false;
    let result = (|| -> Result<()> {
        check_cancel(cancel)?;
        let mut body = http::get(&asset.url, DOWNLOAD_TIMEOUT, asset.size, cancel)
            .localize("update.error.download")?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .localize("update.error.download")?;
        created = true;
        let mut hasher = Sha256::new();
        let mut buf = vec![0; 64 * 1024];
        let mut size = 0;
        loop {
            check_cancel(cancel)?;
            let n = body.read(&mut buf).map_err(|err| -> anyhow::Error {
                if cancel.is_cancelled() {
                    Cancelled.into()
                } else {
                    UpdateError::new("update.error.download").detail(err).into()
                }
            })?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])
                .localize("update.error.download")?;
            size += n as u64;
            progress.store(size, Ordering::Relaxed);
        }
        // EOF alone is not success: curl may still be reporting a truncated
        // transfer, and cancellation can land while finish is waiting for it.
        check_cancel(cancel)?;
        body.finish().map_err(|err| -> anyhow::Error {
            if cancel.is_cancelled() {
                Cancelled.into()
            } else {
                UpdateError::new("update.error.download").detail(err).into()
            }
        })?;
        file.sync_all().localize("update.error.download")?;
        drop(file);
        check_cancel(cancel)?;
        verify(size, &hex(&hasher.finalize()), asset)?;
        check_cancel(cancel)
    })();
    if result.is_err() && created {
        let _ = fs::remove_file(dest);
    }
    result
}

/// File name of a download URL, ignoring any query.
fn url_file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let name = path.rsplit('/').next().unwrap_or_default();
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if safe {
        name.to_string()
    } else {
        "update-package".to_string()
    }
}

// -- installing ----------------------------------------------------------------

#[cfg(target_os = "windows")]
fn run(command: &mut Command, key: &'static str) -> Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|err| UpdateError::new(key).detail(format!("{program}: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = format!("{program}: {} {}", output.status, stderr.trim());
        return Err(UpdateError::new(key).detail(detail).into());
    }
    Ok(())
}

/// Whether files can be created in `dir`.
#[cfg(any(windows, test))]
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".snapit-write-test-{}", std::process::id()));
    let writable = fs::File::create(&probe).is_ok();
    let _ = fs::remove_file(&probe);
    writable
}

/// The first entry in `dir`, or one level below it, matching `pred`.
#[cfg(any(windows, test))]
fn find_entry(dir: &Path, pred: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    if let Some(found) = entries.iter().find(|p| pred(p)) {
        return Some(found.clone());
    }
    entries.iter().filter(|p| p.is_dir()).find_map(|sub| {
        fs::read_dir(sub)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| pred(p))
    })
}

/// `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn unique_staging(path: &Path) -> PathBuf {
    let nonce = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);
    with_suffix(path, &format!(".{}-{nonce}.part", std::process::id()))
}

/// Claim a public download name without replacing a package another instance
/// may already have published. Both paths are siblings, so a hard link publishes
/// the fully written staging file atomically and fails if the target exists.
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn claim_download_target(staging: &Path, target: &Path) -> std::io::Result<()> {
    fs::hard_link(staging, target)?;
    if let Err(err) = fs::remove_file(staging)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!(
            "cannot remove published download staging {}: {err}",
            staging.display()
        );
    }
    Ok(())
}

struct Downloaded {
    /// Verified file. On macOS this is still the unique staging path; the UI
    /// thread gives it the public Downloads name only after accepting the job.
    archive: PathBuf,
    target: PathBuf,
    work: Option<PathBuf>,
}

/// Put `new` in place of the binary `exe`, which may be running. On Windows
/// the running exe cannot be overwritten but can be renamed out of the way.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn replace_binary(new: &Path, exe: &Path) -> Result<()> {
    let staged = with_suffix(exe, ".new");
    let _ = fs::remove_file(&staged);
    fs::copy(new, &staged).localize("update.error.install")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
            .localize("update.error.install")?;
    }
    #[cfg(windows)]
    {
        let old = with_suffix(exe, ".old");
        let _ = fs::remove_file(&old);
        if let Err(err) = fs::rename(exe, &old) {
            let _ = fs::remove_file(&staged);
            return Err(err).localize("update.error.install");
        }
        if let Err(err) = fs::rename(&staged, exe) {
            let _ = fs::rename(&old, exe);
            let _ = fs::remove_file(&staged);
            return Err(err).localize("update.error.install");
        }
    }
    #[cfg(not(windows))]
    if let Err(err) = fs::rename(&staged, exe) {
        let _ = fs::remove_file(&staged);
        return Err(err).localize("update.error.install");
    }
    Ok(())
}

/// Remove what a previous update left next to the binary.
fn remove_leftovers() {
    if let Ok(exe) = std::env::current_exe() {
        for suffix in [".old", ".new"] {
            let _ = fs::remove_file(with_suffix(&exe, suffix));
        }
    }
}

/// Where a downloaded disk image goes: the user's Downloads folder, or the
/// temp directory when it is unavailable.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn download_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Downloads"))
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// macOS installs by hand: the verified disk image is revealed in Finder and
/// the user drags the app into Applications.
#[cfg(target_os = "macos")]
fn reveal(path: &Path) {
    if let Err(err) = Command::new("/usr/bin/open").arg("-R").arg(path).spawn() {
        log::warn!("cannot reveal {}: {err}", path.display());
    }
}

#[cfg(target_os = "windows")]
fn install(
    archive: &Path,
    work: &Path,
    _version: &str,
    expected_sha256: &str,
    expected_size: u64,
) -> Result<Command> {
    let exe = std::env::current_exe().localize("update.error.install")?;
    if archive
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return install_from_setup(archive, work, &exe, expected_sha256, expected_size);
    }
    let dir = exe
        .parent()
        .ok_or_else(|| UpdateError::new("update.error.install"))?;
    if !is_writable(dir) {
        return Err(UpdateError::new("update.error.not_writable")
            .detail(dir.display())
            .into());
    }
    let staging = work.join("extract");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).localize("update.error.extract")?;
    extract(archive, &staging)?;
    let is_binary = |p: &Path| p.is_file() && p.file_name().is_some_and(|n| n == PACKAGE_BINARY);
    let new =
        find_entry(&staging, &is_binary).ok_or_else(|| UpdateError::new("update.error.package"))?;
    replace_binary(&new, &exe)?;
    let _ = fs::remove_dir_all(work);
    Ok(start_command(&exe))
}

/// Argument that marks the detached helper started by `install_from_setup`:
/// `--install-update <setup> <exe> <pid> <sha256> <size>`.
#[cfg(any(windows, test))]
const UPDATE_INSTALL_ARG: &str = "--install-update";

/// The command line of the update helper, besides the program itself.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum InstallerArgs {
    /// An ordinary launch.
    Normal,
    /// The helper was started with a malformed command line.
    Invalid,
    /// Install `setup` once process `pid` has exited, then start `exe`.
    Install {
        setup: PathBuf,
        exe: PathBuf,
        pid: u32,
        sha256: String,
        size: u64,
    },
}

/// Only exactly the flag and its five values count, so an ordinary launch
/// never installs an update.
#[cfg(any(windows, test))]
fn installer_args<I, S>(args: I) -> InstallerArgs
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let args: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    if args
        .get(1)
        .is_none_or(|arg| arg.as_os_str() != std::ffi::OsStr::new(UPDATE_INSTALL_ARG))
    {
        return InstallerArgs::Normal;
    }
    match args.as_slice() {
        [_, _, setup, exe, pid, sha256, size] => {
            let Some(pid) = pid.to_str().and_then(|pid| pid.parse().ok()) else {
                return InstallerArgs::Invalid;
            };
            let Some(sha256) = sha256.to_str() else {
                return InstallerArgs::Invalid;
            };
            if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return InstallerArgs::Invalid;
            }
            let Some(size) = size.to_str().and_then(|size| size.parse().ok()) else {
                return InstallerArgs::Invalid;
            };
            InstallerArgs::Install {
                setup: PathBuf::from(setup),
                exe: PathBuf::from(exe),
                pid,
                sha256: sha256.to_string(),
                size,
            }
        }
        _ => InstallerArgs::Invalid,
    }
}

/// A copy of this executable that installs `setup` after the app exits. The
/// installed exe cannot be overwritten while it runs, so the helper must not be
/// the installed image; it waits for `pid` to exit first.
#[cfg(target_os = "windows")]
fn install_from_setup(
    setup: &Path,
    work: &Path,
    exe: &Path,
    expected_sha256: &str,
    expected_size: u64,
) -> Result<Command> {
    let helper = work.join("snapit-update-helper.exe");
    let _ = fs::remove_file(&helper);
    fs::copy(exe, &helper).localize("update.error.install")?;
    let mut command = start_command(&helper);
    command
        .arg(UPDATE_INSTALL_ARG)
        .arg(setup)
        .arg(exe)
        .arg(std::process::id().to_string())
        .arg(expected_sha256)
        .arg(expected_size.to_string());
    Ok(command)
}

/// When this process is the update helper started by `install_from_setup`, wait
/// for the app to exit, run the installer elevated, and start the app again. An
/// ordinary launch returns immediately.
#[cfg(target_os = "windows")]
pub fn run_update_installer_helper() {
    match installer_args(std::env::args_os()) {
        InstallerArgs::Normal => {}
        InstallerArgs::Invalid => {
            eprintln!("update helper: expected {UPDATE_INSTALL_ARG} <setup> <exe> <pid>");
            std::process::exit(1);
        }
        InstallerArgs::Install {
            setup,
            exe,
            pid,
            sha256,
            size,
        } => {
            let helper = match std::env::current_exe().and_then(fs::canonicalize) {
                Ok(helper) => helper,
                Err(err) => {
                    eprintln!("update helper: {err}");
                    std::process::exit(1);
                }
            };
            let setup = match fs::canonicalize(&setup) {
                Ok(setup) => setup,
                Err(err) => {
                    eprintln!("update helper: cannot resolve installer: {err}");
                    std::process::exit(1);
                }
            };
            if helper
                .file_name()
                .is_none_or(|name| name != "snapit-update-helper.exe")
                || helper.parent() != setup.parent()
                || setup
                    .extension()
                    .is_none_or(|ext| !ext.eq_ignore_ascii_case("exe"))
            {
                eprintln!("update helper: package is outside the authorised work directory");
                std::process::exit(1);
            }
            wait_for_exit(pid);
            let package = match verify_and_lock(&setup, size, &sha256) {
                Ok(package) => package,
                Err(err) => {
                    eprintln!("update helper: package verification failed: {err:#}");
                    std::process::exit(1);
                }
            };
            let installed =
                run_elevated_process(&setup, std::ffi::OsStr::new("/S"), "update.error.install");
            drop(package);
            if let Err(err) = &installed {
                eprintln!("update helper: {err:#}");
            }
            // The installer is a single file; its work directory holds only the
            // running helper now, so it is removed on a later startup.
            let _ = fs::remove_file(&setup);
            if let Err(err) = start_command(&exe).spawn() {
                eprintln!("update helper: restart failed: {err}");
            }
            std::process::exit(if installed.is_ok() { 0 } else { 1 });
        }
    }
}

/// Run `program` with `params` elevated through the shell's UAC prompt and
/// wait for it, returning its exit code.
#[cfg(target_os = "windows")]
fn run_elevated_process(
    program: &Path,
    params: &std::ffi::OsStr,
    exit_key: &'static str,
) -> Result<u32> {
    use std::ffi::OsStr;
    use std::mem;
    use std::os::windows::ffi::OsStrExt as _;
    use std::ptr;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError, WAIT_OBJECT_0},
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
        },
        UI::{
            Shell::{
                SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
                ShellExecuteExW,
            },
            WindowsAndMessaging::SW_HIDE,
        },
    };

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain([0]).collect()
    }

    let verb = wide(OsStr::new("runas"));
    let file = wide(program.as_os_str());
    let params = wide(params);

    // ShellExecuteExW needs an STA thread. A fresh one keeps the UI thread's
    // COM state untouched.
    std::thread::scope(|scope| {
        scope
            .spawn(move || unsafe {
                let hr = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
                let com = hr >= 0;
                let mut info: SHELLEXECUTEINFOW = mem::zeroed();
                info.cbSize = mem::size_of::<SHELLEXECUTEINFOW>() as u32;
                info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
                info.lpVerb = verb.as_ptr();
                info.lpFile = file.as_ptr();
                info.lpParameters = params.as_ptr();
                info.nShow = SW_HIDE;

                if ShellExecuteExW(&mut info) == 0 {
                    let err = GetLastError();
                    if com {
                        CoUninitialize();
                    }
                    return Err(if err == ERROR_CANCELLED {
                        UpdateError::new("update.error.auth_cancelled").into()
                    } else {
                        UpdateError::new("update.error.launch")
                            .detail(format!("ShellExecuteExW: error {err}"))
                            .into()
                    });
                }
                let process = info.hProcess;
                if process.is_null() {
                    if com {
                        CoUninitialize();
                    }
                    return Err(UpdateError::new("update.error.launch")
                        .detail("no process handle")
                        .into());
                }
                let wait = WaitForSingleObject(process, INFINITE);
                let mut code = 0u32;
                let exit_known =
                    wait == WAIT_OBJECT_0 && GetExitCodeProcess(process, &mut code) != 0;
                CloseHandle(process);
                if com {
                    CoUninitialize();
                }
                if !exit_known {
                    return Err(UpdateError::new("update.error.launch")
                        .detail(format!("WaitForSingleObject: {wait}"))
                        .into());
                }
                if code == 0 {
                    Ok(0)
                } else {
                    Err(UpdateError::new(exit_key)
                        .detail(format!("elevated helper exit code {code}"))
                        .into())
                }
            })
            .join()
            .map_err(|_| UpdateError::new("update.error.launch"))?
    })
}

/// Wait until `pid` exits. A process that is already gone, or that cannot be
/// opened, is treated as exited.
#[cfg(target_os = "windows")]
fn wait_for_exit(pid: u32) {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !process.is_null() {
            WaitForSingleObject(process, INFINITE);
            CloseHandle(process);
        }
    }
}

/// Unzip with the bsdtar that ships with Windows 10+, else PowerShell.
#[cfg(target_os = "windows")]
fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let tar = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("tar.exe"))
        .filter(|tar| tar.is_file());
    if let Some(tar) = tar
        && run(
            hidden_command(tar)
                .arg("-xf")
                .arg(archive)
                .arg("-C")
                .arg(dest),
            "update.error.extract",
        )
        .is_ok()
    {
        return Ok(());
    }
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "''"));
    let script = format!(
        "Expand-Archive -LiteralPath {} -DestinationPath {} -Force",
        quote(archive),
        quote(dest)
    );
    run(
        hidden_command("powershell").args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script.as_str(),
        ]),
        "update.error.extract",
    )
}

#[cfg(target_os = "windows")]
fn start_command(exe: &Path) -> Command {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut command = Command::new(exe);
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    command
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn install(_: &Path, _: &Path, _: &str) -> Result<Command> {
    Err(UpdateError::new("update.error.install")
        .detail(std::env::consts::OS)
        .into())
}

/// A fresh scratch directory for one update. The pid + nonce name and
/// create-new owner marker make ownership explicit; no directory is reused.
#[cfg(any(not(target_os = "macos"), test))]
fn work_dir(version: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    loop {
        let nonce = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);
        match create_work_dir_in(&base, version, std::process::id(), nonce) {
            Err(err)
                if err
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|err| err.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                continue;
            }
            result => return result,
        }
    }
}

const WORK_OWNER_FILE: &str = ".snapit-owner";
static NEXT_NONCE: AtomicU64 = AtomicU64::new(1);
static OWNED_WORK_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[cfg(any(not(target_os = "macos"), test))]
fn create_work_dir_in(base: &Path, version: &str, pid: u32, nonce: u64) -> Result<PathBuf> {
    let dir = base.join(format!("snapit-update-{version}-{pid}-{nonce}"));
    fs::create_dir(&dir).localize("update.error.download")?;
    let owner = format!("{pid} {nonce}\n");
    let reserved = (|| -> Result<()> {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(WORK_OWNER_FILE))?
            .write_all(owner.as_bytes())?;
        Ok(())
    })();
    if let Err(err) = reserved {
        let _ = fs::remove_dir_all(&dir);
        return Err(err).localize("update.error.download");
    }
    if let Ok(mut owned) = OWNED_WORK_DIRS.lock() {
        owned.push(dir.clone());
    }
    Ok(dir)
}

fn work_owner(path: &Path) -> Option<(u32, u64)> {
    let owner = fs::read_to_string(path.join(WORK_OWNER_FILE)).ok()?;
    let mut fields = owner.split_whitespace();
    let pid = fields.next()?.parse().ok()?;
    let nonce = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some((pid, nonce))
}

fn work_dir_pid(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    // `rsplitn` yields the suffix first; parse from the right without letting
    // hyphens in a semver pre-release confuse the pid/nonce fields.
    let mut parts = name.rsplitn(3, '-');
    let _nonce = parts.next()?.parse::<u64>().ok()?;
    let pid = parts.next()?.parse::<u32>().ok()?;
    let prefix = parts.next()?;
    (prefix.starts_with("snapit-update-") && prefix.len() > "snapit-update-".len()).then_some(pid)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> Option<bool> {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    if pid == 0 || pid > i32::MAX as u32 {
        return Some(false);
    }
    // SAFETY: kill(pid, 0) only probes process existence and sends no signal.
    if unsafe { kill(pid as i32, 0) } == 0 {
        return Some(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(3) => Some(false), // ESRCH
        _ => None,              // EPERM and unknown errors are not proof of death.
    }
}

#[cfg(windows)]
fn process_alive(pid: u32) -> Option<bool> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    // SAFETY: the returned handle is closed on every path.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return (std::io::Error::last_os_error().raw_os_error()
                == Some(ERROR_INVALID_PARAMETER as i32))
            .then_some(false);
        }
        let mut code = 0;
        let known = GetExitCodeProcess(process, &mut code) != 0;
        CloseHandle(process);
        known.then_some(code == STILL_ACTIVE as u32)
    }
}

/// Remove only directories owned by this process or whose owner pid is
/// provably gone. A live peer, an unreadable marker, or an uncertain process
/// query is left untouched.
fn remove_stale_update_dirs_in(base: &Path) {
    let Ok(entries) = base.read_dir() else {
        return;
    };
    let owned = OWNED_WORK_DIRS
        .lock()
        .map(|dirs| dirs.clone())
        .unwrap_or_default();
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(pid) = work_dir_pid(&path) else {
            continue;
        };
        let Some((owner_pid, _)) = work_owner(&path) else {
            continue;
        };
        if owner_pid != pid {
            continue;
        }
        let ours = owned.contains(&path);
        let stale = !ours && process_alive(pid) == Some(false);
        if ours || stale {
            if let Err(err) = fs::remove_dir_all(&path) {
                log::warn!(
                    "cannot remove stale update directory {}: {err}",
                    path.display()
                );
            } else if let Ok(mut owned) = OWNED_WORK_DIRS.lock() {
                owned.retain(|dir| dir != &path);
            }
        }
    }
}

fn remove_stale_update_dirs() {
    remove_stale_update_dirs_in(&std::env::temp_dir());
}

// -- state ---------------------------------------------------------------------

#[derive(Default)]
pub struct UpdateState {
    checking: bool,
    /// A newer release found by the last check.
    release: Option<Release>,
    error: Option<String>,
    /// Whether a check has finished since launch.
    checked: bool,
    job: Option<Job>,
    source: Option<Source>,
}

impl Global for UpdateState {}

pub(crate) struct Job {
    id: u64,
    downloaded: Arc<AtomicU64>,
    pub(crate) total: u64,
    cancel: http::CancelToken,
    pub(crate) phase: Phase,
    /// Verified package on disk: the disk image on macOS, the downloaded
    /// installer or archive on Windows. `None` until the download finishes.
    archive: Option<PathBuf>,
    /// Scratch directory a Windows install needs while it runs.
    work: Option<PathBuf>,
    #[cfg(target_os = "windows")]
    expected_sha256: String,
    #[cfg(target_os = "windows")]
    expected_size: u64,
}

pub(crate) enum Phase {
    Downloading,
    /// Downloaded and verified, waiting for the user's click.
    Ready,
    /// Windows only: the installer helper is running.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Installing,
    Failed(String),
}

impl Job {
    /// Where the verified package landed, for the window's "ready" line.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn path(&self) -> Option<String> {
        self.archive.as_ref().map(|path| path.display().to_string())
    }
}

impl UpdateState {
    pub fn checking(&self) -> bool {
        self.checking
    }

    pub fn checked(&self) -> bool {
        self.checked
    }

    pub fn release(&self) -> Option<&Release> {
        self.release.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(crate) fn job(&self) -> Option<&Job> {
        self.job.as_ref()
    }

    /// Version of the job's package, for label text and install paths.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn job_version(&self) -> String {
        self.release
            .as_ref()
            .map_or_else(String::new, |release| release.version.clone())
    }

    pub(crate) fn page_url(&self) -> Option<&str> {
        self.source.as_ref().map(|source| source.page_url.as_str())
    }

    pub(crate) fn downloaded(&self) -> u64 {
        self.job
            .as_ref()
            .map_or(0, |job| job.downloaded.load(Ordering::Relaxed))
    }

    pub(crate) fn busy(&self) -> bool {
        matches!(&self.job, Some(job) if !matches!(job.phase, Phase::Failed(_)))
    }

    /// A worker may publish Ready only for the still-current, uncancelled job.
    fn can_publish_ready(&self, id: u64, cancel: &http::CancelToken) -> bool {
        !cancel.is_cancelled()
            && self
                .job
                .as_ref()
                .is_some_and(|job| job.id == id && matches!(job.phase, Phase::Downloading))
    }
}

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

fn next_job_id() -> u64 {
    NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn state(cx: &App) -> &UpdateState {
    cx.global::<UpdateState>()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn open_panel(cx: &mut App) {
    Controller::global(cx).update(cx, |controller, cx| {
        controller.open_panel(Panel::Update, cx)
    });
}

/// Clean up after a previous update and schedule the automatic check.
pub fn init(cx: &mut App) {
    // Finish startup cleanup before any UI action can create a new job. Doing
    // this after the startup delay could otherwise delete our own live job.
    remove_leftovers();
    remove_stale_update_dirs();
    cx.set_global(UpdateState::default());
    cx.spawn(async move |cx| {
        cx.background_executor().timer(STARTUP_DELAY).await;
        cx.update(|cx| {
            let config = AppSettings::get(cx);
            if config.update.check && due(config.update.last_check, now()) {
                check(false, cx);
            }
        });
    })
    .detach();
}

/// Look for a newer release. A manual check opens the update window; an
/// automatic one opens it only when a newer release exists.
pub fn check(manual: bool, cx: &mut App) {
    if !is_configured() {
        if manual {
            let state = cx.global_mut::<UpdateState>();
            state.checked = true;
            state.error = Some(t!("update.not_configured").to_string());
            cx.refresh_windows();
            open_panel(cx);
        }
        return;
    }
    let state = cx.global_mut::<UpdateState>();
    if state.checking || state.busy() {
        if manual {
            open_panel(cx);
        }
        return;
    }
    // A new check supersedes the outcome of the previous update attempt.
    if matches!(&state.job, Some(job) if matches!(job.phase, Phase::Failed(_))) {
        state.job = None;
    }
    state.checking = true;
    state.checked = false;
    state.release = None;
    state.error = None;
    state.source = None;
    AppSettings::update(cx, |config| config.update.last_check = Some(now()));
    cx.refresh_windows();
    if manual {
        open_panel(cx);
    }

    cx.spawn(async move |cx| {
        // Finding the newest release costs a couple of requests (the API, then
        // github.com's pages), so it runs off the UI thread with the download.
        let result = cx
            .background_executor()
            .spawn(async move {
                let source = resolve_source()?;
                let manifest = fetch_manifest(&source.manifest_url)?;
                anyhow::Ok((source, manifest))
            })
            .await;
        cx.update(|cx| {
            let found = match result {
                Ok((source, manifest)) => {
                    cx.global_mut::<UpdateState>().source = Some(source);
                    evaluate(manifest, CURRENT_VERSION, &platform_key())
                }
                Err(err) => {
                    log::warn!("update check failed: {err:#}");
                    cx.global_mut::<UpdateState>().checking = false;
                    cx.global_mut::<UpdateState>().checked = true;
                    cx.global_mut::<UpdateState>().error = Some(error_text(&err));
                    cx.refresh_windows();
                    if manual {
                        open_panel(cx);
                    }
                    return;
                }
            };
            let state = cx.global_mut::<UpdateState>();
            state.checking = false;
            state.checked = true;
            state.release = found;
            state.error = None;
            let available = state.release.is_some();
            cx.refresh_windows();
            // Never steal focus from a capture overlay or a running recording;
            // the release stays available from the menu and tray afterwards.
            if available && !manual && !Controller::global(cx).read(cx).is_busy() {
                open_panel(cx);
            }
        });
    })
    .detach();
}

/// Stop a running download, or forget a finished/failed update and its package.
pub fn dismiss_job(cx: &mut App) {
    let state = cx.global_mut::<UpdateState>();
    if let Some(job) = state.job.as_ref()
        && matches!(job.phase, Phase::Downloading)
    {
        // Kill curl first, then remove the job immediately so `busy()` is
        // released even if a platform is slow to reap the child process.
        job.cancel.cancel();
    }
    if state
        .job
        .as_ref()
        .is_some_and(|job| matches!(job.phase, Phase::Installing))
    {
        cx.refresh_windows();
        return;
    }
    if let Some(job) = state.job.take() {
        if let Some(archive) = job.archive {
            let _ = fs::remove_file(archive);
        }
        if let Some(work) = job.work {
            let _ = fs::remove_dir_all(work);
        }
    }
    cx.refresh_windows();
}

fn set_phase(id: u64, phase: Phase, cx: &mut App) -> bool {
    let changed = if let Some(job) = &mut cx.global_mut::<UpdateState>().job {
        if job.id != id {
            false
        } else {
            job.phase = phase;
            true
        }
    } else {
        false
    };
    if changed {
        cx.refresh_windows();
    }
    changed
}

fn fail(id: u64, err: anyhow::Error, cx: &mut App) {
    if err.is::<Cancelled>() {
        if cx
            .global::<UpdateState>()
            .job
            .as_ref()
            .is_some_and(|job| job.id == id)
        {
            cx.global_mut::<UpdateState>().job = None;
            cx.refresh_windows();
        }
        return;
    }
    set_phase(id, Phase::Failed(error_text(&err)), cx);
}

pub fn start_update(cx: &mut App) {
    let Some(release) = cx.global::<UpdateState>().release.clone() else {
        return;
    };
    // The update window disables "Update Now" and explains a missing package.
    let Some(asset) = release.asset.clone() else {
        return;
    };
    let state = cx.global_mut::<UpdateState>();
    if state.busy() {
        return;
    }
    let downloaded = Arc::new(AtomicU64::new(0));
    let cancel = http::CancelToken::default();
    let id = next_job_id();
    state.job = Some(Job {
        id,
        downloaded: downloaded.clone(),
        total: asset.size,
        cancel: cancel.clone(),
        phase: Phase::Downloading,
        archive: None,
        work: None,
        #[cfg(target_os = "windows")]
        expected_sha256: asset.sha256.clone(),
        #[cfg(target_os = "windows")]
        expected_size: asset.size,
    });
    cx.refresh_windows();

    let finished = Arc::new(AtomicBool::new(false));
    let poll_finished = finished.clone();
    cx.spawn(async move |cx| {
        while !poll_finished.load(Ordering::Relaxed) {
            cx.background_executor().timer(PROGRESS_INTERVAL).await;
            cx.update(|cx| cx.refresh_windows());
        }
    })
    .detach();

    // Only the Windows install needs the version: it names the scratch
    // directory the installer helper reads from.
    #[cfg(not(target_os = "macos"))]
    let version = release.version.clone();
    cx.spawn(async move |cx| {
        let worker_cancel = cancel.clone();
        let fetched = cx
            .background_executor()
            .spawn(async move {
                let name = url_file_name(&asset.url);
                // macOS gets the disk image in Downloads where the user can
                // install it; Windows keeps it in a scratch directory the
                // installer helper can reach.
                #[cfg(target_os = "macos")]
                let (dest, work) = (download_dir().join(&name), None);
                #[cfg(not(target_os = "macos"))]
                let (dest, work) = {
                    let work = work_dir(&version)?;
                    (work.join(&name), Some(work))
                };
                // Stage beside the target so a cancelled run never truncates
                // a package the user already had there. The staging name is
                // unique across processes and jobs.
                let staging = unique_staging(&dest);
                let result = (|| -> Result<Downloaded> {
                    download(&asset, &staging, &downloaded, &worker_cancel)?;
                    check_cancel(&worker_cancel)?;
                    #[cfg(target_os = "macos")]
                    {
                        // The fixed public name is claimed on the UI thread,
                        // after the job identity and cancellation are checked.
                        Ok(Downloaded {
                            archive: staging.clone(),
                            target: dest,
                            work: work.clone(),
                        })
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        // Windows needs the package's real extension, and this
                        // rename stays inside this job's unique work directory.
                        fs::rename(&staging, &dest).localize("update.error.install")?;
                        check_cancel(&worker_cancel)?;
                        Ok(Downloaded {
                            archive: dest.clone(),
                            target: dest,
                            work: work.clone(),
                        })
                    }
                })();
                if result.is_err() {
                    let _ = fs::remove_file(&staging);
                    if let Some(work) = &work {
                        let _ = fs::remove_dir_all(work);
                    }
                }
                result
            })
            .await;
        finished.store(true, Ordering::Relaxed);
        cx.update(|cx| match fetched {
            Ok(downloaded) => publish_download(id, &cancel, downloaded, cx),
            Err(err) => fail(id, err, cx),
        });
    })
    .detach();
}

fn publish_download(id: u64, cancel: &http::CancelToken, downloaded: Downloaded, cx: &mut App) {
    if !cx.global::<UpdateState>().can_publish_ready(id, cancel) {
        let _ = fs::remove_file(&downloaded.archive);
        if let Some(work) = downloaded.work {
            let _ = fs::remove_dir_all(work);
        }
        return;
    }
    if downloaded.archive != downloaded.target
        && let Err(err) = claim_download_target(&downloaded.archive, &downloaded.target)
            .localize("update.error.install")
    {
        let _ = fs::remove_file(&downloaded.archive);
        if let Some(work) = downloaded.work {
            let _ = fs::remove_dir_all(work);
        }
        fail(id, err, cx);
        return;
    }
    // Re-check immediately before Ready. This update runs on the UI thread,
    // so dismissal cannot interleave between the check and publication.
    if !cx.global::<UpdateState>().can_publish_ready(id, cancel) {
        let _ = fs::remove_file(&downloaded.target);
        if let Some(work) = downloaded.work {
            let _ = fs::remove_dir_all(work);
        }
        return;
    }
    let state = cx.global_mut::<UpdateState>();
    if let Some(job) = &mut state.job {
        job.archive = Some(downloaded.target);
        job.work = downloaded.work;
    }
    set_phase(id, Phase::Ready, cx);
}

/// Windows: install the verified package and restart, on the user's click.
#[cfg(not(target_os = "macos"))]
pub fn install_ready(cx: &mut App) {
    let Some((id, archive, work, version, expected_sha256, expected_size)) = (|| {
        let state = cx.global::<UpdateState>();
        let job = state.job()?;
        if !matches!(job.phase, Phase::Ready) {
            return None;
        }
        let (Some(archive), Some(work)) = (job.archive.clone(), job.work.clone()) else {
            return None;
        };
        Some((
            job.id,
            archive,
            work,
            state.job_version(),
            job.expected_sha256.clone(),
            job.expected_size,
        ))
    })() else {
        return;
    };
    if !archive.exists() {
        forget_stale_package(cx);
        return;
    }
    set_phase(id, Phase::Installing, cx);
    cx.spawn(async move |cx| {
        let installed = cx
            .background_executor()
            .spawn(async move {
                let relaunch = install(&archive, &work, &version, &expected_sha256, expected_size);
                // The Windows helper needs the work directory until it runs.
                if relaunch.is_err() {
                    let _ = fs::remove_dir_all(&work);
                }
                relaunch
            })
            .await;
        cx.update(|cx| match installed {
            Ok(relaunch) => restart(id, relaunch, cx),
            Err(err) => fail(id, err, cx),
        });
    })
    .detach();
}

/// macOS: show the saved disk image in Finder; installing it is the user's job.
#[cfg(target_os = "macos")]
pub fn reveal_ready(cx: &mut App) {
    let Some(job) = cx.global::<UpdateState>().job() else {
        return;
    };
    if !matches!(job.phase, Phase::Ready) {
        return;
    }
    let Some(archive) = job.archive.clone() else {
        return;
    };
    if !archive.exists() {
        forget_stale_package(cx);
        return;
    }
    reveal(&archive);
}

/// The package was moved or deleted after the download: drop the job so the
/// window offers a fresh download instead of a dead button.
pub(crate) fn forget_stale_package(cx: &mut App) {
    cx.global_mut::<UpdateState>().job = None;
    cx.refresh_windows();
}

/// Start the installed version and quit this one through the app's own quit
/// path.
#[cfg(not(target_os = "macos"))]
fn restart(id: u64, mut relaunch: Command, cx: &mut App) {
    if let Err(err) = relaunch.spawn() {
        set_phase(
            id,
            Phase::Failed(
                UpdateError::new("update.error.relaunch")
                    .detail(err)
                    .to_string(),
            ),
            cx,
        );
        return;
    }
    cx.dispatch_action(&crate::Quit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0.0", "1.0.0-beta.2"));
        assert!(is_newer("1.0.0-beta.11", "1.0.0-beta.2"));
        assert!(is_newer("1.0.0-beta", "1.0.0-alpha.1"));
        assert!(is_newer("1.0.0-alpha.1", "1.0.0-alpha"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0+build.5", "1.0.0"));
        assert!(!is_newer("1.0.0-rc.1", "1.0.0"));
        assert!(!is_newer("0.9.9", "1.0.0"));
        assert!(!is_newer("1.0", "0.1.0"));
        assert!(!is_newer("1.0.0.1", "0.1.0"));
        assert!(!is_newer("1.0.0-", "0.1.0"));
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("1.0.0-../../outside", "0.1.0"));
        assert_eq!(parse_version("1.0.0-beta_1"), None);
        assert_eq!(parse_version("1.0.0-beta..1"), None);
        assert_eq!(
            parse_version("1.0.0-beta-1"),
            Some(([1, 0, 0], Some("beta-1")))
        );
        assert_eq!(
            parse_version("v1.2.3-rc.1+x"),
            Some(([1, 2, 3], Some("rc.1")))
        );
    }

    #[test]
    fn platform_key_matches_release_assets() {
        let key = platform_key();
        let expected = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            "macos-aarch64"
        } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            "macos-x86_64"
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            "windows-x86_64"
        } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
            "windows-aarch64"
        } else {
            return;
        };
        assert_eq!(key, expected);
    }

    const MANIFEST: &str = r#"{
        "version": "v0.3.0",
        "notes": "Fixes\n- a bug",
        "pub_date": "2026-11-06T00:00:00Z",
        "assets": {
            "macos-aarch64": {"url": "https://example.com/a.zip", "sha256": "AB0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd", "size": 10},
            "freebsd-x86_64": {"url": "ftp://example.com/a.zip", "sha256": "ab0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd", "size": 10},
            "windows-x86_64": {"url": "https://example.com/a.zip", "sha256": "not-hex", "size": 10}
        }
    }"#;

    #[test]
    fn parses_manifest_and_selects_assets() {
        let manifest = parse_manifest(MANIFEST).unwrap();
        assert_eq!(manifest.version, "v0.3.0");
        let asset = select_asset(&manifest, "macos-aarch64").unwrap();
        assert_eq!(
            (asset.url.as_str(), asset.size),
            ("https://example.com/a.zip", 10)
        );
        assert_eq!(select_asset(&manifest, "freebsd-x86_64"), None);
        assert_eq!(select_asset(&manifest, "windows-x86_64"), None);
        assert_eq!(select_asset(&manifest, "windows-aarch64"), None);

        let release =
            evaluate(parse_manifest(MANIFEST).unwrap(), "0.2.9", "macos-aarch64").unwrap();
        assert_eq!(
            (release.version.as_str(), release.notes.as_str()),
            ("0.3.0", "Fixes\n- a bug")
        );
        assert!(release.asset.is_some());
        assert!(
            evaluate(
                parse_manifest(MANIFEST).unwrap(),
                "0.2.9",
                "freebsd-aarch64"
            )
            .unwrap()
            .asset
            .is_none()
        );
        assert!(evaluate(parse_manifest(MANIFEST).unwrap(), "0.3.0", "macos-aarch64").is_none());

        assert!(parse_manifest(r#"{"version": "next"}"#).is_err());
        assert!(parse_manifest("<html>").is_err());
        let minimal = parse_manifest(r#"{"version": "1.0.0"}"#).unwrap();
        assert!(minimal.assets.is_empty() && minimal.notes.is_empty());
    }

    #[test]
    fn picks_the_newest_release_with_a_manifest() {
        let entries: Vec<ReleaseEntry> = serde_json::from_str(
            r#"[
                {"tag_name": "v0.4.0", "html_url": "https://github.com/o/r/releases/tag/v0.4.0",
                 "assets": [{"name": "app.zip", "browser_download_url": "https://x/0.4.0/app.zip"}]},
                {"tag_name": "v0.3.0", "html_url": "https://github.com/o/r/releases/tag/v0.3.0",
                 "assets": [{"name": "latest.json", "browser_download_url": "https://x/0.3.0/latest.json"}]},
                {"tag_name": "v0.5.0", "html_url": "https://github.com/o/r/releases/tag/v0.5.0",
                 "draft": true,
                 "assets": [{"name": "latest.json", "browser_download_url": "https://x/0.5.0/latest.json"}]},
                {"tag_name": "nightly", "html_url": "https://github.com/o/r/releases/tag/nightly",
                 "assets": [{"name": "latest.json", "browser_download_url": "https://x/n/latest.json"}]},
                {"tag_name": "v1.0.0-beta.2", "html_url": "https://github.com/o/r/releases/tag/v1.0.0-beta.2",
                 "assets": [{"name": "latest.json", "browser_download_url": "https://x/beta/latest.json"}]}
            ]"#,
        )
        .unwrap();
        // The highest version wins even when it is a pre-release, which
        // `releases/latest` would never report.
        let source = newest_release(&entries).unwrap();
        assert_eq!(source.manifest_url, "https://x/beta/latest.json");
        assert_eq!(
            source.page_url,
            "https://github.com/o/r/releases/tag/v1.0.0-beta.2"
        );
        assert!(newest_release(&[]).is_none());
    }

    #[test]
    fn falls_back_to_release_pages() {
        let feed = r#"<feed>
            <entry><link href="https://github.com/o/r/releases/tag/v0.3.0"/></entry>
            <entry><link href="https://github.com/o/r/releases/tag/v0.10.0"/></entry>
            <entry><link href="https://github.com/o/r/releases/tag/v0.10.0"/></entry>
            <entry><link href="https://github.com/o/r/releases/tag/nightly"/></entry>
            <entry><title>v9.9.9</title></entry>
        </feed>"#;
        let tags = atom_tags(feed);
        assert_eq!(tags, vec!["v0.3.0", "v0.10.0"]);
        assert_eq!(newest_tag(&tags).map(String::as_str), Some("v0.10.0"));
        assert!(newest_tag(&[]).is_none());

        let relative = r#"<a href="/o/r/releases/download/v0.10.0/latest.json">latest.json</a>"#;
        assert_eq!(
            expanded_manifest_url(relative, "v0.10.0").as_deref(),
            Some("https://github.com/o/r/releases/download/v0.10.0/latest.json")
        );
        assert_eq!(expanded_manifest_url(relative, "v0.3.0"), None);
        let absolute = concat!(
            r#"<a href="https://github.com/o/r/releases/download/"#,
            r#"v0.10.0/latest.json">x</a>"#
        );
        assert_eq!(
            expanded_manifest_url(absolute, "v0.10.0").as_deref(),
            Some("https://github.com/o/r/releases/download/v0.10.0/latest.json")
        );
        // Anything else, such as a link to another host, is refused.
        let elsewhere = concat!(
            r#"<a href="https://evil.example/o/r/releases/download/"#,
            r#"v0.10.0/latest.json">x</a>"#
        );
        assert_eq!(expanded_manifest_url(elsewhere, "v0.10.0"), None);
    }

    #[test]
    fn confirms_before_a_prerelease() {
        assert!(needs_confirm("0.1.0", "0.2.0-beta.1"));
        assert!(!needs_confirm("0.1.0", "0.2.0"));
        assert!(!needs_confirm("0.2.0-beta.1", "0.2.0-beta.2"));
        assert!(!needs_confirm("0.2.0-beta.1", "0.2.0"));
        assert!(is_pre("1.0.0-rc.1"));
        assert!(!is_pre("1.0.0"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prefers_the_disk_image_on_macos() {
        let sha = "ab0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd";
        let both = parse_manifest(&format!(
            r#"{{"version": "0.3.0", "assets": {{
                "macos-aarch64": {{"url": "https://example.com/a.zip", "sha256": "{sha}", "size": 10}},
                "macos-aarch64-dmg": {{"url": "https://example.com/a.dmg", "sha256": "{sha}", "size": 20}}
            }}}}"#
        ))
        .unwrap();
        assert_eq!(select_asset(&both, "macos-aarch64").unwrap().size, 10);
        let asset = update_asset(&both, "macos-aarch64").unwrap();
        assert_eq!(
            (asset.url.as_str(), asset.size),
            ("https://example.com/a.dmg", 20)
        );

        // A release that only publishes the zip still updates.
        let only_zip = parse_manifest(&format!(
            r#"{{"version": "0.3.0", "assets": {{
                "macos-aarch64": {{"url": "https://example.com/a.zip", "sha256": "{sha}", "size": 10}}
            }}}}"#
        ))
        .unwrap();
        assert_eq!(
            update_asset(&only_zip, "macos-aarch64").unwrap().url,
            "https://example.com/a.zip"
        );
    }

    #[test]
    fn verifies_size_and_checksum() {
        let digest = hex(&Sha256::digest(b"abc"));
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let asset = Asset {
            url: String::new(),
            sha256: digest.to_uppercase(),
            size: 3,
        };
        assert!(verify(3, &digest, &asset).is_ok());
        assert!(verify(4, &digest, &asset).is_err());
        assert!(verify(3, &hex(&Sha256::digest(b"abd")), &asset).is_err());

        let dir = std::env::temp_dir().join(format!(
            "snapit-updater-verify-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let package = dir.join("setup.exe");
        fs::write(&package, b"abc").unwrap();
        assert!(verify_file(&package, 3, &digest).is_ok());
        assert!(verify_file(&package, 4, &digest).is_err());
        assert!(verify_file(&package, 3, &"0".repeat(64)).is_err());
        fs::write(&package, b"abd").unwrap();
        assert!(verify_file(&package, 3, &digest).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_placeholder_repository() {
        assert_eq!(parse_repo("https://github.com/OWNER/SnapIt"), None);
        assert_eq!(
            parse_repo("https://github.com/gjfei/snapit"),
            Some("gjfei/snapit".into())
        );
        assert_eq!(
            parse_repo("https://github.com/a-b/c.d.git/"),
            Some("a-b/c.d".into())
        );
        assert_eq!(parse_repo("gjfei/snapit"), Some("gjfei/snapit".into()));
        assert_eq!(parse_repo("https://gitlab.com/a/b"), None);
        assert_eq!(parse_repo("https://github.com/a/b/tree/main"), None);
        assert_eq!(parse_repo(""), None);
        assert_eq!(
            repo().is_some(),
            !env!("CARGO_PKG_REPOSITORY").contains("/OWNER/")
                || option_env!("SNAPIT_UPDATE_REPO").is_some()
        );
    }

    #[test]
    fn parses_update_helper_arguments() {
        assert_eq!(installer_args(["app"]), InstallerArgs::Normal);
        assert_eq!(
            installer_args(["app", "--other", "x"]),
            InstallerArgs::Normal
        );
        let setup = r"C:\Temp\setup.exe";
        let exe = r"C:\Program Files\SnapIt\SnapIt.exe";
        let sha = "00".repeat(32);
        assert_eq!(
            installer_args([
                "app",
                UPDATE_INSTALL_ARG,
                setup,
                exe,
                "42",
                sha.as_str(),
                "123"
            ]),
            InstallerArgs::Install {
                setup: PathBuf::from(setup),
                exe: PathBuf::from(exe),
                pid: 42,
                sha256: sha.clone(),
                size: 123,
            }
        );
        assert_eq!(
            installer_args(["app", UPDATE_INSTALL_ARG, setup, exe, "42", "bad", "123"]),
            InstallerArgs::Invalid
        );
        assert_eq!(
            installer_args(["app", UPDATE_INSTALL_ARG]),
            InstallerArgs::Invalid
        );
        assert_eq!(
            installer_args(["app", UPDATE_INSTALL_ARG, "setup", "exe"]),
            InstallerArgs::Invalid
        );
        assert_eq!(
            installer_args(["app", UPDATE_INSTALL_ARG, "setup", "exe", "nope"]),
            InstallerArgs::Invalid
        );
        assert_eq!(
            installer_args(["app", UPDATE_INSTALL_ARG, "setup", "exe", "1", "extra"]),
            InstallerArgs::Invalid
        );
    }

    #[test]
    fn small_helpers() {
        assert_eq!(truncate_notes("abcdef", 3), "abc…");
        assert_eq!(truncate_notes("说明文字", 10), "说明文字");
        assert_eq!(truncate_notes("说明文字", 2), "说明…");
        assert!(due(None, 100));
        assert!(!due(Some(100), 100 + CHECK_INTERVAL_SECS - 1));
        assert!(due(Some(100), 100 + CHECK_INTERVAL_SECS));
        assert!(due(Some(200), 100));
        assert_eq!(
            url_file_name("https://x/y/SnapIt-1.0-macos.zip?a=b"),
            "SnapIt-1.0-macos.zip"
        );
        assert_eq!(url_file_name("https://x/y/../"), "update-package");
        assert_eq!(url_file_name("https://x/y/.."), "update-package");
        assert_eq!(url_file_name("https://x/y/."), "update-package");
        assert_eq!(
            with_suffix(Path::new("/a/b.exe"), ".old"),
            PathBuf::from("/a/b.exe.old")
        );
    }

    /// Downloads a package from a local release, by hand:
    /// `SNAPIT_TEST_MANIFEST_URL=http://127.0.0.1:8765/latest.json
    /// cargo test simulate_update -- --ignored`
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn simulate_update() {
        let url = std::env::var("SNAPIT_TEST_MANIFEST_URL").unwrap();
        let release = evaluate(
            fetch_manifest(&url).unwrap(),
            &std::env::var("SNAPIT_TEST_CURRENT").unwrap_or_else(|_| "0.0.1".to_string()),
            &platform_key(),
        )
        .expect("a newer release");
        let asset = release.asset.clone().expect("a package for this platform");

        let work = work_dir(&release.version).unwrap();
        let archive = work.join(url_file_name(&asset.url));
        let progress = AtomicU64::new(0);
        let cancel = http::CancelToken::default();
        cancel.cancel();
        let cancelled = download(&asset, &archive, &progress, &cancel).unwrap_err();
        assert!(cancelled.is::<Cancelled>() && !archive.exists());
        download(&asset, &archive, &progress, &http::CancelToken::default()).unwrap();
        assert_eq!(progress.load(Ordering::Relaxed), asset.size);
        assert_eq!(fs::read(&archive).unwrap().len() as u64, asset.size);

        // A package that does not match the manifest is refused, and the
        // rejected file is removed rather than left looking installable.
        let wrong = Asset {
            url: asset.url.clone(),
            sha256: "0".repeat(64),
            size: asset.size,
        };
        let rejected = work.join("rejected.dmg");
        let err =
            download(&wrong, &rejected, &progress, &http::CancelToken::default()).unwrap_err();
        eprintln!("mismatched package refused: {err:#}");
        assert!(!rejected.exists());
        let _ = fs::remove_dir_all(&work);
    }

    fn test_job(id: u64, cancel: http::CancelToken) -> Job {
        Job {
            id,
            downloaded: Arc::new(AtomicU64::new(0)),
            total: 0,
            cancel,
            phase: Phase::Downloading,
            archive: None,
            work: None,
            #[cfg(target_os = "windows")]
            expected_sha256: String::new(),
            #[cfg(target_os = "windows")]
            expected_size: 0,
        }
    }

    #[test]
    fn only_the_current_uncancelled_job_can_publish_ready() {
        let cancel = http::CancelToken::default();
        let mut state = UpdateState {
            job: Some(test_job(1, cancel.clone())),
            ..UpdateState::default()
        };
        assert!(!state.can_publish_ready(2, &cancel));
        assert!(state.can_publish_ready(1, &cancel));
        cancel.cancel();
        assert!(!state.can_publish_ready(1, &cancel));
        state.job = None;
        assert!(!state.busy());
        assert!(!state.can_publish_ready(1, &cancel));
    }

    #[test]
    fn work_directories_are_unique_and_owned_with_create_new() {
        let base = std::env::temp_dir().join(format!(
            "snapit-updater-work-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|index| {
                let base = base.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    create_work_dir_in(&base, "1.2.3-beta.1", std::process::id(), index).unwrap()
                })
            })
            .collect();
        let dirs: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_ne!(dirs[0], dirs[1]);
        for (index, dir) in dirs.iter().enumerate() {
            assert_eq!(work_owner(dir), Some((std::process::id(), index as u64)));
            assert!(
                create_work_dir_in(&base, "1.2.3-beta.1", std::process::id(), index as u64)
                    .is_err()
            );
            assert!(dir.is_dir(), "a losing creator must not remove the owner");
        }
        let first = work_dir("1.2.3").unwrap();
        let second = work_dir("1.2.3").unwrap();
        assert_ne!(first, second);
        fs::remove_dir_all(&first).unwrap();
        fs::remove_dir_all(&second).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn startup_cleanup_preserves_live_peers_and_removes_only_proven_stale_dirs() {
        let base = std::env::temp_dir().join(format!(
            "snapit-updater-cleanup-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        let ours = create_work_dir_in(&base, "1.0.0", std::process::id(), 100).unwrap();
        let live = base.join(format!("snapit-update-1.0.0-{}-101", std::process::id()));
        let dead = base.join(format!("snapit-update-1.0.0-{}-102", u32::MAX));
        for dir in [&live, &dead] {
            fs::create_dir(dir).unwrap();
            let owner = if dir == &live {
                &format!("{} 101\n", std::process::id())
            } else {
                &format!("{} 102\n", u32::MAX)
            };
            fs::write(dir.join(WORK_OWNER_FILE), owner).unwrap();
        }
        let unrelated = base.join("snapit-update-unowned");
        fs::create_dir(&unrelated).unwrap();

        remove_stale_update_dirs_in(&base);

        assert!(
            !ours.exists(),
            "this instance's directories are provably ours"
        );
        assert!(live.exists(), "a live peer's directory must survive");
        assert!(!dead.exists(), "a provably dead owner's directory is stale");
        assert!(unrelated.exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn staging_names_do_not_collide_across_jobs() {
        let target = Path::new("/tmp/SnapIt-1.0.0.dmg");
        assert_ne!(unique_staging(target), unique_staging(target));
    }

    #[test]
    fn public_download_name_is_claimed_without_overwriting_a_peer() {
        let dir = std::env::temp_dir().join(format!(
            "snapit-updater-publish-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let staging = dir.join("SnapIt-1.0.0.dmg.part");
        let target = dir.join("SnapIt-1.0.0.dmg");
        fs::write(&staging, b"new package").unwrap();
        claim_download_target(&staging, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new package");
        assert!(!staging.exists());

        fs::write(&staging, b"peer package").unwrap();
        let err = claim_download_target(&staging, &target).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&target).unwrap(), b"new package");
        assert_eq!(fs::read(&staging).unwrap(), b"peer package");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancellation_terminates_a_blocked_curl_and_removes_staging() {
        use std::io::Write as _;
        use std::net::TcpListener;
        use std::sync::{Barrier, mpsc};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/update.dmg", listener.local_addr().unwrap());
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new(Barrier::new(2));
        let server_release = release.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\nabc")
                .unwrap();
            stream.flush().unwrap();
            started_tx.send(()).unwrap();
            server_release.wait();
            let _ = stream.write_all(&[0; 1021]);
        });
        let dir =
            std::env::temp_dir().join(format!("snapit-updater-cancel-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let staging = dir.join("update.dmg");
        let asset = Asset {
            url,
            sha256: "0".repeat(64),
            size: 1024,
        };
        let cancel = http::CancelToken::default();
        let worker_cancel = cancel.clone();
        let worker_staging = staging.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            result_tx
                .send(download(
                    &asset,
                    &worker_staging,
                    &AtomicU64::new(0),
                    &worker_cancel,
                ))
                .unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        cancel.cancel();
        release.wait();
        let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        server.join().unwrap();
        assert!(result.unwrap_err().is::<Cancelled>());
        assert!(!staging.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn truncated_transfer_is_rejected_and_removes_staging() {
        use std::io::Write as _;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/update.dmg", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\nabc")
                .unwrap();
        });
        let dir = std::env::temp_dir().join(format!(
            "snapit-updater-truncated-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let staging = dir.join("update.dmg");
        let asset = Asset {
            url,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            size: 3,
        };
        let err = download(
            &asset,
            &staging,
            &AtomicU64::new(0),
            &http::CancelToken::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("curl"), "{err:#}");
        assert!(!staging.exists());
        server.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replaces_binary_in_place() {
        let dir = std::env::temp_dir().join(format!("snapit-updater-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (exe, new) = (dir.join("app"), dir.join("new-app"));
        fs::write(&exe, "old").unwrap();
        fs::write(&new, "new").unwrap();
        replace_binary(&new, &exe).unwrap();
        assert_eq!(fs::read_to_string(&exe).unwrap(), "new");
        assert!(!with_suffix(&exe, ".new").exists());
        assert!(is_writable(&dir));
        let found = find_entry(&dir, &|p: &Path| p.file_name().is_some_and(|n| n == "app"));
        assert_eq!(found, Some(exe));
        fs::remove_dir_all(&dir).unwrap();
    }
}
