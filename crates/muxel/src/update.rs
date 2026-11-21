//! In-app update support.
//!
//! Checks the GitHub Releases API for a newer version and, for install formats
//! the app can replace in place (AppImage, the Linux portable binary, the
//! Windows portable `.zip`, and the macOS `.app` bundle), downloads and applies
//! the update. Package-managed installs (Flatpak / `.deb` / `.rpm`) live in
//! read-only system paths, so for those we just surface the right upgrade
//! command instead of trying to self-replace.
//!
//! All network/disk work here is blocking; callers run it off the UI thread via
//! `cx.background_executor().spawn(..)`.

use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(windows)]
use std::{fs, os::windows::fs::OpenOptionsExt as _};
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

/// The running app's version, baked in at compile time.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

const LATEST_API: &str = "https://api.github.com/repos/projecthax/muxel/releases/latest";
/// Public releases page, shown/opened when a self-update isn't possible.
pub const RELEASES_URL: &str = "https://github.com/projecthax/muxel/releases";

/// How muxel was installed — determines whether it can replace itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// A single-file AppImage (the `APPIMAGE` env var points at the file).
    AppImage,
    /// Installed by `.deb`/`.rpm` at `/usr/bin/muxel` (root-owned).
    SystemPackage,
    /// A macOS `.app` bundle.
    MacOsApp,
    /// A loose `muxel.exe` from the portable Windows `.zip`.
    WindowsPortable,
    /// A loose Linux binary from the portable `.tar.gz` (or `install.sh`).
    LinuxPortable,
}

/// The `.AppImage` file this muxel is running from, if it is — `$APPIMAGE`, but
/// only while this executable is inside that AppImage's mount. The variable alone
/// is inherited from any AppImage up the process tree (see
/// `muxel_core::own_appimage`).
pub fn running_appimage() -> Option<PathBuf> {
    let appimage = std::env::var("APPIMAGE").ok();
    let appdir = std::env::var("APPDIR").ok();
    let exe = std::env::current_exe().ok()?;
    muxel_core::own_appimage(appimage.as_deref(), appdir.as_deref(), &exe).or_else(|| {
        // `current_exe` is fully resolved; the runtime's `$APPDIR` may not be.
        let real = std::fs::canonicalize(appdir.as_deref()?).ok()?;
        muxel_core::own_appimage(appimage.as_deref(), real.to_str(), &exe)
    })
}

impl InstallKind {
    /// Detect the install format from environment + the executable path.
    pub fn detect() -> InstallKind {
        if running_appimage().is_some() {
            return InstallKind::AppImage;
        }
        let exe = std::env::current_exe().unwrap_or_default();
        if cfg!(target_os = "macos") && exe.to_string_lossy().contains(".app/Contents/MacOS/") {
            return InstallKind::MacOsApp;
        }
        if cfg!(target_os = "windows") {
            return InstallKind::WindowsPortable;
        }
        // Only the distro-package path is read-only/root-owned; `/usr/local/bin`
        // and `~/.local/bin` (install.sh targets) are user-writable → portable.
        if exe == Path::new("/usr/bin/muxel") {
            return InstallKind::SystemPackage;
        }
        InstallKind::LinuxPortable
    }

    /// Whether the app can download a release and replace itself in place.
    pub fn self_updatable(self) -> bool {
        matches!(
            self,
            InstallKind::AppImage
                | InstallKind::MacOsApp
                | InstallKind::WindowsPortable
                | InstallKind::LinuxPortable
        )
    }

    /// For package-managed installs, the command the user should run instead.
    pub fn upgrade_hint(self) -> Option<&'static str> {
        match self {
            InstallKind::SystemPackage => Some(
                "sudo apt install --only-upgrade muxel    # Debian/Ubuntu\nsudo dnf upgrade muxel                     # Fedora/RHEL",
            ),
            _ => None,
        }
    }
}

