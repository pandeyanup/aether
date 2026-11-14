use std::cmp::Ordering as CmpOrdering;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui_kit::component::button::ButtonVariants;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Disableable, Sizable, WindowExt, h_flex};
use gpui_kit::{
    AnyElement, AppContext, Context, InteractiveElement, IntoElement, ParentElement, Styled, Window,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::app::ChatApp;

pub(crate) const RELEASES_PAGE_URL: &str = "https://github.com/PrashnaLabs/aether/releases";
const RELEASES_API_URL: &str =
    "https://api.github.com/repos/PrashnaLabs/aether/releases?per_page=20";
pub(crate) const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(80);
const USER_AGENT: &str = concat!("aether/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreRelease {
    Alpha(u64),
    Beta(u64),
    Rc(u64),
    Other(String),
    Stable,
}

impl PreRelease {
    fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim().to_ascii_lowercase();
        if s.is_empty() {
            return Some(Self::Stable);
        }
        if let Some(rest) = s.strip_prefix("alpha") {
            return parse_pre_num(rest).map(Self::Alpha);
        }
        if let Some(rest) = s.strip_prefix("beta") {
            return parse_pre_num(rest).map(Self::Beta);
        }
        if let Some(rest) = s.strip_prefix("rc") {
            return parse_pre_num(rest).map(Self::Rc);
        }
        Some(Self::Other(s))
    }

    fn rank(&self) -> (u8, u64, &str) {
        match self {
            Self::Alpha(n) => (0, *n, ""),
            Self::Beta(n) => (1, *n, ""),
            Self::Rc(n) => (2, *n, ""),
            Self::Other(s) => (3, 0, s.as_str()),
            Self::Stable => (4, 0, ""),
        }
    }
}

fn parse_pre_num(rest: &str) -> Option<u64> {
    let trimmed = rest.trim_start_matches(['.', '-', '_']);
    if trimmed.is_empty() {
        return Some(0);
    }
    trimmed.parse::<u64>().ok()
}

impl Ord for PreRelease {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for PreRelease {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: PreRelease,
}

impl Version {
    /// Parses SemVer tags (`0.1.0-alpha`, `v0.1.0`) and prefix-prerelease tags (`alpha-0.1.1`).
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let mut s = raw.trim();
        if s.is_empty() {
            return None;
        }
        if let Some((before_plus, _)) = s.split_once('+') {
            s = before_plus.trim();
        }
        for prefix in ["aether-", "Aether-", "AETHER-", "aether ", "Aether "] {
            if let Some(rest) = s.strip_prefix(prefix) {
                s = rest.trim();
                break;
            }
        }
        s = strip_leading_v(s);

        let mut prefix_pre = None;
        let version_part = match s.split_once('-') {
            Some((left, right)) if !left.bytes().next().is_some_and(|b| b.is_ascii_digit()) => {
                prefix_pre = Some(left);
                strip_leading_v(right)
            }
            _ => s,
        };

        let (core, suffix_pre) = match version_part.split_once('-') {
            Some((core, suffix)) => (core, Some(suffix)),
            None => (version_part, None),
        };

        let mut parts = core.split('.');
        let major = parts.next()?.parse::<u64>().ok()?;
        let minor = parts.next()?.parse::<u64>().ok()?;
        let patch = parts.next()?.parse::<u64>().ok()?;
        if parts.next().is_some() {
            return None;
        }

        let pre = match (suffix_pre, prefix_pre) {
            (Some(label), _) | (None, Some(label)) => PreRelease::parse(label)?,
            (None, None) => PreRelease::Stable,
        };

        Some(Self { major, minor, patch, pre })
    }

    pub(crate) fn display(&self) -> String {
        let base = format!("{}.{}.{}", self.major, self.minor, self.patch);
        match &self.pre {
            PreRelease::Stable => base,
            PreRelease::Alpha(0) => format!("{base}-alpha"),
            PreRelease::Alpha(n) => format!("{base}-alpha.{n}"),
            PreRelease::Beta(0) => format!("{base}-beta"),
            PreRelease::Beta(n) => format!("{base}-beta.{n}"),
            PreRelease::Rc(0) => format!("{base}-rc"),
            PreRelease::Rc(n) => format!("{base}-rc.{n}"),
            PreRelease::Other(s) => format!("{base}-{s}"),
        }
    }
}

