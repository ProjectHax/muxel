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
use std::sync::atomic::{AtomicBool, Ordering};
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

type CreatePseudoConsoleFn =
    unsafe extern "C" fn(COORD, HANDLE, HANDLE, DWORD, *mut HPCON) -> HRESULT;
type ResizePseudoConsoleFn = unsafe extern "C" fn(HPCON, COORD) -> HRESULT;
type ClosePseudoConsoleFn = unsafe extern "C" fn(HPCON);

#[derive(Clone, Copy)]
struct ConPtyTable {
    create: CreatePseudoConsoleFn,
    resize: ResizePseudoConsoleFn,
    close: ClosePseudoConsoleFn,
}

impl ConPtyTable {
    fn from_funcs(funcs: &ConPtyFuncs) -> Self {
        Self {
            create: funcs.CreatePseudoConsole,
            resize: funcs.ResizePseudoConsole,
            close: funcs.ClosePseudoConsole,
        }
    }
}

struct ConPtySelection {
    kernel: ConPtyTable,
    bundled: Option<ConPtyTable>,
    bundled_failed: AtomicBool,
}

impl ConPtySelection {
    fn new(kernel: ConPtyTable, bundled: Option<ConPtyTable>) -> Self {
        Self {
            kernel,
            bundled,
            bundled_failed: AtomicBool::new(false),
        }
    }

    fn create(&self, size: COORD, input: HANDLE, output: HANDLE) -> Result<PsuedoCon, Error> {
        // Fresh Muxel terminals have no parent console cursor to inherit.
        // Its query can otherwise block close after an early child exit.
        let flags = PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE;
        if !self.bundled_failed.load(Ordering::Acquire) {
            if let Some(bundled) = self.bundled {
                let mut con: HPCON = INVALID_HANDLE_VALUE;
                let bundled_hr = unsafe { (bundled.create)(size, input, output, flags, &mut con) };
                if bundled_hr == S_OK {
                    return Ok(PsuedoCon {
                        con,
                        table: bundled,
                    });
                }
                if !self.bundled_failed.swap(true, Ordering::AcqRel) {
                    log::warn!(
                        "Bundled Windows console host failed to start (HRESULT 0x{:08x}); falling back to system (kernel32.dll) for this and subsequent panes",
                        bundled_hr as u32
                    );
                }
                // `conpty.dll` duplicates `hInput` and `hOutput` internally and clears `*phPC`
                // on failure, so our original pipe handles stay open and valid to pass again.
                let mut fallback_con: HPCON = INVALID_HANDLE_VALUE;
                let kernel_hr =
                    unsafe { (self.kernel.create)(size, input, output, flags, &mut fallback_con) };
                ensure!(
                    kernel_hr == S_OK,
                    "failed to create psuedo console: bundled HRESULT 0x{:08x} ({}), system HRESULT 0x{:08x} ({})",
                    bundled_hr as u32,
                    bundled_hr,
                    kernel_hr as u32,
                    kernel_hr
                );
                return Ok(PsuedoCon {
                    con: fallback_con,
                    table: self.kernel,
                });
            }
        }

        let mut con: HPCON = INVALID_HANDLE_VALUE;
        let result = unsafe { (self.kernel.create)(size, input, output, flags, &mut con) };
        ensure!(
            result == S_OK,
            "failed to create psuedo console: HRESULT 0x{:08x} ({})",
            result as u32,
            result
        );
        Ok(PsuedoCon {
            con,
            table: self.kernel,
        })
    }
}

struct ConPtyLoaded {
    _kernel: ConPtyFuncs,
    _bundled: Option<ConPtyFuncs>,
    selection: ConPtySelection,
}

static CONSOLE_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static FORCE_SYSTEM_CONSOLE: OnceLock<bool> = OnceLock::new();

/// Prefer this absolute directory for the bundled pair. Call before opening the first PTY.
pub fn set_console_directory(directory: PathBuf) {
    if directory.is_absolute() {
        let _ = CONSOLE_DIRECTORY.set(directory);
    }
}

/// Force the system console host (`kernel32.dll`), skipping any bundled pair.
/// Call before opening the first PTY.
pub fn set_force_system_console(force: bool) {
    let _ = FORCE_SYSTEM_CONSOLE.set(force);
}

