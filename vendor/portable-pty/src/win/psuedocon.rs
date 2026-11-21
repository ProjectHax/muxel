use super::WinChild;
use crate::cmdbuilder::CommandBuilder;
use crate::win::procthreadattr::ProcThreadAttributeList;
use anyhow::{bail, ensure, Error};
use filedescriptor::{FileDescriptor, OwnedHandle};
use lazy_static::lazy_static;
use shared_library::shared_library;
use std::ffi::OsString;
use std::io::Error as IoError;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::{mem, ptr};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::{HRESULT, S_OK};
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::winbase::{
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};
use winapi::um::wincon::COORD;
use winapi::um::winnt::HANDLE;
use winapi::um::winver::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};

pub type HPCON = HANDLE;

pub const PSEUDOCONSOLE_RESIZE_QUIRK: DWORD = 0x2;
pub const PSEUDOCONSOLE_WIN32_INPUT_MODE: DWORD = 0x4;
#[allow(dead_code)]
pub const PSEUDOCONSOLE_PASSTHROUGH_MODE: DWORD = 0x8;

shared_library!(ConPtyFuncs,
    pub fn CreatePseudoConsole(
        size: COORD,
        hInput: HANDLE,
        hOutput: HANDLE,
        flags: DWORD,
        hpc: *mut HPCON
    ) -> HRESULT,
    pub fn ResizePseudoConsole(hpc: HPCON, size: COORD) -> HRESULT,
    pub fn ClosePseudoConsole(hpc: HPCON),
);

static CONSOLE_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();

/// Prefer this absolute directory for the bundled pair. Call before opening the first PTY.
pub fn set_console_directory(directory: PathBuf) {
    if directory.is_absolute() {
        let _ = CONSOLE_DIRECTORY.set(directory);
    }
}

fn load_conpty() -> ConPtyFuncs {
    // If the kernel doesn't export these functions then their system is
    // too old and we cannot run.
    let kernel = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(
        "this system does not support conpty.  Windows 10 October 2018 or newer is required",
    );

    let bundled = std::env::current_exe()
        .inspect_err(|error| log::debug!("Skipping executable-relative console bundles: {error}"))
        .ok()
        .and_then(|exe| {
            CONSOLE_DIRECTORY
                .get()
                .map(PathBuf::as_path)
                .into_iter()
                .chain(exe.parent())
                .find_map(|directory| {
                    let host = directory.join("OpenConsole.exe");
                    // The DLL silently selects the inbox host if its companion is missing.
                    if !host.is_file() {
                        log::debug!(
                            "Skipping Windows console bundle: no host file at {}",
                            host.display()
                        );
                        return None;
                    }
                    let dll = directory.join("conpty.dll");
                    let functions = ConPtyFuncs::open(&dll)
                        .inspect_err(|error| {
                            log::debug!(
                            "Skipping Windows console bundle: could not load {}: {error:?}",
                                dll.display()
                            )
                        })
                        .ok()?;
                    Some((functions, dll, host))
                })
        });
    if let Some((sideloaded, dll, host)) = bundled {
        let version = file_version(&host).unwrap_or_else(|| "unavailable".to_owned());
        log::info!(
            "Windows console selection: DLL {}, OpenConsole.exe file version {version}",
            dll.display()
        );
        sideloaded
    } else {
        log::info!("Windows console selection: system (kernel32.dll)");
        kernel
    }
}

fn file_version(path: &Path) -> Option<String> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let root = [b'\\' as u16, 0];
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), ptr::null_mut()) };
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u32; (size as usize).div_ceil(mem::size_of::<u32>())];
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, buffer.as_mut_ptr().cast()) } == 0 {
        return None;
    }
    let mut value = ptr::null_mut();
    let mut len = 0;
    // The root value is VS_FIXEDFILEINFO. Read its first four DWORDs while
    // the backing version buffer is still alive.
    if unsafe { VerQueryValueW(buffer.as_ptr().cast(), root.as_ptr(), &mut value, &mut len) } == 0
        || value.is_null()
        || (len as usize) < mem::size_of::<[u32; 4]>()
    {
        return None;
    }
    let [signature, _, ms, ls] = unsafe { value.cast::<[u32; 4]>().read_unaligned() };
    (signature == 0xFEEF04BD)
        .then(|| format!("{}.{}.{}.{}", ms >> 16, ms & 0xffff, ls >> 16, ls & 0xffff))
}

