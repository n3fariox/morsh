#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    // Keep the pipe open for subsequent startup messages as well as CONNECT.
    _stdout: Option<BufReader<ChildStdout>>,
}

impl Server {
    fn start(timeout: &str) -> Self {
        Self::with_signal_timeout(timeout, "0")
    }

    fn with_signal_timeout(timeout: &str, signal_timeout: &str) -> Self {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port().to_string();
        drop(socket);
        let mut child = Command::new(env!("CARGO_BIN_EXE_morsh-server"))
            .args(["new", "-D", "-i", "127.0.0.1", "-p", &port])
            .env("MOSH_SERVER_NETWORK_TMOUT", timeout)
            .env("MOSH_SERVER_SIGNAL_TMOUT", signal_timeout)
            .env_remove("MORSH_DAEMON_CHILD")
            .env_remove("MORSH_KEY")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        stdout.read_line(&mut line).unwrap();
        assert!(line.starts_with("MOSH CONNECT "), "{line}");
        // The connect line is currently printed before the runtime starts.
        let deadline = Instant::now() + Duration::from_secs(3);
        while std::net::UdpSocket::bind(("127.0.0.1", port.parse::<u16>().unwrap())).is_ok() {
            assert!(Instant::now() < deadline, "server never bound UDP socket");
            assert!(
                child.try_wait().unwrap().is_none(),
                "server exited during startup"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            child,
            _stdout: Some(stdout),
        }
    }

    fn wait(&mut self) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn network_timeout_applies_before_first_client() {
    let mut server = Server::start("1");
    assert!(
        server
            .wait()
            .expect("network timeout must not wait the default 60 seconds")
            .success(),
        "network idle timeout must exit successfully like stock mosh"
    );
    let mut stderr = String::new();
    server
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let seconds = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("Network idle for ")?
                .strip_suffix(" seconds.")?
                .parse::<u64>()
                .ok()
        })
        .expect("missing network idle diagnostic");
    assert!(seconds >= 1, "{stderr}");
}

#[test]
fn sigusr1_without_client_shuts_down_cleanly() {
    let mut server = Server::start("0");
    assert_eq!(
        unsafe { libc::kill(server.child.id() as i32, libc::SIGUSR1) },
        0
    );
    assert!(server.wait().expect("server ignored SIGUSR1").success());
}

#[test]
fn sigusr1_respects_signal_idle_timeout() {
    let mut server = Server::with_signal_timeout("0", "1");
    assert_eq!(
        unsafe { libc::kill(server.child.id() as i32, libc::SIGUSR1) },
        0
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        server.child.try_wait().unwrap().is_none(),
        "recent state must protect session"
    );
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        unsafe { libc::kill(server.child.id() as i32, libc::SIGUSR1) },
        0
    );
    assert!(server
        .wait()
        .expect("server ignored idle SIGUSR1")
        .success());
}

#[test]
fn sigterm_without_client_shuts_down_cleanly() {
    let mut server = Server::start("0");
    assert_eq!(
        unsafe { libc::kill(server.child.id() as i32, libc::SIGTERM) },
        0
    );
    assert!(server.wait().expect("server ignored SIGTERM").success());
}

#[test]
fn closed_startup_stdout_does_not_panic() {
    // Gate execution on stdin so the stdout pipe is closed before the server
    // can print its banner, making the EPIPE regression deterministic.
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            "read ready; exec \"$@\"",
            "morsh-test",
            env!("CARGO_BIN_EXE_morsh-server"),
            "new",
            "-D",
            "-i",
            "127.0.0.1",
            "-p",
            "0",
        ])
        .env("MOSH_SERVER_NETWORK_TMOUT", "1")
        .env_remove("MORSH_DAEMON_CHILD")
        .env_remove("MORSH_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    child.stdin.take().unwrap().write_all(b"ready\n").unwrap();
    let mut server = Server {
        child,
        _stdout: None,
    };
    let status = server
        .wait()
        .expect("server did not finish after idle timeout");
    let mut stderr = String::new();
    server
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "startup stdout must not panic: {stderr}");
}
