pub fn fork_and_detach(_port: u16) -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, STARTUPINFOW, PROCESS_INFORMATION, DETACHED_PROCESS,
        CREATE_BREAKAWAY_FROM_JOB,
    };

    log::info!("Spawning detached child process via CreateProcess");

    let exe = std::env::current_exe().expect("morsh-server: failed to get executable path");

    let args: Vec<String> = std::env::args().collect();
    let mut cmd_line = exe.display().to_string();
    if cmd_line.contains(' ') {
        cmd_line = format!("\"{}\"", cmd_line);
    }
    for arg in &args[1..] {
        cmd_line.push(' ');
        if arg.contains(' ') {
            cmd_line.push_str(&format!("\"{}\"", arg));
        } else {
            cmd_line.push_str(arg);
        }
    }
    let mut cmd_wide: Vec<u16> =
        OsStr::new(&cmd_line).encode_wide().chain(Some(0)).collect();

    let exe_wide: Vec<u16> =
        OsStr::new(exe.as_os_str()).encode_wide().chain(Some(0)).collect();

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let result = unsafe {
        CreateProcessW(
            exe_wide.as_ptr(),
            cmd_wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        )
    };

    if result == 0 {
        let err = unsafe { GetLastError() };
        eprintln!(
            "morsh-server: WARNING failed to spawn daemon child (error {}), \
             falling back to in-process mode (will exit when SSH disconnects)",
            err
        );
        return false;
    }

    unsafe {
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }

    std::process::exit(0);
}

pub fn redirect_stdio() {
    use std::os::windows::io::AsRawHandle;

    let nul = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("NUL")
        .expect("Failed to open NUL");

    let handle = nul.as_raw_handle();

    unsafe {
        windows_sys::Win32::System::Console::SetStdHandle(
            windows_sys::Win32::System::Console::STD_INPUT_HANDLE,
            handle,
        );
        windows_sys::Win32::System::Console::SetStdHandle(
            windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE,
            handle,
        );
        windows_sys::Win32::System::Console::SetStdHandle(
            windows_sys::Win32::System::Console::STD_ERROR_HANDLE,
            handle,
        );
    }

    std::mem::forget(nul);
}