fn strip_leading_v(s: &str) -> &str {
    s.strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .filter(|rest| rest.bytes().next().is_some_and(|b| b.is_ascii_digit()))
        .unwrap_or(s)
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct GithubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub sha256_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReleaseInfo {
    pub tag: String,
    pub version: Version,
    pub display_version: String,
    pub html_url: String,
    pub notes: String,
    pub asset: Option<ReleaseAsset>,
}

#[derive(Clone, Debug, Default)]
pub(crate) enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate {
        version: String,
    },
    Available(ReleaseInfo),
    Downloading {
        release: ReleaseInfo,
        done: u64,
        total: u64,
        cancel: Arc<AtomicBool>,
    },
    Installing {
        release: ReleaseInfo,
    },
    ReadyToRestart {
        release: ReleaseInfo,
        target: PathBuf,
    },
    Failed {
        release: Option<ReleaseInfo>,
        error: String,
    },
}

pub(crate) fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub(crate) fn platform_target() -> (&'static str, &'static str) {
    (std::env::consts::OS, std::env::consts::ARCH)
}

/// Extracts a 64-char lowercase SHA-256 hex digest from `"sha256:<hex>"` or `"<hex>  file"`.
pub(crate) fn parse_sha256_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let stripped = trimmed
        .strip_prefix("sha256:")
        .or_else(|| trimmed.strip_prefix("SHA256:"))
        .unwrap_or(trimmed);
    let token = stripped.split_whitespace().next()?;
    if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(token.to_ascii_lowercase())
    } else {
        None
    }
}

pub(crate) fn select_asset(assets: &[GithubAsset], os: &str, arch: &str) -> Option<ReleaseAsset> {
    let ext = match os {
        "macos" => ".dmg",
        "linux" => ".tar.gz",
        "windows" => ".zip",
        _ => return None,
    };
    let suffix = format!("-{os}-{arch}{ext}").to_ascii_lowercase();
    let primary = assets.iter().find(|asset| {
        let lower = asset.name.to_ascii_lowercase();
        !lower.ends_with(".sha256") && lower.ends_with(&suffix)
    })?;
    let sha256 = primary.digest.as_deref().and_then(parse_sha256_text);
    let sidecar_name = format!("{}.sha256", primary.name);
    let sha256_url = assets
        .iter()
        .find(|asset| asset.name == sidecar_name)
        .map(|asset| asset.browser_download_url.clone());

    Some(ReleaseAsset {
        name: primary.name.clone(),
        download_url: primary.browser_download_url.clone(),
        size: primary.size,
        sha256,
        sha256_url,
    })
}

pub(crate) fn latest_release(
    releases: &[GithubRelease],
    os: &str,
    arch: &str,
) -> Option<ReleaseInfo> {
    releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version =
                Version::parse(&release.tag_name).or_else(|| Version::parse(&release.name))?;
            let display_version = version.display();
            let html_url = if release.html_url.trim().is_empty() {
                RELEASES_PAGE_URL.to_string()
            } else {
                release.html_url.clone()
            };
            Some(ReleaseInfo {
                tag: release.tag_name.clone(),
                version,
                display_version,
                html_url,
                notes: release.body.clone().unwrap_or_default(),
                asset: select_asset(&release.assets, os, arch),
            })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
}

pub(crate) fn check_outcome(
    current_raw: &str,
    releases: &[GithubRelease],
    os: &str,
    arch: &str,
) -> Result<Option<ReleaseInfo>, String> {
    let current = Version::parse(current_raw)
        .ok_or_else(|| format!("Unrecognized current version: {current_raw}"))?;
    let Some(latest) = latest_release(releases, os, arch) else {
        return Ok(None);
    };
    if latest.version > current { Ok(Some(latest)) } else { Ok(None) }
}

