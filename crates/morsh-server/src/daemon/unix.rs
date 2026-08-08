pub fn fork_and_detach(_port: u16) -> bool {
    tracing::info!("Forking to daemonize...");

    match unsafe { libc::fork() } {
        -1 => {
            eprintln!("morsh-server: fork failed");
            std::process::exit(1);
        }
        0 => {
            tracing::info!("Child forked OK, calling setsid()");
            unsafe { libc::setsid(); }
            tracing::info!("setsid() complete, child PID={}", std::process::id());
            true
        }
        pid => {
            tracing::info!("Parent (PID={}) exiting, child (PID={}) continues as daemon", std::process::id(), pid);
            std::process::exit(0);
        }
    }
}

pub fn redirect_stdio() {
    use std::os::unix::io::AsRawFd;

    tracing::info!("Redirecting stdio to /dev/null");
    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .expect("Failed to open /dev/null");

    unsafe {
        libc::dup2(devnull.as_raw_fd(), 0);
        libc::dup2(devnull.as_raw_fd(), 1);
        libc::dup2(devnull.as_raw_fd(), 2);
    }
}