/// A newer release found on GitHub.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    /// Version without a leading `v` (e.g. `0.2.0`).
    pub version: String,
    /// Release notes (the GitHub release body; may be empty).
    pub notes: String,
    /// `(asset name, download url)` pairs attached to the release.
    pub assets: Vec<(String, String)>,
}

/// What to run after an update is staged, to start the new version.
#[derive(Clone, Debug)]
pub struct RelaunchPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Compare dotted numeric versions; `true` iff `latest` is strictly newer than
/// `current`. A leading `v` and any `-pre`/`+build` suffix are ignored; any
/// non-numeric component makes the comparison conservatively return `false`.
fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        let v = v.trim().trim_start_matches('v');
        let core = v.split(['-', '+']).next().unwrap_or(v);
        core.split('.').map(|p| p.parse::<u64>().ok()).collect()
    }
    match (parts(latest), parts(current)) {
        (Some(a), Some(b)) => {
            let n = a.len().max(b.len());
            for i in 0..n {
                let x = a.get(i).copied().unwrap_or(0);
                let y = b.get(i).copied().unwrap_or(0);
                if x != y {
                    return x > y;
                }
            }
            false
        }
        _ => false,
    }
}

/// Query GitHub for the latest release; `Ok(None)` when already up to date (or
/// there are no releases / the repo is private → 404).
pub fn fetch_latest() -> Result<Option<UpdateInfo>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let resp = match agent
        .get(LATEST_API)
        .set("User-Agent", &format!("muxel/{APP_VERSION}"))
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(r) => r,
        // No published release yet (or private repo): treat as "up to date".
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(e) => return Err(anyhow!("update check failed: {e}")),
    };

    let json: serde_json::Value = resp.into_json().context("parsing release JSON")?;
    let tag = json.get("tag_name").and_then(|t| t.as_str()).unwrap_or("");
    if tag.is_empty() || !is_newer(tag, APP_VERSION) {
        return Ok(None);
    }
    let notes = json
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string();
    let assets = json
        .get("assets")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let name = a.get("name")?.as_str()?.to_string();
                    let url = a.get("browser_download_url")?.as_str()?.to_string();
                    Some((name, url))
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(Some(UpdateInfo {
        version: tag.trim_start_matches('v').to_string(),
        notes,
        assets,
    }))
}

/// The release asset to download for a given install kind, if self-updatable.
pub fn asset_for(kind: InstallKind, assets: &[(String, String)]) -> Option<&(String, String)> {
    let name = match kind {
        InstallKind::AppImage | InstallKind::LinuxPortable => {
            if cfg!(target_arch = "aarch64") {
                "muxel-linux-aarch64.AppImage"
            } else {
                "muxel-linux-x86_64.AppImage"
            }
        }
        InstallKind::WindowsPortable => {
            if cfg!(target_arch = "aarch64") {
                "muxel-windows-arm64.zip"
            } else {
                "muxel-windows-x86_64.zip"
            }
        }
        // macOS ships one universal (x86_64 + aarch64) .app, not per-arch builds.
        InstallKind::MacOsApp => "muxel-macos-universal.zip",
        InstallKind::SystemPackage => return None,
    };
    assets.iter().find(|(n, _)| n == name)
}

/// Download `asset_url` and apply it for `kind`, returning how to relaunch.
pub fn download_and_apply(kind: InstallKind, url: &str, version: &str) -> Result<RelaunchPlan> {
    let work = std::env::temp_dir().join(format!("muxel-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).context("creating update temp dir")?;

    let result = match kind {
        InstallKind::AppImage | InstallKind::LinuxPortable => apply_linux(url, &work),
        InstallKind::WindowsPortable => apply_windows(url, &work, version),
        InstallKind::MacOsApp => apply_macos(url, &work),
        InstallKind::SystemPackage => {
            bail!("this install updates via the system package manager")
        }
    };
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&work);
    }
    result
}

/// Start the freshly-installed version and exit the current process.
pub fn relaunch_and_exit(plan: &RelaunchPlan) -> ! {
    let _ = muxel_core::child_env::command(&plan.program)
        .args(&plan.args)
        .spawn();
    std::process::exit(0);
}