pub(crate) fn updates_dir() -> Option<PathBuf> {
    aether_core::config_dir().map(|dir| dir.join("updates"))
}

/// Removes temporary update files and Windows `.old` backups from previous runs.
pub(crate) fn cleanup_old_update_artifacts() {
    if let Some(dir) = updates_dir()
        && let Ok(entries) = fs::read_dir(&dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "part")
                || path.file_name().is_some_and(|n| {
                    let s = n.to_string_lossy();
                    s.starts_with("aether-extract-") || s.starts_with("aether-dmg-")
                })
            {
                let _ =
                    if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut old = exe.as_os_str().to_os_string();
        old.push(".old");
        let _ = fs::remove_file(PathBuf::from(old));
    }
}

fn fetch_releases() -> Result<Vec<GithubRelease>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to start network runtime: {e}"))?;
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(20))
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;
        let response = client
            .get(RELEASES_API_URL)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .map_err(|e| format!("Could not reach GitHub Releases: {}", e.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("GitHub Releases returned HTTP {status}"));
        }
        let body = response
            .text()
            .await
            .map_err(|e| format!("Failed to read GitHub Releases response: {e}"))?;
        serde_json::from_str::<Vec<GithubRelease>>(&body)
            .map_err(|e| format!("Invalid GitHub Releases response: {e}"))
    })
}

enum DownloadEvent {
    Progress { done: u64, total: u64 },
    Installing,
}

fn download_and_install(
    release: &ReleaseInfo,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(DownloadEvent),
) -> Result<PathBuf, String> {
    let asset = release
        .asset
        .as_ref()
        .ok_or_else(|| format!("No installer found for this platform at {}", release.html_url))?;
    let dir = updates_dir().ok_or_else(|| "No Aether config directory found".to_string())?;
    aether_core::create_private_dir(&dir)
        .map_err(|e| format!("Could not create updates directory: {e}"))?;

    let part_path = dir.join(format!("{}.part", asset.name));
    let archive_path = dir.join(&asset.name);
    let _ = fs::remove_file(&part_path);
    let _ = fs::remove_file(&archive_path);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Failed to start download runtime: {e}"))?;

    let (actual_sha256, expected_sha256) = runtime.block_on(async {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        let expected = match (&asset.sha256, &asset.sha256_url) {
            (Some(hash), _) => Some(hash.clone()),
            (None, Some(url)) => Some(fetch_sha256(&client, url).await?),
            (None, None) => None,
        };
        let expected = expected.ok_or_else(|| {
            "Release asset is missing a SHA-256 checksum; update aborted for safety.".to_string()
        })?;

        let mut response = client
            .get(&asset.download_url)
            .send()
            .await
            .map_err(|e| format!("Download failed: {}", e.without_url()))?
            .error_for_status()
            .map_err(|e| format!("Download failed: {}", e.without_url()))?;

        let total = response.content_length().filter(|&n| n > 0).unwrap_or(asset.size);
        let mut file =
            File::create(&part_path).map_err(|e| format!("Could not create file: {e}"))?;
        let mut hasher = Sha256::new();
        let mut done: u64 = 0;
        let mut last_emit = Instant::now();

        on_event(DownloadEvent::Progress { done: 0, total });
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("Download interrupted: {}", e.without_url()))?
        {
            if cancel.load(Ordering::Relaxed) {
                return Err("Update download cancelled".to_string());
            }
            file.write_all(&chunk).map_err(|e| format!("Could not write update file: {e}"))?;
            hasher.update(&chunk);
            done = done.saturating_add(chunk.len() as u64);
            if last_emit.elapsed() >= PROGRESS_EMIT_INTERVAL {
                on_event(DownloadEvent::Progress { done, total });
                last_emit = Instant::now();
            }
        }
        on_event(DownloadEvent::Progress { done, total: total.max(done) });
        file.flush().map_err(|e| format!("Could not flush update file: {e}"))?;
        let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Ok::<(String, String), String>((actual, expected))
    })?;

    if actual_sha256 != expected_sha256 {
        let _ = fs::remove_file(&part_path);
        return Err("Downloaded update failed SHA-256 verification".to_string());
    }

    fs::rename(&part_path, &archive_path)
        .map_err(|e| format!("Could not finalize downloaded package: {e}"))?;
    on_event(DownloadEvent::Installing);
    let result = install_package(&archive_path, &dir);
    let _ = fs::remove_file(&archive_path);
    result
}

