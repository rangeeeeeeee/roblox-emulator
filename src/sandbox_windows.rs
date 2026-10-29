use std::{ffi::{c_char, c_void, CStr}, fs::File, io, os::windows::{ffi::OsStrExt, io::FromRawHandle}, ptr};

#[repr(C)]
struct Handles {
    sandbox: *mut c_void,
    input: *mut c_void,
    output: *mut c_void,
}

unsafe extern "C" {
    fn emu_sandbox_spawn(exe: *const u16, profile: *const u16, handles: *mut Handles, stage: *mut c_char, stage_size: usize) -> u32;
    fn emu_sandbox_close(sandbox: *mut c_void);
    fn emu_sandbox_verify_current(stage: *mut c_char, stage_size: usize) -> u32;
}

pub(crate) struct WorkerProcess {
    pub(crate) stdin: File,
    pub(crate) stdout: File,
    sandbox: *mut c_void,
}

impl Drop for WorkerProcess {
    fn drop(&mut self) { unsafe { emu_sandbox_close(self.sandbox); } }
}

fn checked_error(code: u32, stage: &[c_char]) -> io::Result<()> {
    if code == 0 { return Ok(()); }
    let operation = unsafe { CStr::from_ptr(stage.as_ptr()) }.to_string_lossy();
    Err(io::Error::other(format!("Sandbox {operation}: {}", io::Error::from_raw_os_error(code as i32))))
}

pub(crate) fn spawn_worker() -> io::Result<WorkerProcess> {
    let executable = std::env::current_exe()?;
    let exe: Vec<u16> = executable.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let profile = format!("com.robloxemu.worker.{}", nonce.iter().map(|byte| format!("{byte:02x}")).collect::<String>());
    let profile: Vec<u16> = profile.encode_utf16().chain(Some(0)).collect();
    let mut handles = Handles { sandbox: ptr::null_mut(), input: ptr::null_mut(), output: ptr::null_mut() };
    let mut stage = [0 as c_char; 128];
    let code = unsafe { emu_sandbox_spawn(exe.as_ptr(), profile.as_ptr(), &mut handles, stage.as_mut_ptr(), stage.len()) };
    checked_error(code, &stage)?;
    if handles.sandbox.is_null() || handles.input.is_null() || handles.output.is_null() {
        if !handles.sandbox.is_null() { unsafe { emu_sandbox_close(handles.sandbox); } }
        return Err(io::Error::other("Sandbox returned invalid handles"));
    }
    Ok(WorkerProcess {
        stdin: unsafe { File::from_raw_handle(handles.input) },
        stdout: unsafe { File::from_raw_handle(handles.output) },
        sandbox: handles.sandbox,
    })
}

pub(crate) fn verify_worker_security() -> io::Result<()> {
    let mut stage = [0 as c_char; 128];
    let code = unsafe { emu_sandbox_verify_current(stage.as_mut_ptr(), stage.len()) };
    checked_error(code, &stage)
}
