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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    let mut body =
        http::get(url, MANIFEST_TIMEOUT, MAX_MANIFEST_BYTES).localize("update.error.network")?;
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
    use std::process::{Child, ChildStdout, Command, Stdio};
    use std::time::Duration;

    /// A response body streamed from curl's stdout.
    pub struct Body {
        child: Child,
        stdout: ChildStdout,
        received: u64,
        max_len: u64,
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
    pub fn get(url: &str, timeout: Duration, max_len: u64) -> io::Result<Body> {
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
        Ok(Body {
            child,
            stdout,
            received: 0,
            max_len,
        })
    }

    impl Read for Body {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.stdout.read(buf)?;
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
            let mut stderr = String::new();
            if let Some(mut pipe) = self.child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            let status = self.child.wait()?;
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
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn fetch_manifest(url: &str) -> Result<Manifest> {
    let mut body =
        http::get(url, MANIFEST_TIMEOUT, MAX_MANIFEST_BYTES).localize("update.error.network")?;
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

/// Download `asset` to `dest`, counting bytes in `progress`, then verify it.
fn download(asset: &Asset, dest: &Path, progress: &AtomicU64, cancel: &AtomicBool) -> Result<()> {
    let mut body =
        http::get(&asset.url, DOWNLOAD_TIMEOUT, asset.size).localize("update.error.download")?;
    let mut file = fs::File::create(dest).localize("update.error.download")?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    let mut size = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = fs::remove_file(dest);
            return Err(Cancelled.into());
        }
        let n = body.read(&mut buf).localize("update.error.download")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .localize("update.error.download")?;
        size += n as u64;
        progress.store(size, Ordering::Relaxed);
    }
    body.finish().localize("update.error.download")?;
    file.sync_all().localize("update.error.download")?;
    drop(file);
    // A package that fails verification must not sit on disk looking
    // installable.
    let verified = verify(size, &hex(&hasher.finalize()), asset);
    if verified.is_err() {
        let _ = fs::remove_file(dest);
    }
    verified
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

/// A fresh scratch directory for one update. macOS has no installer to feed,
/// so nothing there needs a work directory.
#[cfg(any(not(target_os = "macos"), test))]
fn work_dir(version: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("snapit-update-{version}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).localize("update.error.download")?;
    Ok(dir)
}

/// Remove scratch directories left by earlier updates. The Windows helper
/// cannot delete its own copy while it runs, so its directory stays behind.
fn remove_stale_update_dirs() {
    let Ok(entries) = std::env::temp_dir().read_dir() else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("snapit-update-")
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
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
    downloaded: Arc<AtomicU64>,
    pub(crate) total: u64,
    cancel: Arc<AtomicBool>,
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
    cx.set_global(UpdateState::default());
    cx.spawn(async move |cx| {
        cx.background_executor().timer(STARTUP_DELAY).await;
        cx.background_executor()
            .spawn(async {
                remove_leftovers();
                remove_stale_update_dirs();
            })
            .await;
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
    match state.job.as_ref() {
        Some(job) if matches!(job.phase, Phase::Downloading) => {
            job.cancel.store(true, Ordering::Relaxed);
        }
        Some(job) if matches!(job.phase, Phase::Ready | Phase::Failed(_)) => {
            if let Some(job) = state.job.take() {
                if let Some(archive) = job.archive {
                    let _ = fs::remove_file(archive);
                }
                if let Some(work) = job.work {
                    let _ = fs::remove_dir_all(work);
                }
            }
        }
        _ => {}
    }
    cx.refresh_windows();
}

fn set_phase(phase: Phase, cx: &mut App) {
    if let Some(job) = &mut cx.global_mut::<UpdateState>().job {
        job.phase = phase;
    }
    cx.refresh_windows();
}

fn fail(err: anyhow::Error, cx: &mut App) {
    if err.is::<Cancelled>() {
        cx.global_mut::<UpdateState>().job = None;
        cx.refresh_windows();
        return;
    }
    let message = error_text(&err);
    set_phase(Phase::Failed(message), cx);
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
    let cancel = Arc::new(AtomicBool::new(false));
    state.job = Some(Job {
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
                // a package the user already had there.
                let staging = with_suffix(&dest, ".part");
                let _ = fs::remove_file(&staging);
                download(&asset, &staging, &downloaded, &cancel)?;
                fs::rename(&staging, &dest).localize("update.error.install")?;
                anyhow::Ok((dest, work))
            })
            .await;
        finished.store(true, Ordering::Relaxed);
        cx.update(|cx| match fetched {
            Ok((archive, work)) => {
                let state = cx.global_mut::<UpdateState>();
                if let Some(job) = &mut state.job {
                    job.archive = Some(archive);
                    job.work = work;
                }
                set_phase(Phase::Ready, cx);
            }
            Err(err) => fail(err, cx),
        });
    })
    .detach();
}

/// Windows: install the verified package and restart, on the user's click.
#[cfg(not(target_os = "macos"))]
pub fn install_ready(cx: &mut App) {
    let Some(job) = cx.global::<UpdateState>().job() else {
        return;
    };
    if !matches!(job.phase, Phase::Ready) {
        return;
    }
    let (Some(archive), Some(work), version, expected_sha256, expected_size) = (
        job.archive.clone(),
        job.work.clone(),
        cx.global::<UpdateState>().job_version(),
        job.expected_sha256.clone(),
        job.expected_size,
    ) else {
        return;
    };
    if !archive.exists() {
        forget_stale_package(cx);
        return;
    }
    set_phase(Phase::Installing, cx);
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
            Ok(relaunch) => restart(relaunch, cx),
            Err(err) => fail(err, cx),
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
fn restart(mut relaunch: Command, cx: &mut App) {
    if let Err(err) = relaunch.spawn() {
        set_phase(
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
        let cancelled = download(&asset, &archive, &progress, &AtomicBool::new(true)).unwrap_err();
        assert!(cancelled.is::<Cancelled>() && !archive.exists());
        download(&asset, &archive, &progress, &AtomicBool::new(false)).unwrap();
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
        let err = download(&wrong, &rejected, &progress, &AtomicBool::new(false)).unwrap_err();
        eprintln!("mismatched package refused: {err:#}");
        assert!(!rejected.exists());
        let _ = fs::remove_dir_all(&work);
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