async fn fetch_sha256(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Could not fetch checksum: {}", e.without_url()))?
        .error_for_status()
        .map_err(|e| format!("Checksum fetch failed: {}", e.without_url()))?;
    let body =
        response.text().await.map_err(|e| format!("Could not read checksum response: {e}"))?;
    parse_sha256_text(&body)
        .ok_or_else(|| "Release checksum file did not contain a valid SHA-256 digest".to_string())
}

fn install_package(archive: &Path, work_dir: &Path) -> Result<PathBuf, String> {
    let (os, _) = platform_target();
    match os {
        "macos" => install_macos_dmg(archive, work_dir),
        "linux" => install_linux_tar_gz(archive, work_dir),
        "windows" => install_windows_zip(archive, work_dir),
        other => Err(format!("In-app updates are not supported on {other}")),
    }
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct MountGuard {
    mount_point: PathBuf,
    mounted: bool,
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        if self.mounted {
            let status =
                aether_core::command("hdiutil").arg("detach").arg(&self.mount_point).status();
            if !status.is_ok_and(|s| s.success()) {
                let _ = aether_core::command("hdiutil")
                    .args(["detach", "-force"])
                    .arg(&self.mount_point)
                    .status();
            }
        }
    }
}

fn install_macos_dmg(dmg: &Path, work_dir: &Path) -> Result<PathBuf, String> {
    let pid = std::process::id();
    let mount_dir = TempDirGuard(work_dir.join(format!("aether-dmg-{pid}")));
    let _ = fs::remove_dir_all(&mount_dir.0);
    fs::create_dir_all(&mount_dir.0)
        .map_err(|e| format!("Could not create DMG mount directory: {e}"))?;

    let attach = aether_core::command("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"])
        .arg(&mount_dir.0)
        .arg(dmg)
        .output()
        .map_err(|e| format!("Could not run hdiutil attach: {e}"))?;
    if !attach.status.success() {
        let stderr = String::from_utf8_lossy(&attach.stderr);
        return Err(format!("Failed to open update disk image: {}", stderr.trim()));
    }
    let mut mount_guard = MountGuard { mount_point: mount_dir.0.clone(), mounted: true };

    let source_app = mount_dir.0.join("Aether.app");
    if !source_app.is_dir() {
        return Err("Update disk image does not contain Aether.app".to_string());
    }

    let target_app = macos_target_app_path()?;
    let target_parent =
        target_app.parent().ok_or_else(|| "Invalid macOS install location".to_string())?;
    fs::create_dir_all(target_parent)
        .map_err(|e| format!("Could not prepare {}: {e}", target_parent.display()))?;

    let staged_app = target_parent.join(format!(".Aether-update-{pid}.app"));
    let backup_app = target_parent.join(format!(".Aether-old-{pid}.app"));
    let _ = fs::remove_dir_all(&staged_app);
    let _ = fs::remove_dir_all(&backup_app);

    let ditto = aether_core::command("ditto")
        .arg(&source_app)
        .arg(&staged_app)
        .output()
        .map_err(|e| format!("Could not copy updated app bundle: {e}"))?;
    if !ditto.status.success() {
        let _ = fs::remove_dir_all(&staged_app);
        let stderr = String::from_utf8_lossy(&ditto.stderr);
        return Err(format!("Failed to copy Aether.app: {}", stderr.trim()));
    }

    let _ = aether_core::command("hdiutil").arg("detach").arg(&mount_dir.0).status().map(|s| {
        if s.success() {
            mount_guard.mounted = false;
        }
    });

    let _ = aether_core::command("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&staged_app)
        .status();

    let had_existing = target_app.exists();
    if had_existing && let Err(e) = fs::rename(&target_app, &backup_app) {
        let _ = fs::remove_dir_all(&staged_app);
        return Err(format!("Could not replace existing app at {}: {e}", target_app.display()));
    }

    if let Err(e) = fs::rename(&staged_app, &target_app) {
        if had_existing {
            let _ = fs::rename(&backup_app, &target_app);
        }
        let _ = fs::remove_dir_all(&staged_app);
        return Err(format!("Could not install update to {}: {e}", target_app.display()));
    }

    if had_existing {
        let _ = fs::remove_dir_all(&backup_app);
    }

    Ok(target_app)
}

