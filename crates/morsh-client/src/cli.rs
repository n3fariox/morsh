//! Command-line parsing for `morsh-client`.
//!
//! The grammar is deliberately tiny — `morsh-client <host:port>` plus the two
//! informational flags — because the wrapper builds the argv, not a human.

use std::net::SocketAddr;

pub const USAGE: &str = "Usage: morsh-client <server-ip:port>";

pub const HELP: &str = "\
Usage: morsh-client <server-ip:port>

Connects to a morsh-server using the morsh state synchronization protocol.

Options:
  -h, --help       Show this help message
  -v, --version    Show version information

Environment:
  MORSH_KEY                Base64 encryption key (from morsh-server output)
  MOSH_PREDICTION_DISPLAY  always | never | adaptive (default)";

/// What the caller should do with the argv it was given.
#[derive(Debug)]
pub enum ParseOutcome {
    Connect(SocketAddr),
    Help,
    Version,
    Error(String),
}

/// Parse `args` as passed to the process, including `argv[0]`.
pub fn parse(args: &[String]) -> ParseOutcome {
    match args.get(1).map(String::as_str) {
        Some("-h") | Some("--help") => return ParseOutcome::Help,
        Some("-v") | Some("--version") => return ParseOutcome::Version,
        None => {
            return ParseOutcome::Error(format!(
                "{USAGE}\n  Environment: MORSH_KEY (base64 key from morsh-server)"
            ))
        }
        Some(_) => {}
    }

    match args[1].parse::<SocketAddr>() {
        Ok(addr) => ParseOutcome::Connect(addr),
        Err(e) => ParseOutcome::Error(format!("Invalid server address '{}': {}", args[1], e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("morsh-client")
            .chain(rest.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn an_address_is_parsed() {
        match parse(&argv(&["127.0.0.1:60001"])) {
            ParseOutcome::Connect(addr) => {
                assert_eq!(addr.port(), 60001);
                assert_eq!(addr.ip().to_string(), "127.0.0.1");
            }
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn an_ipv6_address_is_parsed() {
        match parse(&argv(&["[::1]:60001"])) {
            ParseOutcome::Connect(addr) => assert_eq!(addr.port(), 60001),
            other => panic!("expected Connect, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_host_is_an_error_not_a_panic() {
        assert!(matches!(
            parse(&argv(&["localhost"])),
            ParseOutcome::Error(_)
        ));
    }

    #[test]
    fn no_arguments_is_an_error() {
        assert!(matches!(parse(&argv(&[])), ParseOutcome::Error(_)));
    }

    #[test]
    fn informational_flags_are_recognized() {
        assert!(matches!(parse(&argv(&["-h"])), ParseOutcome::Help));
        assert!(matches!(parse(&argv(&["--help"])), ParseOutcome::Help));
        assert!(matches!(parse(&argv(&["-v"])), ParseOutcome::Version));
        assert!(matches!(
            parse(&argv(&["--version"])),
            ParseOutcome::Version
        ));
    }
}