fn discover_bundled<'a, T>(
    directories: impl IntoIterator<Item = &'a Path>,
    mut open_dll: impl FnMut(&Path) -> Result<T, String>,
) -> Option<(T, PathBuf, PathBuf)> {
    directories.into_iter().find_map(|directory| {
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
        let functions = open_dll(&dll)
            .inspect_err(|error| {
                log::debug!(
                    "Skipping Windows console bundle: could not load {}: {}",
                    dll.display(),
                    error
                )
            })
            .ok()?;
        Some((functions, dll, host))
    })
}

fn select_conpty<F>(
    force_system: bool,
    kernel: ConPtyTable,
    load_bundle: impl FnOnce() -> Option<(F, ConPtyTable, PathBuf, PathBuf)>,
) -> (Option<F>, ConPtySelection) {
    if force_system {
        log::info!("Windows console selection: system (kernel32.dll, forced)");
        return (None, ConPtySelection::new(kernel, None));
    }
    if let Some((owner, table, dll, host)) = load_bundle() {
        let version = file_version(&host).unwrap_or_else(|| "unavailable".to_owned());
        log::info!(
            "Windows console selection: DLL {}, OpenConsole.exe file version {}",
            dll.display(),
            version
        );
        (Some(owner), ConPtySelection::new(kernel, Some(table)))
    } else {
        log::info!("Windows console selection: system (kernel32.dll)");
        (None, ConPtySelection::new(kernel, None))
    }
}