fn macos_target_app_path() -> Result<PathBuf, String> {
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            if ancestor.extension().is_some_and(|e| e == "app")
                && ancestor.file_name().is_some_and(|n| n != "Aether Dev.app")
            {
                return Ok(ancestor.to_path_buf());
            }
        }
    }
    let system_apps = Path::new("/Applications");
    let probe = system_apps.join(format!(".aether-write-probe-{}", std::process::id()));
    if fs::OpenOptions::new().write(true).create_new(true).open(&probe).is_ok() {
        let _ = fs::remove_file(&probe);
        return Ok(system_apps.join("Aether.app"));
    }
    Ok(aether_core::home_dir().join("Applications/Aether.app"))
}

fn install_linux_tar_gz(archive: &Path, work_dir: &Path) -> Result<PathBuf, String> {
    let pid = std::process::id();
    let extract_dir = TempDirGuard(work_dir.join(format!("aether-extract-{pid}")));
    let _ = fs::remove_dir_all(&extract_dir.0);
    fs::create_dir_all(&extract_dir.0)
        .map_err(|e| format!("Could not create extract directory: {e}"))?;

    let tar = aether_core::command("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&extract_dir.0)
        .output()
        .map_err(|e| format!("Could not run tar: {e}"))?;
    if !tar.status.success() {
        let stderr = String::from_utf8_lossy(&tar.stderr);
        return Err(format!("Failed to unpack update archive: {}", stderr.trim()));
    }

    let extracted = find_file_named(&extract_dir.0, "aether", 4)
        .ok_or_else(|| "Update archive did not contain the aether binary".to_string())?;
    let exe =
        std::env::current_exe().map_err(|e| format!("Could not locate current executable: {e}"))?;
    let exe_dir = exe.parent().ok_or_else(|| "Invalid current executable path".to_string())?;
    let staged = exe_dir.join(format!(".aether-update-{pid}"));

    if let Err(e) = fs::copy(&extracted, &staged) {
        return Err(format!("Cannot write update into {}: {e}", exe_dir.display()));
    }
    set_executable_permissions(&staged);
    if let Err(e) = fs::rename(&staged, &exe) {
        let _ = fs::remove_file(&staged);
        return Err(format!("Cannot replace binary {}: {e}", exe.display()));
    }
    Ok(exe)
}

#[cfg(unix)]
fn set_executable_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn set_executable_permissions(_path: &Path) {}

fn install_windows_zip(archive: &Path, work_dir: &Path) -> Result<PathBuf, String> {
    let pid = std::process::id();
    let extract_dir = TempDirGuard(work_dir.join(format!("aether-extract-{pid}")));
    let _ = fs::remove_dir_all(&extract_dir.0);
    fs::create_dir_all(&extract_dir.0)
        .map_err(|e| format!("Could not create extract directory: {e}"))?;

    let tar = aether_core::command("tar.exe")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(&extract_dir.0)
        .output()
        .map_err(|e| format!("Could not run tar.exe: {e}"))?;
    if !tar.status.success() {
        let stderr = String::from_utf8_lossy(&tar.stderr);
        return Err(format!("Failed to unpack update archive: {}", stderr.trim()));
    }

    let extracted = find_file_named(&extract_dir.0, "aether.exe", 4)
        .ok_or_else(|| "Update archive did not contain aether.exe".to_string())?;
    let exe =
        std::env::current_exe().map_err(|e| format!("Could not locate current executable: {e}"))?;
    let exe_dir = exe.parent().ok_or_else(|| "Invalid current executable path".to_string())?;
    let staged = exe_dir.join(format!(".aether-update-{pid}.exe"));

    if let Err(e) = fs::copy(&extracted, &staged) {
        return Err(format!("Cannot write update into {}: {e}", exe_dir.display()));
    }

    let mut backup_os = exe.as_os_str().to_os_string();
    backup_os.push(".old");
    let backup = PathBuf::from(backup_os);
    let _ = fs::remove_file(&backup);

    if let Err(e) = fs::rename(&exe, &backup) {
        let _ = fs::remove_file(&staged);
        return Err(format!("Could not prepare executable for replacement: {e}"));
    }
    if let Err(e) = fs::rename(&staged, &exe) {
        let _ = fs::rename(&backup, &exe);
        let _ = fs::remove_file(&staged);
        return Err(format!("Could not install updated executable: {e}"));
    }
    Ok(exe)
}

fn find_file_named(root: &Path, name: &str, max_depth: usize) -> Option<PathBuf> {
    if max_depth == 0 {
        return None;
    }
    let entries = fs::read_dir(root).ok()?;
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
        if path.is_dir() {
            dirs.push(path);
        }
    }
    for dir in dirs {
        if let Some(found) = find_file_named(&dir, name, max_depth - 1) {
            return Some(found);
        }
    }
    None
}

