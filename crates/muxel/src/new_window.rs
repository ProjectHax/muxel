//! New Window: open another muxel window by starting another muxel process.
//!
//! Each muxel window is its own process holding one workspace, locked so two
//! windows never write the same layout (`muxel_store::try_lock_workspace`). A new
//! window therefore opens on the workspace selector, where workspaces open in
//! other windows are marked "in use", or straight into a workspace named with
//! `--workspace`. It keeps this process's `--user-data-dir`.
//!
//! Starting the same binary again works on Linux and Windows. A macOS app bundle
//! goes through `open -n`, because Launch Services only activates a running app;
//! `open` passes `--args` along but not the environment, hence the explicit
//! `--user-data-dir`. An AppImage starts its `.AppImage` file again rather than
//! the binary inside this process's mount, which goes away when this one exits.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context as _, Result};
use gpui::{Bounds, Pixels, point, px};
use uuid::Uuid;

/// Marks a launch made by New Window: its window opens offset from the saved
/// position, so it doesn't land exactly on top of the window that opened it.
pub const NEW_WINDOW_FLAG: &str = "--new-window";
/// `--workspace ID`: open that workspace at launch instead of the selector,
/// unless another window already has it open.
pub const WORKSPACE_FLAG: &str = "--workspace";

/// How far a new window is moved from the saved position, right and down.
const CASCADE: f32 = 32.0;

/// What this launch was asked to do by [`NEW_WINDOW_FLAG`] and [`WORKSPACE_FLAG`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LaunchRequest {
    pub cascade: bool,
    pub workspace: Option<Uuid>,
}

static LAUNCH: OnceLock<LaunchRequest> = OnceLock::new();

/// Take this launch's window flags out of `args`, remember them for
/// [`launch_request`], and return the rest.
pub fn configure(args: Vec<OsString>) -> Result<Vec<OsString>, String> {
    let (request, rest) = take_flags(args)?;
    let _ = LAUNCH.set(request);
    Ok(rest)
}

/// The flags [`configure`] found; empty when it wasn't called.
pub fn launch_request() -> LaunchRequest {
    LAUNCH.get().copied().unwrap_or_default()
}

fn take_flags(args: Vec<OsString>) -> Result<(LaunchRequest, Vec<OsString>), String> {
    let mut request = LaunchRequest::default();
    let mut rest = Vec::with_capacity(args.len());
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == NEW_WINDOW_FLAG {
            request.cascade = true;
        } else if arg == WORKSPACE_FLAG {
            let value = args
                .next()
                .ok_or_else(|| format!("{WORKSPACE_FLAG} needs a workspace id"))?;
            let id = value
                .to_str()
                .and_then(|v| v.parse::<Uuid>().ok())
                .ok_or_else(|| {
                    format!(
                        "{WORKSPACE_FLAG}: {} is not a workspace id",
                        value.to_string_lossy()
                    )
                })?;
            request.workspace = Some(id);
        } else {
            rest.push(arg);
        }
    }
    Ok((request, rest))
}

/// Where a New Window launch puts its window: the saved bounds moved right and
/// down, so both windows stay visible. Size and maximized state are kept.
pub fn cascaded(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: point(bounds.origin.x + px(CASCADE), bounds.origin.y + px(CASCADE)),
        size: bounds.size,
    }
}