// ----------------------------------------------------------------------------
// platform apply steps
// ----------------------------------------------------------------------------

fn apply_linux(url: &str, work: &Path) -> Result<RelaunchPlan> {
    let dl = work.join("muxel.new");
    download_to(url, &dl)?;
    make_executable(&dl)?;
    // AppImage: replace the .AppImage file itself (current_exe is inside the
    // read-only FUSE mount). Portable: replace the running binary directly.
    if let Some(appimage) = running_appimage() {
        overwrite_in_place(&dl, &appimage)?;
        Ok(RelaunchPlan {
            program: appimage,
            args: vec![],
        })
    } else {
        let exe = std::env::current_exe().context("resolving current_exe")?;
        self_replace::self_replace(&dl).context("replacing the muxel binary")?;
        Ok(RelaunchPlan {
            program: exe,
            args: vec![],
        })
    }
}

fn apply_windows(
    url: &str,
    work: &Path,
    #[cfg_attr(not(windows), allow(unused_variables))] version: &str,
) -> Result<RelaunchPlan> {
    let zip = work.join("muxel.zip");
    download_to(url, &zip)?;
    let new_exe = extract_windows_exe(&zip, work)?;
    let exe = std::env::current_exe().context("resolving current_exe")?;
    #[cfg(windows)]
    if let Some(install) = exe.parent()
        && let Err(error) = stage_windows_console(&zip, install, version)
    {
        log::warn!("Windows console host update skipped: {error:#}");
    }
    self_replace::self_replace(&new_exe).context("replacing muxel.exe")?;
    Ok(RelaunchPlan {
        program: exe,
        args: vec![],
    })
}

#[cfg(windows)]
const CONSOLE_FOLDER: &str = "muxel-console";
#[cfg(windows)]
const CONSOLE_FILES: [&str; 3] = ["conpty.dll", "OpenConsole.exe", "CONPTY-LICENSE.txt"];

