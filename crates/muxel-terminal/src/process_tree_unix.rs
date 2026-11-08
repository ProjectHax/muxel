//! A Unix shell is quiet only when it has no child process, including jobs
//! running behind its prompt. Failure to inspect children never proves idle.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    executable: PathBuf,
    #[cfg(target_os = "linux")]
    started_at: u64,
}

impl ProcessIdentity {
    pub(crate) fn capture(pid: u32, expected: &Path) -> Option<Self> {
        let pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
        // The PTY's pre-exec cleanup can release the spawn handshake before
        // the target image is ready. Record the known launch path, not the
        // image observed during that transition; still_matches reads it live.
        #[cfg(target_os = "linux")]
        let started_at = linux::birth_time(Path::new("/proc"), pid).ok()?;
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
        Some(Self {
            executable: expected.to_owned(),
            #[cfg(target_os = "linux")]
            started_at,
        })
    }

    pub(crate) fn still_matches(&self, pid: u32) -> bool {
        Self::read(pid).as_ref() == Some(self)
    }

    fn read(pid: u32) -> Option<Self> {
        let pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
        #[cfg(target_os = "linux")]
        {
            linux::identity(Path::new("/proc"), pid).ok()
        }
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            // SAFETY: path is a writable buffer of exactly the supplied size.
            if unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) } <= 0
            {
                return None;
            }
            let path = std::ffi::CStr::from_bytes_until_nul(&path).ok()?;
            let executable =
                std::fs::canonicalize(std::ffi::OsStr::from_bytes(path.to_bytes())).ok()?;
            Some(Self { executable })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = pid;
            None
        }
    }
}

