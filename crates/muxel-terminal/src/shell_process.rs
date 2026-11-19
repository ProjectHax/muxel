//! Fresh Windows child-process evidence for a shell close.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Instant;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, ERROR_SUCCESS, FILETIME, HANDLE,
};
use windows::Win32::Storage::Packaging::Appx::{
    GetPackageFamilyName, PACKAGE_FAMILY_NAME_MAX_LENGTH,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_ROUTINE_FLAGS, RRF_RT_REG_SZ,
    RRF_SUBKEY_WOW6432KEY, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::core::{PCWSTR, PWSTR, w};

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this guard owns a successful OpenProcess/snapshot handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Creation time, in FILETIME ticks. Read at spawn while the child's own
/// handle pins its PID, and again when checking identity at close.
pub(super) fn started(pid: u32) -> Option<u64> {
    // SAFETY: OpenProcess takes values; the guard closes its returned handle.
    let process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? });
    let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
    // SAFETY: the process handle is live and each output is a stack FILETIME.
    unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) }
        .ok()?;
    Some(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
}

fn image(pid: u32) -> Option<PathBuf> {
    // SAFETY: the guard pins this process while the API fills the owned buffer.
    let process =
        OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? });
    let mut buffer = vec![0u16; 32768];
    let mut size = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    }
    .ok()?;
    Some(PathBuf::from(OsString::from_wide(&buffer[..size as usize])))
}

fn same_path(left: &Path, right: &Path) -> bool {
    fn fold(c: u16) -> u16 {
        match c {
            47 => 92,
            65..=90 => c + 32,
            _ => c,
        }
    }
    left.as_os_str()
        .encode_wide()
        .map(fold)
        .eq(right.as_os_str().encode_wide().map(fold))
}

fn named(path: &Path, name: &str) -> bool {
    path.file_name()
        .and_then(|part| part.to_str())
        .is_some_and(|part| part.eq_ignore_ascii_case(name))
}

/// Pin the image of the executable selected by the Git Bash resolver to this
/// launch. Scoop's stable junction and the process image can name the same file.
pub(super) fn selected_git_image(program: &Path, pid: u32) -> Option<PathBuf> {
    if !named(program, "bash.exe") {
        return None;
    }
    let selected = muxel_core::resolved_git_bash_program()?;
    let actual = image(pid)?;
    (same_path(&actual, &selected)
        || actual
            .canonicalize()
            .ok()
            .zip(selected.canonicalize().ok())
            .is_some_and(|(actual, selected)| same_path(&actual, &selected)))
    .then_some(actual)
}