#[cfg(windows)]
fn stage_windows_console(zip: &Path, install: &Path, version: &str) -> Result<()> {
    if !version.starts_with(|c: char| c.is_ascii_digit())
        || version.ends_with('.')
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    {
        bail!("invalid Windows console host version {version}");
    }
    let mut archive = zip::ZipArchive::new(fs::File::open(zip)?)?;
    let names = CONSOLE_FILES.map(|file| format!("{CONSOLE_FOLDER}/{version}/{file}"));
    if names
        .iter()
        .all(|name| archive.index_for_name(name).is_none())
    {
        return Ok(());
    }
    let root = install.join(CONSOLE_FOLDER);
    let dest = root.join(version);
    let staging = root.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root)?;
    fs::create_dir(&staging)?;
    let result = (|| {
        for (name, file) in names.iter().zip(CONSOLE_FILES) {
            extract_zip_entry(&mut archive, name, &staging.join(file))?;
        }
        publish_windows_console(&staging, &dest).context("publishing Windows console host")
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

#[cfg(windows)]
fn publish_windows_console(staging: &Path, dest: &Path) -> std::io::Result<()> {
    let publish = || {
        fs::rename(staging, dest).or_else(|error| {
            if console_files_match(staging, dest) {
                Ok(())
            } else {
                Err(error)
            }
        })
    };
    if publish().is_ok() {
        return Ok(());
    }
    let guard = match open_console_for_removal(dest) {
        Ok(guard) => guard,
        // Another updater can move aside a stale directory before publishing.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return publish(),
        Err(error) => {
            return if console_files_match(staging, dest) {
                Ok(())
            } else {
                Err(error)
            };
        }
    };
    if console_files_match(staging, dest) {
        return Ok(());
    }
    let retired = staging.with_extension("old");
    fs::rename(dest, &retired)?;
    drop(guard);
    if let Err(error) = publish() {
        let _ = fs::rename(&retired, dest);
        return Err(error);
    }
    let _ = fs::remove_dir_all(retired);
    Ok(())
}

#[cfg(windows)]
fn console_files_match(staging: &Path, dest: &Path) -> bool {
    use sha2::{Digest as _, Sha256};

    CONSOLE_FILES.iter().all(|file| {
        let compare = || -> std::io::Result<bool> {
            let mut expected = fs::File::open(staging.join(file))?;
            let mut actual = fs::File::open(dest.join(file))?;
            if expected.metadata()?.len() != actual.metadata()?.len() {
                return Ok(false);
            }
            let mut expected_hash = Sha256::new();
            let mut actual_hash = Sha256::new();
            std::io::copy(&mut expected, &mut expected_hash)?;
            std::io::copy(&mut actual, &mut actual_hash)?;
            Ok(expected_hash.finalize() == actual_hash.finalize())
        };
        compare().unwrap_or(false)
    })
}

#[cfg(windows)]
fn open_console_directory(directory: &Path) -> std::io::Result<std::fs::File> {
    fs::File::options()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(directory)
}

#[cfg(windows)]
fn open_console_for_removal(directory: &Path) -> std::io::Result<std::fs::File> {
    use windows::Win32::Foundation::GENERIC_WRITE;

    // DELETE conflicts with a running app. The write access, without write
    // sharing, excludes other removers while allowing rename and deletion.
    fs::File::options()
        .access_mode(DELETE.0 | GENERIC_WRITE.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(directory)
}

#[cfg(windows)]
pub fn prepare_windows_console() -> Option<std::fs::File> {
    let exe = std::env::current_exe().ok()?;
    let root = exe.parent()?.join(CONSOLE_FOLDER);
    let directory = root.join(APP_VERSION);
    let directory_handle = open_console_directory(&directory)
        .inspect_err(|error| {
            log::debug!(
                "Skipping Windows console bundle at {}: {error}",
                directory.display()
            )
        })
        .ok();
    if directory_handle.is_some() {
        muxel_terminal::set_console_directory(directory);
    }
    prune_windows_consoles(&root, APP_VERSION);
    directory_handle
}

#[cfg(windows)]
fn prune_windows_consoles(root: &Path, current: &str) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !is_newer(current, &entry.file_name().to_string_lossy())
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let path = entry.path();
        if let Ok(_guard) = open_console_for_removal(&path) {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn apply_macos(url: &str, work: &Path) -> Result<RelaunchPlan> {
    let zip = work.join("muxel.zip");
    download_to(url, &zip)?;
    let extracted = work.join("extracted");
    std::fs::create_dir_all(&extracted)?;
    extract_all(&zip, &extracted)?;
    let new_app = extracted.join("muxel.app");
    if !new_app.exists() {
        bail!("downloaded archive did not contain muxel.app");
    }

    let exe = std::env::current_exe().context("resolving current_exe")?;
    let bundle = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .context("could not locate the running .app bundle")?
        .to_path_buf();

    // Make sure the new binary is executable, then swap the bundle: move the old
    // one aside, move the new one in, and roll back if the move-in fails.
    make_executable(&new_app.join("Contents/MacOS/muxel"))?;
    let backup = bundle.with_extension("app.bak");
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(&bundle, &backup)
        .with_context(|| format!("moving aside {} (is it writable?)", bundle.display()))?;
    if let Err(e) = move_dir(&new_app, &bundle) {
        let _ = std::fs::rename(&backup, &bundle);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&backup);

    // Clear quarantine so Gatekeeper doesn't block the relaunch (best-effort).
    let _ = muxel_core::child_env::command("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&bundle)
        .status();

    Ok(RelaunchPlan {
        program: PathBuf::from("/usr/bin/open"),
        args: vec!["-n".into(), bundle.to_string_lossy().into_owned()],
    })
}

// ----------------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------------

fn download_to(url: &str, dest: &Path) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .build();
    let resp = agent
        .get(url)
        .set("User-Agent", &format!("muxel/{APP_VERSION}"))
        .call()
        .map_err(|e| anyhow!("download failed: {e}"))?;
    let mut reader = resp.into_reader();
    let mut file =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    std::io::copy(&mut reader, &mut file).context("writing downloaded file")?;
    Ok(())
}

/// Atomically replace `target` with the contents of `new` (stage in the target's
/// own directory, then rename over it).
fn overwrite_in_place(new: &Path, target: &Path) -> Result<()> {
    let dir = target.parent().context("target has no parent directory")?;
    let staged = dir.join(format!(".muxel-update-{}", std::process::id()));
    std::fs::copy(new, &staged).with_context(|| format!("staging into {}", dir.display()))?;
    make_executable(&staged)?;
    std::fs::rename(&staged, target).with_context(|| {
        let _ = std::fs::remove_file(&staged);
        format!("replacing {}", target.display())
    })?;
    Ok(())
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Move a directory tree, falling back to copy+remove across filesystems.
fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to)?;
    let _ = std::fs::remove_dir_all(from);
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn extract_windows_exe(zip_path: &Path, out_dir: &Path) -> Result<PathBuf> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).context("opening zip")?;
    let dest = out_dir.join("muxel.exe");
    extract_zip_entry(&mut archive, "muxel.exe", &dest)?;
    Ok(dest)
}

fn extract_zip_entry(
    archive: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
    dest: &Path,
) -> Result<()> {
    let mut entry = archive
        .by_name(name)
        .with_context(|| format!("opening ZIP entry {name}"))?;
    anyhow::ensure!(entry.is_file(), "not a file in update ZIP: {name}");
    std::io::copy(&mut entry, &mut std::fs::File::create(dest)?)?;
    Ok(())
}

fn extract_all(zip_path: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).context("opening zip")?;
    archive.extract(dest).context("extracting zip")?;
    Ok(())
}

// ----------------------------------------------------------------------------
// tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    mod windows_console {
        use super::{
            extract_windows_exe, open_console_directory, prune_windows_consoles,
            stage_windows_console,
        };
        use std::fs;
        use std::io::Write as _;
        use std::path::{Path, PathBuf};
        use zip::result::ZipError;
        use zip::unstable::write::FileOptionsExt as _;
        use zip::write::SimpleFileOptions;

        const FILES: [(&str, &[u8]); 3] = [
            ("conpty.dll", b"console DLL"),
            ("OpenConsole.exe", b"console host"),
            ("CONPTY-LICENSE.txt", b"licence"),
        ];

        struct Fixture(PathBuf);

        impl Fixture {
            fn new() -> Self {
                let path = std::env::temp_dir()
                    .join(format!("muxel-update-test-{}", uuid::Uuid::new_v4()));
                fs::create_dir(&path).unwrap();
                Self(path)
            }

            fn zip(&self, version: &str, files: &[(&str, &[u8])]) -> PathBuf {
                let zip = self.0.join(format!("{version}.zip"));
                let mut archive = zip::ZipWriter::new(fs::File::create(&zip).unwrap());
                archive
                    .start_file("muxel.exe", SimpleFileOptions::default())
                    .unwrap();
                archive.write_all(b"updated app").unwrap();
                for (name, bytes) in files {
                    archive
                        .start_file(
                            format!("muxel-console/{version}/{name}"),
                            SimpleFileOptions::default(),
                        )
                        .unwrap();
                    archive.write_all(bytes).unwrap();
                }
                archive.finish().unwrap();
                zip
            }

            fn console(&self, version: &str) -> PathBuf {
                self.0.join("muxel-console").join(version)
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        fn assert_console(directory: &Path) {
            for (name, bytes) in FILES {
                assert_eq!(fs::read(directory.join(name)).unwrap(), bytes);
            }
        }

        #[test]
        fn update_delivers_the_complete_console_folder() {
            let fixture = Fixture::new();
            let zip = fixture.zip("2.0.0", &FILES);
            stage_windows_console(&zip, &fixture.0, "2.0.0").unwrap();
            assert_console(&fixture.console("2.0.0"));
        }

        #[test]
        fn older_zip_can_update_without_console_files() {
            let fixture = Fixture::new();
            let zip = fixture.zip("1.0.0", &[]);
            stage_windows_console(&zip, &fixture.0, "1.0.0").unwrap();
            assert!(!fixture.0.join("muxel-console").exists());
            let app = extract_windows_exe(&zip, &fixture.0).unwrap();
            assert_eq!(fs::read(app).unwrap(), b"updated app");
        }

        #[test]
        fn partial_zip_does_not_publish_a_console() {
            let fixture = Fixture::new();
            let zip = fixture.zip("2.0.0", &FILES[..1]);
            assert!(stage_windows_console(&zip, &fixture.0, "2.0.0").is_err());
            assert_eq!(
                fs::read_dir(fixture.0.join("muxel-console"))
                    .unwrap()
                    .count(),
                0
            );
        }

        #[test]
        fn repairs_an_incomplete_destination_and_detects_stale_same_size_bytes() {
            let fixture = Fixture::new();
            let zip = fixture.zip("2.0.0", &FILES);
            let dest = fixture.console("2.0.0");
            fs::create_dir_all(&dest).unwrap();
            fs::write(dest.join("conpty.dll"), FILES[0].1).unwrap();
            stage_windows_console(&zip, &fixture.0, "2.0.0").unwrap();
            assert_console(&dest);

            let stale = vec![b'x'; FILES[2].1.len()];
            fs::write(dest.join(FILES[2].0), &stale).unwrap();
            stage_windows_console(&zip, &fixture.0, "2.0.0").unwrap();
            assert_console(&dest);
        }

        #[test]
        fn reuses_matching_files_but_cannot_repair_a_directory_held_by_the_app() {
            let fixture = Fixture::new();
            let zip = fixture.zip("2.0.0", &FILES);
            let dest = fixture.console("2.0.0");
            stage_windows_console(&zip, &fixture.0, "2.0.0").unwrap();
            let active_directory = open_console_directory(&dest).unwrap();
            stage_windows_console(&zip, &fixture.0, "2.0.0").unwrap();
            assert_console(&dest);

            // Use a different fixture at the same release path to require repair.
            let replacement = fixture.zip(
                "2.0.0",
                &[("conpty.dll", b"updated DLL"), FILES[1], FILES[2]],
            );
            assert!(stage_windows_console(&replacement, &fixture.0, "2.0.0").is_err());
            assert_console(&dest);
            drop(active_directory);
            stage_windows_console(&replacement, &fixture.0, "2.0.0").unwrap();
            assert_eq!(fs::read(dest.join("conpty.dll")).unwrap(), b"updated DLL");
        }

        #[test]
        fn extracts_the_root_exe_even_if_an_earlier_non_exe_entry_cannot_be_opened() {
            let fixture = Fixture::new();
            let zip = fixture.0.join("release.zip");
            let plain = SimpleFileOptions::default();
            for locked in [false, true] {
                let sidecar = if locked {
                    plain.with_deprecated_encryption(b"fixture")
                } else {
                    plain
                };
                let mut archive = zip::ZipWriter::new(fs::File::create(&zip).unwrap());
                for (name, bytes, options) in [
                    (
                        "muxel-console/2.0.0/conpty.dll",
                        b"sidecar".as_slice(),
                        sidecar,
                    ),
                    ("nested/muxel.exe", b"nested app", plain),
                    ("muxel.exe", b"root app", plain),
                ] {
                    archive.start_file(name, options).unwrap();
                    archive.write_all(bytes).unwrap();
                }
                archive.finish().unwrap();
                let mut archive = zip::ZipArchive::new(fs::File::open(&zip).unwrap()).unwrap();
                let root = extract_windows_exe(&zip, &fixture.0).unwrap();
                assert_eq!(fs::read(root).unwrap(), b"root app");
                // A valid earlier member can still refuse lookup without credentials.
                match archive.by_index(0) {
                    Ok(entry) if !locked => {
                        assert_eq!(entry.name(), "muxel-console/2.0.0/conpty.dll")
                    }
                    Err(ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED)) if locked => {}
                    Ok(_) => panic!("locked fixture member opened without a password"),
                    Err(error) => panic!("unexpected fixture error: {error}"),
                }
            }
        }

        #[test]
        fn update_preserves_a_console_folder_in_use_until_a_later_start() {
            let fixture = Fixture::new();
            let old_zip = fixture.zip("1.0.0", &FILES);
            stage_windows_console(&old_zip, &fixture.0, "1.0.0").unwrap();
            let old = fixture.console("1.0.0");
            let active_directory = open_console_directory(&old).unwrap();

            let new_zip = fixture.zip("2.0.0", &FILES);
            stage_windows_console(&new_zip, &fixture.0, "2.0.0").unwrap();
            prune_windows_consoles(&fixture.0.join("muxel-console"), "1.0.0");
            assert_console(&fixture.console("2.0.0"));
            prune_windows_consoles(&fixture.0.join("muxel-console"), "2.0.0");
            assert_console(&old);

            drop(active_directory);
            prune_windows_consoles(&fixture.0.join("muxel-console"), "2.0.0");
            assert!(!old.exists());
            assert_console(&fixture.console("2.0.0"));
        }
    }

    #[test]
    fn is_newer_compares_versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9")); // numeric, not lexical
        assert!(is_newer("0.2", "0.1.9")); // shorter-but-larger component
        assert!(!is_newer("0.1.0", "0.1")); // 0.1.0 == 0.1 (missing parts = 0)
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("0.1.0", "also-bad"));
    }

    #[test]
    fn asset_for_picks_the_right_file() {
        // Mirror exactly what the release workflow publishes (see release.yml).
        let assets = vec![
            ("muxel-linux-x86_64.AppImage".into(), "u1".into()),
            ("muxel-linux-aarch64.AppImage".into(), "u2".into()),
            ("muxel-linux-x86_64.tar.gz".into(), "u3".into()),
            ("muxel-macos-universal.zip".into(), "u4".into()),
            ("muxel-macos-universal.dmg".into(), "u5".into()),
            ("muxel-windows-x86_64.zip".into(), "u6".into()),
            ("muxel-windows-arm64.zip".into(), "u7".into()),
        ];
        // Linux/Windows have per-architecture assets; macOS ships one universal build.
        let linux = if cfg!(target_arch = "aarch64") {
            "muxel-linux-aarch64.AppImage"
        } else {
            "muxel-linux-x86_64.AppImage"
        };
        let windows = if cfg!(target_arch = "aarch64") {
            "muxel-windows-arm64.zip"
        } else {
            "muxel-windows-x86_64.zip"
        };
        assert_eq!(
            asset_for(InstallKind::AppImage, &assets).map(|a| a.0.as_str()),
            Some(linux)
        );
        assert_eq!(
            asset_for(InstallKind::LinuxPortable, &assets).map(|a| a.0.as_str()),
            Some(linux)
        );
        assert_eq!(
            asset_for(InstallKind::MacOsApp, &assets).map(|a| a.0.as_str()),
            Some("muxel-macos-universal.zip")
        );
        assert_eq!(
            asset_for(InstallKind::WindowsPortable, &assets).map(|a| a.0.as_str()),
            Some(windows)
        );
        assert!(asset_for(InstallKind::SystemPackage, &assets).is_none());
    }

    #[test]
    fn self_updatable_and_hints() {
        assert!(InstallKind::AppImage.self_updatable());
        assert!(InstallKind::MacOsApp.self_updatable());
        assert!(InstallKind::WindowsPortable.self_updatable());
        assert!(InstallKind::LinuxPortable.self_updatable());
        assert!(!InstallKind::SystemPackage.self_updatable());

        assert!(
            InstallKind::SystemPackage
                .upgrade_hint()
                .unwrap()
                .contains("muxel")
        );
        assert!(InstallKind::AppImage.upgrade_hint().is_none());
    }
}
