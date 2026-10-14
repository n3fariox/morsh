//! `morsh --fake-proxy -- HOST PORT`: mosh's trick for learning which address
//! ssh really connects to.
//!
//! The wrapper runs ssh with `ProxyCommand=morsh --fake-proxy -- %h %p`, so
//! ssh itself expands aliases, `HostName` and `Port` and hands us the result.
//! We open the TCP connection, announce the address on stderr as
//! `MOSH IP <addr>` (the wrapper sends its UDP there), then shuttle bytes
//! between ssh and the socket like netcat.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::process::exit;

/// The flag that selects this mode; checked before normal argument parsing.
pub const FLAG: &str = "--fake-proxy";

/// The `ProxyCommand` value that makes ssh run this mode, or `None` if our
/// own path is unknown.
pub fn proxy_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.to_str()?;
    // ssh runs the command through `$SHELL -c` on Unix and parses it itself
    // on Windows, where double quotes are what it understands.
    #[cfg(windows)]
    let exe = format!("\"{exe}\"");
    #[cfg(not(windows))]
    let exe = crate::shell_quote(exe);
    // `%` would otherwise start an ssh token.
    let exe = exe.replace('%', "%%");
    Some(format!("{exe} {FLAG} -- %h %p"))
}

/// Pick the address out of a `MOSH IP <addr>` line.
pub fn parse_ip_line(line: &str) -> Option<&str> {
    line.trim_end().strip_prefix("MOSH IP ").map(str::trim)
}

/// Run the proxy; `args` are the arguments after `--fake-proxy`.
pub fn run(args: &[String]) -> ! {
    let args: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--").collect();
    let (host, port) = match args[..] {
        [host, port] => match port.parse::<u16>() {
            Ok(port) => (host, port),
            Err(_) => fail(&format!("bad port '{port}'")),
        },
        _ => fail("usage: morsh --fake-proxy -- HOST PORT"),
    };

    let stream = connect(host, port);
    let ip = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
    eprintln!("MOSH IP {ip}");

    let mut to_server = stream.try_clone().unwrap_or_else(|e| fail(&e.to_string()));
    std::thread::spawn(move || {
        pump(&mut std::io::stdin().lock(), &mut to_server);
        let _ = to_server.shutdown(Shutdown::Write);
    });
    pump(&mut &stream, &mut std::io::stdout().lock());
    exit(0);
}

/// Connect to the first reachable address, IPv4 first (mosh's default
/// `--family=prefer-inet`).
fn connect(host: &str, port: u16) -> TcpStream {
    let mut addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .unwrap_or_else(|e| fail(&format!("could not resolve {host}: {e}")))
        .collect();
    addrs.sort_by_key(|a| !a.is_ipv4());

    let mut last = String::from("no addresses");
    for addr in addrs {
        match TcpStream::connect(addr) {
            Ok(s) => return s,
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    fail(&format!("could not connect to {host}, last tried {last}"))
}

/// Copy until EOF, flushing every chunk: ssh is waiting on each handshake
/// message, and a buffered stdout would hold it back.
fn pump(from: &mut impl Read, to: &mut impl Write) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).and_then(|()| to.flush()).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("morsh: {msg}");
    exit(255);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ip_line_is_recognised() {
        assert_eq!(parse_ip_line("MOSH IP 203.0.113.9\r\n"), Some("203.0.113.9"));
        assert_eq!(parse_ip_line("MOSH IP 2001:db8::1"), Some("2001:db8::1"));
        assert_eq!(parse_ip_line("Warning: Permanently added"), None);
    }

    #[test]
    fn the_proxy_command_passes_sshs_host_and_port_tokens() {
        let cmd = proxy_command().unwrap();
        assert!(cmd.ends_with(" --fake-proxy -- %h %p"), "{cmd}");
    }
}