fn format_bytes_short(bytes: u64) -> String {
    const MB: f64 = (1024 * 1024) as f64;
    const KB: f64 = 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{:.0} KB", bytes as f64 / KB)
    }
}

pub(crate) fn update_percent(done: u64, total: u64) -> Option<u8> {
    if total == 0 {
        return None;
    }
    Some(((done.min(total) as f64 / total as f64) * 100.0).round() as u8)
}

impl ChatApp {
    pub(crate) fn check_for_updates(
        &mut self,
        manual: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.update_state {
            UpdateState::Downloading { .. } | UpdateState::Installing { .. } => {
                if manual {
                    window.push_notification(
                        Notification::info("An Aether update is already being installed."),
                        cx,
                    );
                }
                return;
            }
            UpdateState::ReadyToRestart { release, .. } => {
                if manual {
                    window.push_notification(
                        Notification::info(format!(
                            "Aether {} is installed. Restart to finish updating.",
                            release.display_version
                        )),
                        cx,
                    );
                }
                return;
            }
            UpdateState::Checking if !manual => return,
            _ => {}
        }

        self.update_check_run = self.update_check_run.wrapping_add(1);
        let run = self.update_check_run;
        if manual || matches!(self.update_state, UpdateState::Idle | UpdateState::Failed { .. }) {
            self.update_state = UpdateState::Checking;
            cx.notify();
        }

        let current = current_version().to_string();
        let (os, arch) = platform_target();
        let check = cx.background_spawn(async move {
            let releases = fetch_releases()?;
            check_outcome(&current, &releases, os, arch)
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = check.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.update_check_run != run {
                    return;
                }
                match result {
                    Ok(Some(release)) => {
                        let version = release.display_version.clone();
                        this.update_state = UpdateState::Available(release);
                        cx.notify();
                        if manual {
                            window.push_notification(
                                Notification::info(format!("Aether {version} is available."))
                                    .title("Update available"),
                                cx,
                            );
                        }
                    }
                    Ok(None) => {
                        this.update_state =
                            UpdateState::UpToDate { version: current_version().to_string() };
                        cx.notify();
                        if manual {
                            window.push_notification(
                                Notification::info(format!(
                                    "Aether {} is up to date.",
                                    current_version()
                                ))
                                .title("Up to date"),
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        if manual {
                            this.update_state =
                                UpdateState::Failed { release: None, error: error.clone() };
                            cx.notify();
                            window.push_notification(
                                Notification::warning(error).title("Update check failed"),
                                cx,
                            );
                        } else {
                            if matches!(this.update_state, UpdateState::Checking) {
                                this.update_state = UpdateState::Idle;
                                cx.notify();
                            }
                            eprintln!("aether: background update check failed: {error}");
                        }
                    }
                }
            });
        })
        .detach();
    }