/// Start another muxel window, opening `workspace` when given or else the
/// workspace selector. Returns once the process has started; it runs on its own
/// and outlives this one.
pub fn open(workspace: Option<Uuid>) -> Result<()> {
    let exe = std::env::current_exe().context("resolving the muxel executable")?;
    let root = std::env::var_os(muxel_store::USER_DATA_DIR_ENV)
        .filter(|root| !root.is_empty())
        .map(PathBuf::from);
    let (program, args) = launch_plan(
        &exe,
        crate::update::running_appimage(),
        root.as_deref(),
        workspace,
    );
    let mut command = muxel_core::child_env::command(&program);
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Its own process group, so a signal meant for this window's group (a
    // terminal's Ctrl+C when muxel was started from one) leaves it alone.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    // Collect its exit status whenever it ends, so it never lingers as a zombie.
    let _ = std::thread::Builder::new()
        .name("muxel-new-window".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(())
}

/// The program and arguments that start another window of this muxel.
fn launch_plan(
    exe: &Path,
    appimage: Option<PathBuf>,
    root: Option<&Path>,
    workspace: Option<Uuid>,
) -> (PathBuf, Vec<OsString>) {
    let mut flags: Vec<OsString> = Vec::new();
    if let Some(root) = root {
        flags.push("--user-data-dir".into());
        flags.push(root.into());
    }
    flags.push(NEW_WINDOW_FLAG.into());
    if let Some(id) = workspace {
        flags.push(WORKSPACE_FLAG.into());
        flags.push(id.to_string().into());
    }
    if let Some(appimage) = appimage {
        return (appimage, flags);
    }
    if let Some(bundle) = macos_bundle(exe) {
        let mut args: Vec<OsString> = vec!["-n".into(), bundle.into(), "--args".into()];
        args.extend(flags);
        return (PathBuf::from("/usr/bin/open"), args);
    }
    (exe.to_path_buf(), flags)
}

/// The `.app` bundle `exe` runs from, on macOS.
fn macos_bundle(exe: &Path) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let macos_dir = exe.parent()?;
    let contents = macos_dir.parent()?;
    let bundle = contents.parent()?;
    (macos_dir.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension().is_some_and(|ext| ext == "app"))
    .then(|| bundle.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{
        LaunchRequest, NEW_WINDOW_FLAG, WORKSPACE_FLAG, cascaded, launch_plan, take_flags,
    };
    use gpui::{Bounds, point, px, size};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    fn os(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn window_flags_are_taken_out_and_the_rest_kept() {
        let id = Uuid::new_v4();
        let line = os(&[NEW_WINDOW_FLAG, "--other", WORKSPACE_FLAG, &id.to_string()]);
        let (request, rest) = take_flags(line).unwrap();
        assert_eq!(
            request,
            LaunchRequest {
                cascade: true,
                workspace: Some(id)
            }
        );
        assert_eq!(rest, os(&["--other"]));
        assert_eq!(
            take_flags(os(&[])).unwrap(),
            (LaunchRequest::default(), vec![])
        );
    }

    #[test]
    fn a_workspace_flag_needs_a_real_id() {
        assert!(take_flags(os(&[WORKSPACE_FLAG])).is_err());
        assert!(take_flags(os(&[WORKSPACE_FLAG, "work"])).is_err());
    }

    #[test]
    fn a_new_window_is_offset_from_the_saved_position() {
        let saved = Bounds {
            origin: point(px(100.0), px(50.0)),
            size: size(px(1200.0), px(800.0)),
        };
        let moved = cascaded(saved);
        assert_eq!(moved.origin, point(px(132.0), px(82.0)));
        assert_eq!(moved.size, saved.size);
    }

    #[test]
    fn a_plain_binary_starts_itself_again_with_the_same_profile() {
        let exe = Path::new("/opt/muxel/muxel");
        let id = Uuid::new_v4();
        let (program, args) = launch_plan(exe, None, Some(Path::new("/tmp/profile")), Some(id));
        assert_eq!(program, exe);
        assert_eq!(
            args,
            os(&[
                "--user-data-dir",
                "/tmp/profile",
                NEW_WINDOW_FLAG,
                WORKSPACE_FLAG,
                &id.to_string()
            ])
        );
        let (_, args) = launch_plan(exe, None, None, None);
        assert_eq!(args, os(&[NEW_WINDOW_FLAG]));
    }

    #[test]
    fn an_appimage_starts_its_file_not_the_binary_in_its_mount() {
        let exe = Path::new("/tmp/.mount_muxelX/usr/bin/muxel");
        let appimage = PathBuf::from("/home/user/Apps/muxel.AppImage");
        let (program, args) = launch_plan(exe, Some(appimage.clone()), None, None);
        assert_eq!(program, appimage);
        assert_eq!(args, os(&[NEW_WINDOW_FLAG]));
    }

    #[test]
    fn a_macos_bundle_goes_through_open() {
        let exe = Path::new("/Applications/muxel.app/Contents/MacOS/muxel");
        let (program, args) = launch_plan(exe, None, Some(Path::new("/tmp/p")), None);
        if cfg!(target_os = "macos") {
            assert_eq!(program, Path::new("/usr/bin/open"));
            assert_eq!(
                args,
                os(&[
                    "-n",
                    "/Applications/muxel.app",
                    "--args",
                    "--user-data-dir",
                    "/tmp/p",
                    NEW_WINDOW_FLAG
                ])
            );
        } else {
            assert_eq!(program, exe);
        }
    }
}