lazy_static! {
    static ref CONPTY: ConPtyFuncs = load_conpty();
}

pub struct PsuedoCon {
    con: HPCON,
}

unsafe impl Send for PsuedoCon {}
unsafe impl Sync for PsuedoCon {}

impl Drop for PsuedoCon {
    fn drop(&mut self) {
        unsafe { (CONPTY.ClosePseudoConsole)(self.con) };
    }
}

impl PsuedoCon {
    pub fn new(size: COORD, input: FileDescriptor, output: FileDescriptor) -> Result<Self, Error> {
        let mut con: HPCON = INVALID_HANDLE_VALUE;
        let result = unsafe {
            (CONPTY.CreatePseudoConsole)(
                size,
                input.as_raw_handle() as _,
                output.as_raw_handle() as _,
                // Fresh Muxel terminals have no parent console cursor to inherit.
                // Its query can otherwise block close after an early child exit.
                PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE,
                &mut con,
            )
        };
        ensure!(
            result == S_OK,
            "failed to create psuedo console: HRESULT {}",
            result
        );
        Ok(Self { con })
    }

    pub fn resize(&self, size: COORD) -> Result<(), Error> {
        let result = unsafe { (CONPTY.ResizePseudoConsole)(self.con, size) };
        ensure!(
            result == S_OK,
            "failed to resize console to {}x{}: HRESULT: {}",
            size.X,
            size.Y,
            result
        );
        Ok(())
    }

    pub fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<WinChild> {
        let Some(job) = cmd.job_object() else {
            return self.create_process(&cmd, None);
        };
        // A job Windows refuses (a parent job's limits, a handle without
        // assign access) must not keep the program from starting. The retry
        // runs without it; the caller can tell with `IsProcessInJob`.
        match self.create_process(&cmd, Some(job as HANDLE)) {
            Ok(child) => Ok(child),
            Err(err) => {
                log::warn!(
                    "starting the child in its job object failed, retrying without it: {err:#}"
                );
                self.create_process(&cmd, None)
            }
        }
    }

    fn create_process(
        &self,
        cmd: &CommandBuilder,
        job: Option<HANDLE>,
    ) -> anyhow::Result<WinChild> {
        let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        // Explicitly set the stdio handles as invalid handles otherwise
        // we can end up with a weird state where the spawned process can
        // inherit the explicitly redirected output handles from its parent.
        // For example, when daemonizing wezterm-mux-server, the stdio handles
        // are redirected to a log file and the spawned process would end up
        // writing its output there instead of to the pty we just created.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;

        let mut attrs = ProcThreadAttributeList::with_capacity(if job.is_some() { 2 } else { 1 })?;
        attrs.set_pty(self.con)?;
        if let Some(job) = job {
            attrs.set_job(job)?;
        }
        si.lpAttributeList = attrs.as_mut_ptr();

        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let (mut exe, mut cmdline) = cmd.cmdline()?;
        let cmd_os = OsString::from_wide(&cmdline);

        let cwd = cmd.current_directory();

        let res = unsafe {
            CreateProcessW(
                exe.as_mut_slice().as_mut_ptr(),
                cmdline.as_mut_slice().as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                cmd.environment_block().as_mut_slice().as_mut_ptr() as *mut _,
                cwd.as_ref()
                    .map(|c| c.as_slice().as_ptr())
                    .unwrap_or(ptr::null()),
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        if res == 0 {
            let err = IoError::last_os_error();
            let msg = format!(
                "CreateProcessW `{:?}` in cwd `{:?}` failed: {}",
                cmd_os,
                cwd.as_ref().map(|c| OsString::from_wide(c)),
                err
            );
            log::error!("{}", msg);
            bail!("{}", msg);
        }

        // Make sure we close out the thread handle so we don't leak it;
        // we do this simply by making it owned
        let _main_thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread as _) };
        let proc = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as _) };

        Ok(WinChild {
            proc: Mutex::new(proc),
        })
    }
}
