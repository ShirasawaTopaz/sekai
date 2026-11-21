use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use std::ptr;

const CREATE_SUSPENDED: u32 = 4;
const TH32CS_SNAPTHREAD: u32 = 4;
const THREAD_SUSPEND_RESUME: u32 = 2;
const ERROR_NO_MORE_FILES: i32 = 18;

#[repr(C)]
struct ThreadEntry {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> RawHandle;
    fn AssignProcessToJobObject(job: RawHandle, process: RawHandle) -> i32;
    fn TerminateJobObject(job: RawHandle, exit_code: u32) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> RawHandle;
    fn Thread32First(snapshot: RawHandle, entry: *mut ThreadEntry) -> i32;
    fn Thread32Next(snapshot: RawHandle, entry: *mut ThreadEntry) -> i32;
    fn OpenThread(access: u32, inherit: i32, thread_id: u32) -> RawHandle;
    fn ResumeThread(thread: RawHandle) -> u32;
}

pub(super) struct Job(OwnedHandle);

impl Job {
    pub(super) fn terminate(&self) {
        // SAFETY: The job handle remains owned by self.
        unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
    }
}

pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, Job)> {
    // SAFETY: Null arguments request an unnamed, non-inheritable job.
    let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateJobObjectW returned a valid owned handle.
    let job = Job(unsafe { OwnedHandle::from_raw_handle(handle) });
    // Assign before resuming so every descendant starts in the job.
    let mut child = command.creation_flags(CREATE_SUSPENDED).spawn()?;
    // SAFETY: Both handles remain valid for the call.
    let assigned =
        unsafe { AssignProcessToJobObject(job.0.as_raw_handle(), child.as_raw_handle()) };
    let started = if assigned == 0 {
        Err(io::Error::last_os_error())
    } else {
        resume_initial_thread(child.id())
    };
    if let Err(error) = started {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, job))
}

fn resume_initial_thread(pid: u32) -> io::Result<()> {
    // SAFETY: The thread snapshot covers the system and ignores the PID argument.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == (-1isize) as RawHandle {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The snapshot handle is valid and owned.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = ThreadEntry {
        size: size_of::<ThreadEntry>() as u32,
        usage: 0,
        thread_id: 0,
        owner_process_id: 0,
        base_priority: 0,
        delta_priority: 0,
        flags: 0,
    };
    // SAFETY: entry has the Win32 layout and supplies its size.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while found != 0 {
        if entry.owner_process_id == pid {
            // SAFETY: The initial thread belongs to the suspended child.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: OpenThread returned a valid owned handle.
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        entry.size = size_of::<ThreadEntry>() as u32;
        // SAFETY: The snapshot is owned and entry is initialized.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "shell thread not found",
        ));
    }
    Err(error)
}