pub(crate) fn shell_has_no_children(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }

    #[cfg(target_os = "linux")]
    {
        linux::has_no_children(std::path::Path::new("/proc"), pid).unwrap_or(false)
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: signal 0 only checks the shell's existence; no signal is sent.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        let mut child: libc::pid_t = 0;
        // One slot suffices: any child means busy, even if more did not fit.
        // libproc maps some errors to a zero count, so clear and read errno
        // on this thread around the call to distinguish failure from no child.
        // SAFETY: __error points to this thread's errno; child is a writable,
        // aligned pid_t and the buffer size describes exactly that one slot.
        let (count, error) = unsafe {
            *libc::__error() = 0;
            let count = libc::proc_listchildpids(
                pid,
                (&mut child as *mut libc::pid_t).cast(),
                std::mem::size_of_val(&child) as libc::c_int,
            );
            (count, *libc::__error())
        };
        count == 0 && error == 0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::ProcessIdentity;
    use std::fs;
    use std::io::{self, ErrorKind};
    use std::path::Path;

    pub(super) fn has_no_children(proc: &Path, pid: libc::pid_t) -> io::Result<bool> {
        match task_children(proc, pid) {
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // A vanished shell task does not establish that the kernel
                // lacks CONFIG_PROC_CHILDREN. Probe our own surviving task.
                if !children_interface_missing(proc)? || !complete_visibility(proc)? {
                    return Err(error);
                }
                no_children_by_parent(proc, pid)
            }
            result => result,
        }
    }

    pub(super) fn identity(proc: &Path, pid: libc::pid_t) -> io::Result<ProcessIdentity> {
        let process = proc.join(pid.to_string());
        let started_at = birth_time(proc, pid)?;
        let executable = fs::canonicalize(fs::read_link(process.join("exe"))?)?;
        // A reused pid must not combine the previous process's start time
        // with the new process's executable.
        if birth_time(proc, pid)? != started_at {
            return Err(invalid_process());
        }
        Ok(ProcessIdentity {
            executable,
            started_at,
        })
    }

    pub(super) fn birth_time(proc: &Path, pid: libc::pid_t) -> io::Result<u64> {
        start_time(&fs::read(proc.join(pid.to_string()).join("stat"))?)
    }

    fn children_interface_missing(proc: &Path) -> io::Result<bool> {
        let task = proc.join(format!("self/task/{}", std::process::id()));
        match fs::read(task.join("children")) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(fs::metadata(task)?.is_dir()),
            Err(error) => Err(error),
        }
    }

    fn complete_visibility(proc: &Path) -> io::Result<bool> {
        let mounts = fs::read(proc.join("self/mountinfo"))?;
        let mut found = false;
        for line in mounts
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let separator = line
                .windows(3)
                .position(|part| part == b" - ")
                .ok_or_else(invalid_process)?;
            let mut mount = fields(&line[..separator]);
            let root = mount.nth(3).ok_or_else(invalid_process)?;
            if mount.next().ok_or_else(invalid_process)? != b"/proc" {
                continue;
            }
            let options = mount.next().ok_or_else(invalid_process)?;
            let mut filesystem = fields(&line[separator + 3..]);
            let kind = filesystem.next().ok_or_else(invalid_process)?;
            filesystem.next().ok_or_else(invalid_process)?;
            let super_options = filesystem.next().ok_or_else(invalid_process)?;
            if root != b"/"
                || kind != b"proc"
                || !unrestricted(options)
                || !unrestricted(super_options)
            {
                return Ok(false);
            }
            found = true;
        }
        Ok(found)
    }

    fn unrestricted(options: &[u8]) -> bool {
        options.split(|byte| *byte == b',').all(|option| {
            !option.starts_with(b"pidns=")
                && (!option.starts_with(b"hidepid")
                    || matches!(option, b"hidepid=0" | b"hidepid=off"))
        })
    }

    fn task_children(proc: &Path, pid: libc::pid_t) -> io::Result<bool> {
        let mut saw_task = false;
        for task in fs::read_dir(proc.join(pid.to_string()).join("task"))? {
            let children = fs::read(task?.path().join("children"))?;
            if children.iter().any(|byte| !byte.is_ascii_whitespace()) {
                return Ok(false);
            }
            saw_task = true;
        }
        // An empty task directory does not establish a living shell.
        Ok(saw_task)
    }

    pub(super) fn no_children_by_parent(proc: &Path, pid: libc::pid_t) -> io::Result<bool> {
        let mut saw_shell = false;
        for process in fs::read_dir(proc)? {
            let process = process?;
            let Some(process_pid) = process
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            let stat = fs::read(process.path().join("stat"))?;
            if parent_pid(&stat)? == pid {
                return Ok(false);
            }
            saw_shell |= process_pid == pid;
        }
        Ok(saw_shell)
    }

    fn parent_pid(stat: &[u8]) -> io::Result<libc::pid_t> {
        // comm is parenthesized and can contain spaces, ')' and non-UTF8.
        // The fields after its final ')' start with state, then parent pid.
        std::str::from_utf8(stat_field(stat, 1)?)
            .ok()
            .and_then(|parent| parent.parse().ok())
            .ok_or_else(invalid_process)
    }

    fn start_time(stat: &[u8]) -> io::Result<u64> {
        // Field 22; state is the first field after comm (field 3).
        std::str::from_utf8(stat_field(stat, 19)?)
            .ok()
            .and_then(|started| started.parse().ok())
            .ok_or_else(invalid_process)
    }

    fn stat_field(stat: &[u8], index: usize) -> io::Result<&[u8]> {
        let end = stat
            .iter()
            .rposition(|&byte| byte == b')')
            .ok_or_else(invalid_process)?;
        fields(&stat[end + 1..])
            .nth(index)
            .ok_or_else(invalid_process)
    }

    fn fields(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
        bytes
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
    }

    fn invalid_process() -> io::Error {
        io::Error::new(ErrorKind::InvalidData, "incomplete process information")
    }

    #[cfg(test)]
    mod tests {
        use super::{has_no_children, parent_pid, start_time};
        use std::fs;
        use std::path::PathBuf;

        struct Proc(PathBuf);

        impl Proc {
            fn new() -> Self {
                let root =
                    std::env::temp_dir().join(format!("muxel-proc-{}", uuid::Uuid::new_v4()));
                fs::create_dir_all(root.join("40/task/40")).unwrap();
                fs::create_dir_all(root.join(format!("self/task/{}", std::process::id()))).unwrap();
                fs::write(root.join("40/stat"), b"40 (bash) S 1 40 40").unwrap();
                let proc = Self(root);
                proc.mount_options("rw");
                proc
            }

            fn write(&self, path: &str, content: &[u8]) {
                let path = self.0.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, content).unwrap();
            }

            fn mount_options(&self, options: &str) {
                self.write(
                    "self/mountinfo",
                    format!("1 0 0:1 / /proc rw - proc proc {options}\n").as_bytes(),
                );
            }
        }

        impl Drop for Proc {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn children_of_every_shell_thread_count() {
            let proc = Proc::new();
            proc.write("40/task/40/children", b"");
            proc.write("40/task/41/children", b"51 ");
            assert!(!has_no_children(&proc.0, 40).unwrap());
        }

        #[test]
        fn missing_children_files_fall_back_to_parent_pids() {
            for options in ["rw", "rw,hidepid=0", "rw,hidepid=off"] {
                let proc = Proc::new();
                proc.mount_options(options);
                proc.write("7/stat", b"7 (unrelated) S 1 7 7");
                assert!(has_no_children(&proc.0, 40).unwrap());
                proc.write("51/stat", b"51 (sleep) S 40 51 40");
                assert!(!has_no_children(&proc.0, 40).unwrap());
                fs::remove_dir_all(proc.0.join("51")).unwrap();
                assert!(has_no_children(&proc.0, 40).unwrap());
            }
        }

        #[test]
        fn failed_reads_cannot_prove_idle() {
            let proc = Proc::new();
            // A directory in place of a file causes a read error even as root.
            fs::create_dir(proc.0.join("40/task/40/children")).unwrap();
            assert!(has_no_children(&proc.0, 40).is_err());
            fs::remove_dir(proc.0.join("40/task/40/children")).unwrap();
            proc.write("7/stat", b"unreadable parent fields");
            assert!(has_no_children(&proc.0, 40).is_err());
            fs::remove_dir_all(proc.0.join("7")).unwrap();
            assert!(!has_no_children(&proc.0, 99).unwrap());
        }

        #[test]
        fn stat_names_do_not_shift_parent_fields() {
            assert_eq!(parent_pid(b"51 (a name ) with \xff) S 40 51").unwrap(), 40);
            assert!(parent_pid(b"51 (no parent) S").is_err());
            assert_eq!(
                start_time(
                    b"40 (a name) S 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 987654321 23"
                )
                .unwrap(),
                987654321
            );
            assert!(start_time(b"40 (no start time) S 1").is_err());
        }

        #[test]
        fn hidden_processes_cannot_be_treated_as_absent_children() {
            for options in [
                "rw,hidepid=1",
                "rw,hidepid=2",
                "rw,hidepid=4",
                "rw,hidepid=invisible",
                "rw,pidns=123",
            ] {
                let proc = Proc::new();
                // Only the shell is visible. A restricted proc mount can omit
                // a live child without producing a read error for that child.
                proc.mount_options(options);
                assert!(!has_no_children(&proc.0, 40).unwrap_or(false), "{options}");
            }
        }

        #[test]
        fn fallback_needs_readable_mount_information() {
            let proc = Proc::new();
            for mountinfo in [
                b"".as_slice(),
                b"unreadable mount fields",
                b"1 0 0:1 /40 /proc rw - proc proc rw\n",
                b"1 0 0:1 / /somewhere-else rw - proc proc rw\n",
            ] {
                proc.write("self/mountinfo", mountinfo);
                assert!(!has_no_children(&proc.0, 40).unwrap_or(false));
            }
            fs::remove_file(proc.0.join("self/mountinfo")).unwrap();
            assert!(!has_no_children(&proc.0, 40).unwrap_or(false));
        }

        #[test]
        fn a_disappearing_task_does_not_select_the_fallback() {
            let proc = Proc::new();
            proc.write("40/task/40/children", b"");
            fs::create_dir_all(proc.0.join("40/task/41")).unwrap();
            proc.write(&format!("self/task/{}/children", std::process::id()), b"");
            assert!(!has_no_children(&proc.0, 40).unwrap_or(false));
        }

        #[test]
        fn a_disappearing_proc_entry_cannot_prove_idle() {
            let proc = Proc::new();
            fs::create_dir_all(proc.0.join("51")).unwrap();
            assert!(!has_no_children(&proc.0, 40).unwrap_or(false));
        }
    }
}