    pub(crate) fn start_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let release = match &self.update_state {
            UpdateState::Available(release) => release.clone(),
            UpdateState::Failed { release: Some(release), .. } => release.clone(),
            _ => return,
        };
        let Some(asset) = release.asset.clone() else {
            cx.open_url(&release.html_url);
            return;
        };

        self.update_install_run = self.update_install_run.wrapping_add(1);
        let install_run = self.update_install_run;
        let cancel = Arc::new(AtomicBool::new(false));
        self.update_state = UpdateState::Downloading {
            release: release.clone(),
            done: 0,
            total: asset.size,
            cancel: cancel.clone(),
        };
        cx.notify();

        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let worker_release = release.clone();
        let worker_cancel = cancel.clone();
        let job = cx.background_spawn(async move {
            download_and_install(&worker_release, &worker_cancel, |ev| {
                tx.unbounded_send(ev).ok();
            })
        });

        cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = rx.next().await {
                let _ = this.update(cx, |this, cx| {
                    if this.update_install_run != install_run {
                        return;
                    }
                    match ev {
                        DownloadEvent::Progress { done, total } => {
                            if let UpdateState::Downloading {
                                done: shown_done,
                                total: shown_total,
                                ..
                            } = &mut this.update_state
                            {
                                *shown_done = done;
                                *shown_total = total;
                                cx.notify();
                            }
                        }
                        DownloadEvent::Installing => {
                            if let UpdateState::Downloading { release, .. } = &this.update_state {
                                this.update_state =
                                    UpdateState::Installing { release: release.clone() };
                                cx.notify();
                            }
                        }
                    }
                });
            }