fn registry_path(
    hive: HKEY,
    key: PCWSTR,
    value: PCWSTR,
    view: REG_ROUTINE_FLAGS,
) -> Option<PathBuf> {
    let mut path = [0u16; 1024];
    let mut size = std::mem::size_of_val(&path) as u32;
    // SAFETY: callers pass terminated literals. Size describes the writable buffer.
    let result = unsafe {
        RegGetValueW(
            hive,
            key,
            value,
            RRF_RT_REG_SZ | view,
            None,
            Some(path.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if result != ERROR_SUCCESS {
        return None;
    }
    let len = path.iter().position(|&c| c == 0)?;
    let path = PathBuf::from(OsString::from_wide(&path[..len]));
    path.is_absolute().then_some(path)
}

fn registered_git_root(root: &Path) -> bool {
    // Git for Windows' installer records the installation in the machine or
    // user hive. A bin/usr/bin layout alone is also used by other MSYS shells.
    [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER]
        .into_iter()
        .any(|hive| {
            [REG_ROUTINE_FLAGS(0), RRF_SUBKEY_WOW6432KEY]
                .into_iter()
                .any(|view| {
                    registry_path(hive, w!("Software\\GitForWindows"), w!("InstallPath"), view)
                        .is_some_and(|registered| same_path(root, &registered))
                })
        })
}

fn wsl_installed_image(image: &Path) -> bool {
    let msi_root = registry_path(
        HKEY_LOCAL_MACHINE,
        w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Lxss\\MSI"),
        w!("InstallLocation"),
        RRF_SUBKEY_WOW6464KEY,
    );
    msi_root.is_some_and(|root| same_path(image, &root.join("wsl.exe")))
        || std::env::var_os("ProgramW6432")
            .or_else(|| std::env::var_os("ProgramFiles"))
            .is_some_and(|root| same_path(image, &Path::new(&root).join("WSL/wsl.exe")))
}

fn wsl_package(pid: u32) -> bool {
    // SAFETY: the successful handle is owned across the package query.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return false;
    };
    let process = OwnedHandle(process);
    let mut name = [0u16; PACKAGE_FAMILY_NAME_MAX_LENGTH as usize + 1];
    let mut len = name.len() as u32;
    // SAFETY: len gives the writable capacity in UTF-16 units, including NUL.
    let result = unsafe { GetPackageFamilyName(process.0, &mut len, PWSTR(name.as_mut_ptr())) };
    result == ERROR_SUCCESS
        && name[..(len as usize).saturating_sub(1)]
            .iter()
            .copied()
            .eq("MicrosoftCorporationII.WindowsSubsystemForLinux_8wekyb3d8bbwe".encode_utf16())
}

/// Known packaging launchers delegate the console to a real shell/client.
/// Match the image paths, never merely a same-name background child.
fn launcher_child(
    parent: &Path,
    child: &Path,
    system_root: Option<&Path>,
    selected_git: Option<&Path>,
    git_root: impl FnOnce(&Path) -> bool,
    installed_wsl: impl FnOnce(&Path) -> bool,
    packaged_wsl: impl FnOnce() -> bool,
) -> bool {
    if named(parent, "bash.exe")
        && let Some(bin) = parent.parent().filter(|bin| named(bin, "bin"))
        && let Some(root) = bin.parent()
        && same_path(child, &root.join("usr/bin/bash.exe"))
    {
        return selected_git.is_some_and(|selected| same_path(parent, selected)) || git_root(root);
    }
    system_root.is_some_and(|root| same_path(parent, &root.join("System32/wsl.exe")))
        && named(child, "wsl.exe")
        && !same_path(parent, child)
        && (installed_wsl(child) || packaged_wsl())
}

/// Certify only a complete snapshot containing the original shell and no
/// work child. A recognized launcher's sole delegate is the shell/client;
/// its children still count. Windows retains parent PIDs after exit; an older process
/// naming a recycled shell PID is not its child. Unreadable child identities
/// count as work. Descendants whose intermediate parent has exited cannot be
/// reconstructed from this snapshot.
pub(super) struct Snapshot {
    started: Instant,
    present: HashSet<u32>,
    children: HashMap<u32, Vec<u32>>,
}

impl Snapshot {
    pub(super) fn started(&self) -> Instant {
        self.started
    }

    pub(super) fn read() -> Option<Self> {
        let started = Instant::now();
        // SAFETY: snapshot creation takes no pointers; the guard closes it.
        let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return None;
        };
        let snapshot = OwnedHandle(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: entry has the size the API requires and lives through the call.
        if unsafe { Process32FirstW(snapshot.0, &mut entry) }.is_err() {
            return None;
        }
        let mut result = Self {
            started,
            present: HashSet::new(),
            children: HashMap::new(),
        };
        loop {
            result.present.insert(entry.th32ProcessID);
            result
                .children
                .entry(entry.th32ParentProcessID)
                .or_default()
                .push(entry.th32ProcessID);
            // SAFETY: as for First. Only the normal end certifies a complete walk.
            if let Err(error) = unsafe { Process32NextW(snapshot.0, &mut entry) } {
                return (error.code() == windows::core::HRESULT::from_win32(ERROR_NO_MORE_FILES.0))
                    .then_some(result);
            }
        }
    }

    pub(super) fn has_no_children(
        &self,
        pid: u32,
        creation: u64,
        prompt_observed: Instant,
        selected_git_image: Option<&Path>,
    ) -> bool {
        // A batch may read another pane after this snapshot. Its later prompt
        // cannot be paired with an earlier view of the process table.
        if prompt_observed > self.started
            || !self.present.contains(&pid)
            || started(pid) != Some(creation)
        {
            return false;
        }
        let mut only = None;
        for &child in self.children.get(&pid).into_iter().flatten() {
            let Some(time) = started(child) else {
                return false;
            };
            if time < creation {
                continue;
            }
            if only.replace((child, time)).is_some() {
                return false;
            }
        }
        let Some((child, child_creation)) = only else {
            return true;
        };
        let Some(parent_image) = image(pid) else {
            return false;
        };
        let Some(child_image) = image(child) else {
            return false;
        };
        let system_root = std::env::var_os("SystemRoot").map(PathBuf::from);
        launcher_child(
            &parent_image,
            &child_image,
            system_root.as_deref(),
            selected_git_image,
            registered_git_root,
            wsl_installed_image,
            || wsl_package(child),
        ) && started(child) == Some(child_creation)
            && self.children.get(&child).is_none_or(|children| {
                children
                    .iter()
                    .all(|child| started(*child).is_some_and(|time| time < child_creation))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{launcher_child, same_path};
    use std::path::Path;

    #[test]
    fn only_known_launcher_images_delegate_the_child_check() {
        let system = Some(Path::new(r"C:\Windows"));
        let selected = Some(Path::new(r"E:\portable\Git\bin\bash.exe"));
        let cases = [
            (
                r"C:\fixture\Git\bin\bash.exe",
                r"C:\fixture\Git\usr\bin\bash.exe",
                true,
            ),
            (
                r"C:\fixture\Git\usr\bin\bash.exe",
                r"C:\fixture\Git\usr\bin\bash.exe",
                false,
            ),
            (r"C:\fixture\Git\bin\bash.exe", r"C:\other\bash.exe", false),
            (
                r"C:\other\Git\bin\bash.exe",
                r"C:\other\Git\usr\bin\bash.exe",
                false,
            ),
            (
                r"E:\portable\Git\bin\bash.exe",
                r"E:\portable\Git\usr\bin\bash.exe",
                true,
            ),
            (
                r"E:\portable\Git\bin\bash.exe",
                r"E:\different\Git\usr\bin\bash.exe",
                false,
            ),
            (
                r"C:\Windows\System32\wsl.exe",
                r"C:\Program Files\WSL\wsl.exe",
                true,
            ),
            (
                r"C:\Windows\System32\wsl.exe",
                r"E:\tools\linux\wsl.exe",
                true,
            ),
            (
                r"C:\Windows\System32\wsl.exe",
                r"E:\tools\linux-old\wsl.exe",
                false,
            ),
            (r"C:\Windows\System32\wsl.exe", r"C:\other\wsl.exe", false),
            (
                r"C:\Program Files\WSL\wsl.exe",
                r"C:\Program Files\WSL\wsl.exe",
                false,
            ),
            (
                r"C:\Windows\System32\cmd.exe",
                r"C:\Windows\System32\cmd.exe",
                false,
            ),
            (r"C:\fixture\nu.exe", r"C:\fixture\nu.exe", false),
            (r"C:\fixture\pwsh.exe", r"C:\fixture\pwsh.exe", false),
        ];
        let mismatches: Vec<_> = cases
            .into_iter()
            .filter(|&(parent, child, expected)| {
                launcher_child(
                    Path::new(parent),
                    Path::new(child),
                    system,
                    selected,
                    |root| same_path(root, Path::new(r"C:\fixture\Git")),
                    |image| {
                        [r"C:\Program Files\WSL", r"E:\tools\linux\"]
                            .iter()
                            .any(|root| same_path(image, &Path::new(root).join("wsl.exe")))
                    },
                    || false,
                ) != expected
            })
            .collect();
        assert!(
            mismatches.is_empty(),
            "wrong launcher decisions: {mismatches:?}"
        );
        assert!(launcher_child(
            Path::new(r"C:\Windows\System32\wsl.exe"),
            Path::new(r"C:\Program Files\WindowsApps\fixture\wsl.exe"),
            system,
            None,
            |_| false,
            |_| false,
            || true,
        ));
    }
}
