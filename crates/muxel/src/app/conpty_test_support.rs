use muxel_terminal::TerminalSession;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

/// Keep the fixture's process identity after exit, independently of PTY delivery.
pub(super) struct ChildHandle(HANDLE);

impl ChildHandle {
    pub(super) fn new(session: &TerminalSession) -> Self {
        // SAFETY: retain the process created by the fixture before waiting, so
        // PID reuse cannot satisfy a later liveness assertion.
        Self(unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                session.child_pid().unwrap(),
            )
            .unwrap()
        })
    }

    /// The process `pid` names now; `None` if no process has that PID.
    pub(super) fn open(pid: u32) -> Option<Self> {
        // SAFETY: the returned guard owns the handle.
        unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .ok()
        .map(Self)
    }

    pub(super) fn running(&self) -> bool {
        // SAFETY: this guard owns the retained handle until Drop.
        let status = unsafe { WaitForSingleObject(self.0, 0) };
        if status == WAIT_TIMEOUT {
            true
        } else {
            assert_eq!(status, WAIT_OBJECT_0);
            false
        }
    }

    /// Whether the process has exited by `timeout`. Windows signals the handle
    /// once the process is gone, with every handle it held closed, its current
    /// folder's included.
    pub(super) fn exits_within(&self, timeout: Duration) -> bool {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: this guard owns the retained handle until Drop.
        unsafe { WaitForSingleObject(self.0, millis) == WAIT_OBJECT_0 }
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns the retained process handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}
