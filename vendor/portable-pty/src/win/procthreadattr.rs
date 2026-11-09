use crate::win::psuedocon::HPCON;
use anyhow::{ensure, Error};
use std::io::Error as IoError;
use std::{mem, ptr};
use winapi::shared::minwindef::DWORD;
use winapi::um::processthreadsapi::*;
use winapi::um::winnt::HANDLE;

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x00020016;
/// `ProcThreadAttributeValue(ProcThreadAttributeJobList, FALSE, TRUE, FALSE)`.
const PROC_THREAD_ATTRIBUTE_JOB_LIST: usize = 0x0002000D;

pub struct ProcThreadAttributeList {
    data: Vec<u8>,
    /// `UpdateProcThreadAttribute` stores a pointer to the value, not a copy;
    /// the job handles must stay put until `CreateProcessW` returns.
    jobs: Box<[HANDLE; 1]>,
}

impl ProcThreadAttributeList {
    pub fn with_capacity(num_attributes: DWORD) -> Result<Self, Error> {
        let mut bytes_required: usize = 0;
        unsafe {
            InitializeProcThreadAttributeList(
                ptr::null_mut(),
                num_attributes,
                0,
                &mut bytes_required,
            )
        };
        let mut data = Vec::with_capacity(bytes_required);
        // We have the right capacity, so force the vec to consider itself
        // that length.  The contents of those bytes will be maintained
        // by the win32 apis used in this impl.
        unsafe { data.set_len(bytes_required) };

        let attr_ptr = data.as_mut_slice().as_mut_ptr() as *mut _;
        let res = unsafe {
            InitializeProcThreadAttributeList(attr_ptr, num_attributes, 0, &mut bytes_required)
        };
        ensure!(
            res != 0,
            "InitializeProcThreadAttributeList failed: {}",
            IoError::last_os_error()
        );
        Ok(Self {
            data,
            jobs: Box::new([ptr::null_mut()]),
        })
    }

    pub fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.data.as_mut_slice().as_mut_ptr() as *mut _
    }

    pub fn set_pty(&mut self, con: HPCON) -> Result<(), Error> {
        let res = unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                con,
                mem::size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        ensure!(
            res != 0,
            "UpdateProcThreadAttribute failed: {}",
            IoError::last_os_error()
        );
        Ok(())
    }

    /// Create the process inside `job` (Windows 10+). Requires a list made
    /// with room for this attribute.
    pub fn set_job(&mut self, job: HANDLE) -> Result<(), Error> {
        self.jobs[0] = job;
        let res = unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST,
                self.jobs.as_mut_ptr() as *mut _,
                mem::size_of::<[HANDLE; 1]>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        ensure!(
            res != 0,
            "UpdateProcThreadAttribute(job list) failed: {}",
            IoError::last_os_error()
        );
        Ok(())
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}
