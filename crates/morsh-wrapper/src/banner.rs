//! Read an SSH server's identification banner without logging in.
//!
//! A server sends `SSH-2.0-<software>` as soon as the TCP connection opens
//! (RFC 4253 §4.2), and Windows servers say so there
//! (`SSH-2.0-OpenSSH_for_Windows_9.5`, Bitvise's `…WinSSHD…`). That is
//! enough to decide whether the launch command needs a POSIX shell without a
//! second ssh login.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);

/// Fetch the banner line, or `None` if the server can't be reached directly
/// or doesn't answer like an SSH server.
pub fn fetch(host: &str, port: u16) -> Option<String> {
    let mut addrs: Vec<_> = (host, port).to_socket_addrs().ok()?.collect();
    addrs.sort_by_key(|a| !a.is_ipv4());
    let stream = addrs
        .iter()
        .find_map(|a| TcpStream::connect_timeout(a, TIMEOUT).ok())?;
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;

    // Identify ourselves too, so the server logs an ordinary pre-auth
    // disconnect rather than a client that never spoke.
    let _ = (&stream).write_all(b"SSH-2.0-morsh_probe\r\n");

    read_banner(BufReader::new((&stream).take(8 * 1024)))
}

/// Find the `SSH-` line; servers may send other lines before it.
fn read_banner(mut reader: impl BufRead) -> Option<String> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.starts_with("SSH-") {
            return Some(line.trim_end().to_string());
        }
    }
}

/// Does the banner come from a Windows SSH server?
pub fn is_windows(banner: &str) -> bool {
    let banner = banner.to_ascii_lowercase();
    banner.contains("windows") || banner.contains("winsshd")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_servers_are_recognised() {
        assert!(is_windows("SSH-2.0-OpenSSH_for_Windows_9.5"));
        assert!(is_windows("SSH-2.0-9.31 FlowSsh: Bitvise SSH Server (WinSSHD) 9.31"));
        assert!(!is_windows("SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13.5"));
        assert!(!is_windows("SSH-2.0-dropbear_2022.83"));
    }

    #[test]
    fn lines_before_the_banner_are_skipped() {
        let input: &[u8] = b"Welcome to the jump box\r\nSSH-2.0-OpenSSH_9.6\r\n";
        assert_eq!(read_banner(input).as_deref(), Some("SSH-2.0-OpenSSH_9.6"));
        assert_eq!(read_banner(&b"not ssh\r\n"[..]), None);
    }
}