            let outcome = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.update_install_run != install_run || cancel.load(Ordering::Relaxed) {
                    return;
                }
                match outcome {
                    Ok(target) => {
                        let version = release.display_version.clone();
                        this.update_state = UpdateState::ReadyToRestart { release, target };
                        cx.notify();
                        window.push_notification(
                            Notification::info(format!(
                                "Aether {version} is ready. Restart to finish updating."
                            ))
                            .title("Update ready"),
                            cx,
                        );
                    }
                    Err(error) => {
                        this.update_state =
                            UpdateState::Failed { release: Some(release), error: error.clone() };
                        cx.notify();
                        window.push_notification(
                            Notification::error(error).title("Update failed"),
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    }

    pub(crate) fn cancel_update(&mut self, cx: &mut Context<Self>) {
        if let UpdateState::Downloading { release, cancel, .. } = &self.update_state {
            cancel.store(true, Ordering::Relaxed);
            self.update_install_run = self.update_install_run.wrapping_add(1);
            self.update_state = UpdateState::Available(release.clone());
            cx.notify();
        }
    }

    pub(crate) fn restart_for_update(&mut self, cx: &mut Context<Self>) {
        if let UpdateState::ReadyToRestart { target, .. } = &self.update_state {
            cx.set_restart_path(target.clone());
            cx.restart();
        }
    }

    pub(crate) fn update_status_summary(&self) -> String {
        let current = current_version();
        match &self.update_state {
            UpdateState::Idle => format!("Current version: {current}"),
            UpdateState::Checking => format!("Current version: {current} · Checking for updates…"),
            UpdateState::UpToDate { version } => {
                format!("Aether {version} is the latest version available.")
            }
            UpdateState::Available(release) => {
                if let Some(asset) = &release.asset {
                    format!(
                        "Aether {} is available ({}, current: {current}).",
                        release.display_version,
                        format_bytes_short(asset.size),
                    )
                } else {
                    format!(
                        "Aether {} is available on GitHub Releases (current: {current}).",
                        release.display_version,
                    )
                }
            }
            UpdateState::Downloading { release, done, total, .. } => {
                match update_percent(*done, *total) {
                    Some(pct) => {
                        format!("Downloading Aether {}… {pct}%", release.display_version)
                    }
                    None => format!(
                        "Downloading Aether {} ({})…",
                        release.display_version,
                        format_bytes_short(*done),
                    ),
                }
            }
            UpdateState::Installing { release } => {
                format!("Verifying and installing Aether {}…", release.display_version)
            }
            UpdateState::ReadyToRestart { release, .. } => {
                format!(
                    "Aether {} is installed. Restart Aether to launch the new version.",
                    release.display_version
                )
            }
            UpdateState::Failed { error, .. } => format!("Current version: {current} · {error}"),
        }
    }

    pub(crate) fn render_update_settings_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let release_notes_url = match &self.update_state {
            UpdateState::Available(r)
            | UpdateState::Downloading { release: r, .. }
            | UpdateState::Installing { release: r }
            | UpdateState::ReadyToRestart { release: r, .. } => r.html_url.clone(),
            UpdateState::Failed { release: Some(r), .. } => r.html_url.clone(),
            _ => RELEASES_PAGE_URL.to_string(),
        };

        let releases_btn = aether_ui::button("update-releases-link")
            .debug_selector(|| "update-releases-link".into())
            .ghost()
            .small()
            .label("Release notes")
            .accessibility_label("Open Aether releases on GitHub")
            .on_click(move |_, _, cx| cx.open_url(&release_notes_url));

        let primary = match &self.update_state {
            UpdateState::Idle
            | UpdateState::UpToDate { .. }
            | UpdateState::Failed { release: None, .. } => aether_ui::button("update-check")
                .debug_selector(|| "update-check".into())
                .ghost()
                .small()
                .label("Check for updates")
                .accessibility_label("Check for updates")
                .on_click(
                    cx.listener(|this, _, window, cx| this.check_for_updates(true, window, cx)),
                )
                .into_any_element(),
            UpdateState::Checking => aether_ui::button("update-checking")
                .debug_selector(|| "update-checking".into())
                .ghost()
                .small()
                .loading(true)
                .disabled(true)
                .label("Checking…")
                .accessibility_label("Checking for updates")
                .into_any_element(),
            UpdateState::Available(release)
            | UpdateState::Failed { release: Some(release), .. } => {
                let label = if release.asset.is_some() {
                    format!("Update to {}", release.display_version)
                } else {
                    format!("Download {}", release.display_version)
                };
                aether_ui::button("update-install")
                    .debug_selector(|| "update-install".into())
                    .primary()
                    .small()
                    .label(label.clone())
                    .accessibility_label(label)
                    .on_click(cx.listener(|this, _, window, cx| this.start_update(window, cx)))
                    .into_any_element()
            }
            UpdateState::Downloading { .. } => aether_ui::button("update-cancel")
                .debug_selector(|| "update-cancel".into())
                .ghost()
                .small()
                .label("Cancel")
                .accessibility_label("Cancel update download")
                .on_click(cx.listener(|this, _, _, cx| this.cancel_update(cx)))
                .into_any_element(),
            UpdateState::Installing { .. } => aether_ui::button("update-installing")
                .debug_selector(|| "update-installing".into())
                .ghost()
                .small()
                .loading(true)
                .disabled(true)
                .label("Installing…")
                .accessibility_label("Installing update")
                .into_any_element(),
            UpdateState::ReadyToRestart { .. } => aether_ui::button("update-restart")
                .debug_selector(|| "update-restart".into())
                .primary()
                .small()
                .label("Restart to update")
                .accessibility_label("Restart to finish updating Aether")
                .on_click(cx.listener(|this, _, _, cx| this.restart_for_update(cx)))
                .into_any_element(),
        };

        h_flex().items_center().gap_2().child(primary).child(releases_btn).into_any_element()
    }
}

#[cfg(test)]
#[path = "../tests/unit/updater.rs"]
mod tests;