fn load_conpty() -> ConPtyLoaded {
    // If the kernel doesn't export these functions then their system is
    // too old and we cannot run.
    let kernel = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(
        "this system does not support conpty.  Windows 10 October 2018 or newer is required",
    );

    let force_system = FORCE_SYSTEM_CONSOLE.get().copied().unwrap_or(false);
    let (bundled, selection) =
        select_conpty(force_system, ConPtyTable::from_funcs(&kernel), || {
            std::env::current_exe()
                .inspect_err(|error| {
                    log::debug!("Skipping executable-relative console bundles: {}", error)
                })
                .ok()
                .and_then(|exe| {
                    let directories = CONSOLE_DIRECTORY
                        .get()
                        .map(PathBuf::as_path)
                        .into_iter()
                        .chain(exe.parent());
                    discover_bundled(directories, |dll| {
                        ConPtyFuncs::open(dll).map_err(|error| format!("{:?}", error))
                    })
                })
                .map(|(funcs, dll, host)| {
                    let table = ConPtyTable::from_funcs(&funcs);
                    (funcs, table, dll, host)
                })
        });
    ConPtyLoaded {
        _kernel: kernel,
        _bundled: bundled,
        selection,
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
    static ref CONPTY: ConPtyLoaded = load_conpty();
}

pub struct PsuedoCon {
    con: HPCON,
    table: ConPtyTable,
}

unsafe impl Send for PsuedoCon {}
unsafe impl Sync for PsuedoCon {}

impl Drop for PsuedoCon {
    fn drop(&mut self) {
        unsafe { (self.table.close)(self.con) };
    }
}

impl PsuedoCon {
    pub fn new(size: COORD, input: FileDescriptor, output: FileDescriptor) -> Result<Self, Error> {
        CONPTY.selection.create(
            size,
            input.as_raw_handle() as _,
            output.as_raw_handle() as _,
        )
    }

    pub fn resize(&self, size: COORD) -> Result<(), Error> {
        let result = unsafe { (self.table.resize)(self.con, size) };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, AtomicUsize};

    const BUNDLED_HANDLE: usize = 0x1111;
    const KERNEL_HANDLE: usize = 0x2222;
    const HR_ACCESS_DENIED: HRESULT = 0x8007_0005u32 as HRESULT;
    const HR_OUT_OF_MEMORY: HRESULT = 0x8007_000Eu32 as HRESULT;

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static BUNDLED_CREATE_HR: AtomicI32 = AtomicI32::new(S_OK);
    static KERNEL_CREATE_HR: AtomicI32 = AtomicI32::new(S_OK);
    static BUNDLED_CREATE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static KERNEL_CREATE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static BUNDLED_RESIZE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static KERNEL_RESIZE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static BUNDLED_CLOSE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static KERNEL_CLOSE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST_BUNDLED_CLOSE: AtomicUsize = AtomicUsize::new(0);
    static LAST_KERNEL_CLOSE: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn fake_bundled_create(
        _size: COORD,
        _input: HANDLE,
        _output: HANDLE,
        _flags: DWORD,
        hpc: *mut HPCON,
    ) -> HRESULT {
        BUNDLED_CREATE_CALLS.fetch_add(1, Ordering::SeqCst);
        let hr = BUNDLED_CREATE_HR.load(Ordering::SeqCst);
        if hr == S_OK {
            *hpc = BUNDLED_HANDLE as HPCON;
        } else {
            *hpc = ptr::null_mut();
        }
        hr
    }

    unsafe extern "C" fn fake_bundled_resize(_hpc: HPCON, _size: COORD) -> HRESULT {
        BUNDLED_RESIZE_CALLS.fetch_add(1, Ordering::SeqCst);
        S_OK
    }

    unsafe extern "C" fn fake_bundled_close(hpc: HPCON) {
        BUNDLED_CLOSE_CALLS.fetch_add(1, Ordering::SeqCst);
        LAST_BUNDLED_CLOSE.store(hpc as usize, Ordering::SeqCst);
    }

    unsafe extern "C" fn fake_kernel_create(
        _size: COORD,
        _input: HANDLE,
        _output: HANDLE,
        _flags: DWORD,
        hpc: *mut HPCON,
    ) -> HRESULT {
        KERNEL_CREATE_CALLS.fetch_add(1, Ordering::SeqCst);
        let hr = KERNEL_CREATE_HR.load(Ordering::SeqCst);
        if hr == S_OK {
            *hpc = KERNEL_HANDLE as HPCON;
        } else {
            *hpc = ptr::null_mut();
        }
        hr
    }

    unsafe extern "C" fn fake_kernel_resize(_hpc: HPCON, _size: COORD) -> HRESULT {
        KERNEL_RESIZE_CALLS.fetch_add(1, Ordering::SeqCst);
        S_OK
    }

    unsafe extern "C" fn fake_kernel_close(hpc: HPCON) {
        KERNEL_CLOSE_CALLS.fetch_add(1, Ordering::SeqCst);
        LAST_KERNEL_CLOSE.store(hpc as usize, Ordering::SeqCst);
    }

    fn reset_fakes(bundled_hr: HRESULT, kernel_hr: HRESULT) -> (ConPtyTable, ConPtyTable) {
        BUNDLED_CREATE_HR.store(bundled_hr, Ordering::SeqCst);
        KERNEL_CREATE_HR.store(kernel_hr, Ordering::SeqCst);
        BUNDLED_CREATE_CALLS.store(0, Ordering::SeqCst);
        KERNEL_CREATE_CALLS.store(0, Ordering::SeqCst);
        BUNDLED_RESIZE_CALLS.store(0, Ordering::SeqCst);
        KERNEL_RESIZE_CALLS.store(0, Ordering::SeqCst);
        BUNDLED_CLOSE_CALLS.store(0, Ordering::SeqCst);
        KERNEL_CLOSE_CALLS.store(0, Ordering::SeqCst);
        LAST_BUNDLED_CLOSE.store(0, Ordering::SeqCst);
        LAST_KERNEL_CLOSE.store(0, Ordering::SeqCst);
        (
            ConPtyTable {
                create: fake_bundled_create,
                resize: fake_bundled_resize,
                close: fake_bundled_close,
            },
            ConPtyTable {
                create: fake_kernel_create,
                resize: fake_kernel_resize,
                close: fake_kernel_close,
            },
        )
    }

    #[test]
    fn falls_back_to_system_table_when_bundled_create_fails() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (bundled, kernel) = reset_fakes(HR_ACCESS_DENIED, S_OK);
        let selection = ConPtySelection::new(kernel, Some(bundled));
        let size = COORD { X: 80, Y: 24 };

        let pane = selection
            .create(size, ptr::null_mut(), ptr::null_mut())
            .expect("second table should succeed when bundled create fails");
        assert_eq!(BUNDLED_CREATE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(KERNEL_CREATE_CALLS.load(Ordering::SeqCst), 1);

        pane.resize(size).unwrap();
        assert_eq!(BUNDLED_RESIZE_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(KERNEL_RESIZE_CALLS.load(Ordering::SeqCst), 1);

        drop(pane);
        assert_eq!(BUNDLED_CLOSE_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(KERNEL_CLOSE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(LAST_KERNEL_CLOSE.load(Ordering::SeqCst), KERNEL_HANDLE);

        // Subsequent panes skip the failed bundle and go straight to kernel32.
        let second = selection
            .create(size, ptr::null_mut(), ptr::null_mut())
            .expect("subsequent pane should go straight to kernel32");
        assert_eq!(BUNDLED_CREATE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(KERNEL_CREATE_CALLS.load(Ordering::SeqCst), 2);
        drop(second);
        assert_eq!(KERNEL_CLOSE_CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn running_bundled_pane_keeps_bundled_table_after_later_pane_falls_back() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (bundled, kernel) = reset_fakes(S_OK, S_OK);
        let selection = ConPtySelection::new(kernel, Some(bundled));
        let size = COORD { X: 80, Y: 24 };

        let bundled_pane = selection
            .create(size, ptr::null_mut(), ptr::null_mut())
            .expect("first pane should open on bundled table");
        BUNDLED_CREATE_HR.store(HR_ACCESS_DENIED, Ordering::SeqCst);

        let fallback_pane = selection
            .create(size, ptr::null_mut(), ptr::null_mut())
            .expect("second pane should fall back to kernel32");

        bundled_pane.resize(size).unwrap();
        fallback_pane.resize(size).unwrap();
        assert_eq!(BUNDLED_RESIZE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(KERNEL_RESIZE_CALLS.load(Ordering::SeqCst), 1);

        drop(bundled_pane);
        assert_eq!(BUNDLED_CLOSE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(LAST_BUNDLED_CLOSE.load(Ordering::SeqCst), BUNDLED_HANDLE);
        assert_eq!(KERNEL_CLOSE_CALLS.load(Ordering::SeqCst), 0);

        drop(fallback_pane);
        assert_eq!(KERNEL_CLOSE_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(LAST_KERNEL_CLOSE.load(Ordering::SeqCst), KERNEL_HANDLE);
    }

    #[test]
    fn returns_both_hresults_when_bundled_and_system_create_both_fail() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (bundled, kernel) = reset_fakes(HR_ACCESS_DENIED, HR_OUT_OF_MEMORY);
        let selection = ConPtySelection::new(kernel, Some(bundled));
        let err = selection
            .create(COORD { X: 80, Y: 24 }, ptr::null_mut(), ptr::null_mut())
            .err()
            .expect("both tables failed");
        let message = format!("{:#}", err);
        assert!(
            message.contains("80070005"),
            "missing bundled HRESULT: {}",
            message
        );
        assert!(
            message.contains("8007000e"),
            "missing system HRESULT: {}",
            message
        );
    }

    #[test]
    fn forced_system_selection_ignores_present_bundle() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (bundled, kernel) = reset_fakes(S_OK, S_OK);
        let dir = std::env::temp_dir().join(format!("portable-pty-forced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("OpenConsole.exe"), b"host").unwrap();
        std::fs::write(dir.join("conpty.dll"), b"dll").unwrap();

        let mut open_calls = 0usize;
        let (owner, selection) = select_conpty(true, kernel, || {
            discover_bundled([dir.as_path()], |_| {
                open_calls += 1;
                Ok(bundled)
            })
            .map(|(table, dll, host)| ((), table, dll, host))
        });
        assert!(owner.is_none());
        assert_eq!(open_calls, 0);

        let pane = selection
            .create(COORD { X: 80, Y: 24 }, ptr::null_mut(), ptr::null_mut())
            .unwrap();
        assert_eq!(BUNDLED_CREATE_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(KERNEL_CREATE_CALLS.load(Ordering::SeqCst), 1);
        drop(pane);
        assert_eq!(KERNEL_CLOSE_CALLS.load(Ordering::SeqCst), 1);

        // With the same bundle present and not forced, selection loads and uses the bundle.
        let (owner, selection) = select_conpty(false, kernel, || {
            discover_bundled([dir.as_path()], |_| {
                open_calls += 1;
                Ok(bundled)
            })
            .map(|(table, dll, host)| ((), table, dll, host))
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(owner.is_some());
        assert_eq!(open_calls, 1);

        let pane = selection
            .create(COORD { X: 80, Y: 24 }, ptr::null_mut(), ptr::null_mut())
            .unwrap();
        assert_eq!(BUNDLED_CREATE_CALLS.load(Ordering::SeqCst), 1);
        drop(pane);
        assert_eq!(BUNDLED_CLOSE_CALLS.load(Ordering::SeqCst), 1);
    }
}
