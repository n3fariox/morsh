//! Command-line parsing for `morsh-server`.
//!
//! The grammar is the one `mosh.pl` emits and `mosh-server.cc` accepts:
//!
//! ```text
//! mosh-server new -c <colors> [-s | -i <addr>] [-p <port[:port2]>] [-l NAME=VALUE]... [-- <command>]
//! ```
//!
//! `new` is a bare positional token, not a subcommand with its own flag set, so
//! every flag can be combined with it.  `--` is mandatory to introduce a
//! command; anything before it that isn't a recognised flag is a legacy
//! positional (`mosh-server <ip> [port]`).
//!
//! This is hand-rolled rather than clap-derived because clap cannot express a
//! bare `new` token that is followed by the full option set.

use std::process;

/// Parsed command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOptions {
    /// Low end of the requested bind port range (0 = any).
    pub port_low: u16,
    /// High end of the requested bind port range (0 = any).
    pub port_high: u16,
    /// Explicit bind address from `-i`.
    pub bind_addr: Option<String>,
    /// `-s`: take the bind address from `$SSH_CONNECTION`.
    pub use_ssh_connection: bool,
    /// `-D` / `--no-daemonize` (morsh extension).
    pub no_daemonize: bool,
    /// `--log-file FILE` (morsh extension).
    pub log_file: Option<String>,
    /// `-l NAME=VALUE`, repeatable.
    pub locale_vars: Vec<(String, String)>,
    /// Command after `--`, if any.
    pub command_args: Vec<String>,
    /// `-c COLORS`, accepted and ignored (we always support 256 colors).
    pub colors: u32,
    /// `-v`, repeatable.
    pub verbose: u32,
}

/// What the caller should do instead of starting a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    Run(Box<CliOptions>),
    Help,
    Version,
    Error(String),
}

pub const USAGE: &str = "\
Usage: morsh-server new [-s | -i LOCALADDR] [-p PORT[:PORT2]] [-c COLORS]
                        [-l NAME=VALUE]... [-D] [--log-file FILE] [-- COMMAND...]

  -s               bind to the IP address in $SSH_CONNECTION
  -i LOCALADDR     bind to this address
  -p PORT[:PORT2]  bind to this UDP port (or the first free port in the range)
  -c COLORS        terminal color count (accepted, ignored)
  -l NAME=VALUE    set a locale environment variable for the child (repeatable)
  -v               increase verbosity (repeatable)
  -D, --no-daemonize   run in the foreground (morsh extension)
  --log-file FILE  write debug logs to FILE (morsh extension)
  -- COMMAND...    run COMMAND instead of the login shell
";

/// Parse `LOW` or `LOW:HIGH`, mirroring mosh's `Connection::parse_portrange`.
fn parse_portrange(s: &str) -> Result<(u16, u16), String> {
    let bad = || format!("Bad UDP port range ({s})");
    let (low_str, high_str) = match s.split_once(':') {
        Some((l, h)) => (l, h),
        None => (s, s),
    };
    let low: u32 = low_str.parse().map_err(|_| bad())?;
    let high: u32 = high_str.parse().map_err(|_| bad())?;
    if !(1..=65535).contains(&low) || !(1..=65535).contains(&high) || low > high {
        return Err(bad());
    }
    Ok((low as u16, high as u16))
}

/// Options that take a value when written in short form.
fn short_takes_value(c: char) -> bool {
    matches!(c, '@' | 'i' | 'p' | 'c' | 'l')
}

/// Parse the argument list (excluding argv\[0\]).
pub fn parse_args(args: &[String]) -> ParseOutcome {
    for a in args {
        match a.as_str() {
            "-h" | "--help" => return ParseOutcome::Help,
            "--version" => return ParseOutcome::Version,
            "--" => break,
            _ => {}
        }
    }

    let mut opts = CliOptions::default();

    // `--` is mandatory to introduce the command; everything after it is the
    // command, everything before it is options.
    let (head, command) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    opts.command_args = command;

    let new_syntax = head.first().map(|s| s.as_str()) == Some("new");
    let head: &[String] = if new_syntax { &head[1..] } else { head };

    if !new_syntax {
        // Legacy positional syntax used by very old client wrappers:
        //   morsh-server                → defaults
        //   morsh-server <ip>           → bind address
        //   morsh-server <ip> <port>    → bind address and port
        match head.len() {
            0 => return ParseOutcome::Run(Box::new(opts)),
            1 => {
                if head[0].starts_with('-') {
                    return ParseOutcome::Error(format!("unexpected argument '{}'", head[0]));
                }
                opts.bind_addr = Some(head[0].clone());
            }
            2 => {
                if head[0].starts_with('-') || head[1].starts_with('-') {
                    return ParseOutcome::Error("unexpected argument".to_string());
                }
                opts.bind_addr = Some(head[0].clone());
                match parse_portrange(&head[1]) {
                    Ok((lo, hi)) => {
                        opts.port_low = lo;
                        opts.port_high = hi;
                    }
                    Err(e) => return ParseOutcome::Error(e),
                }
            }
            _ => return ParseOutcome::Error("too many arguments".to_string()),
        }
        return ParseOutcome::Run(Box::new(opts));
    }

    let mut i = 0usize;
    while i < head.len() {
        let arg = head[i].as_str();
        i += 1;

        // Long options (morsh extensions).
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (rest, None),
            };
            match name {
                "no-daemonize" => opts.no_daemonize = true,
                "log-file" => {
                    let value = match inline {
                        Some(v) => v,
                        None => match head.get(i) {
                            Some(v) => {
                                i += 1;
                                v.clone()
                            }
                            None => {
                                return ParseOutcome::Error(
                                    "--log-file requires a FILE argument".into(),
                                )
                            }
                        },
                    };
                    opts.log_file = Some(value);
                }
                // mosh's getopt loop prints usage but does not die on unknown
                // options, so that a newer client can talk to an older server.
                other => eprintln!("morsh-server: ignoring unknown option '--{other}'"),
            }
            continue;
        }

        if arg == "-" || !arg.starts_with('-') {
            // A stray positional in `new` syntax.  mosh's getopt stops at the
            // first non-option; treat it the same way and ignore the rest.
            eprintln!("morsh-server: ignoring unexpected argument '{arg}'");
            continue;
        }

        // Clustered short options, e.g. `-sv` or `-c256`.
        let chars: Vec<char> = arg[1..].chars().collect();
        let mut ci = 0usize;
        while ci < chars.len() {
            let c = chars[ci];
            ci += 1;

            let value = if short_takes_value(c) {
                let inline: String = chars[ci..].iter().collect();
                ci = chars.len();
                if !inline.is_empty() {
                    Some(inline)
                } else {
                    match head.get(i) {
                        Some(v) => {
                            i += 1;
                            Some(v.clone())
                        }
                        None => {
                            return ParseOutcome::Error(format!("option -{c} requires an argument"))
                        }
                    }
                }
            } else {
                None
            };

            match c {
                // Undocumented mosh option that exists only to eat its argument.
                '@' => {}
                'i' => {
                    opts.bind_addr = value;
                    opts.use_ssh_connection = false;
                }
                's' => {
                    opts.use_ssh_connection = true;
                    opts.bind_addr = None;
                }
                'p' => match parse_portrange(value.as_deref().unwrap_or("")) {
                    Ok((lo, hi)) => {
                        opts.port_low = lo;
                        opts.port_high = hi;
                    }
                    Err(e) => return ParseOutcome::Error(e),
                },
                'c' => {
                    let raw = value.unwrap_or_default();
                    match raw.parse::<u32>() {
                        Ok(v) => opts.colors = v,
                        Err(_) => {
                            return ParseOutcome::Error(format!("Bad number of colors ({raw})"))
                        }
                    }
                }
                'l' => {
                    let raw = value.unwrap_or_default();
                    let Some((name, val)) = raw.split_once('=') else {
                        return ParseOutcome::Error(format!(
                            "-l argument must be NAME=VALUE, got '{raw}'"
                        ));
                    };
                    opts.locale_vars.push((name.to_string(), val.to_string()));
                }
                'v' => opts.verbose += 1,
                'D' => opts.no_daemonize = true,
                other => eprintln!("morsh-server: ignoring unknown option '-{other}'"),
            }
        }
    }

    ParseOutcome::Run(Box::new(opts))
}

/// Parse the process command line, exiting on `--help`, `--version`, or error.
pub fn parse() -> CliOptions {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        ParseOutcome::Run(opts) => *opts,
        ParseOutcome::Help => {
            print!("{USAGE}");
            process::exit(0);
        }
        ParseOutcome::Version => {
            println!("morsh-server (morsh) {}", env!("CARGO_PKG_VERSION"));
            process::exit(0);
        }
        ParseOutcome::Error(msg) => {
            eprintln!("morsh-server: {msg}");
            eprint!("{USAGE}");
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> CliOptions {
        match parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()) {
            ParseOutcome::Run(o) => *o,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    fn parse_err(args: &[&str]) -> String {
        match parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()) {
            ParseOutcome::Error(e) => e,
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// The literal invocation `mosh.pl` builds (mosh.pl:377-392).
    #[test]
    fn mosh_pl_invocation() {
        let opts = parse(&[
            "new",
            "-c",
            "256",
            "-s",
            "-l",
            "LANG=en_US.UTF-8",
            "-l",
            "LC_CTYPE=en_US.UTF-8",
        ]);
        assert_eq!(opts.colors, 256);
        assert!(opts.use_ssh_connection);
        assert_eq!(opts.bind_addr, None);
        assert_eq!(
            opts.locale_vars,
            vec![
                ("LANG".to_string(), "en_US.UTF-8".to_string()),
                ("LC_CTYPE".to_string(), "en_US.UTF-8".to_string()),
            ]
        );
        assert!(opts.command_args.is_empty());
    }

    #[test]
    fn mosh_pl_invocation_with_bind_ip_port_and_command() {
        let opts = parse(&[
            "new", "-c", "256", "-i", "10.0.0.5", "-p", "60000:60010", "-l", "LANG=C.UTF-8", "--",
            "htop", "-d", "5",
        ]);
        assert_eq!(opts.bind_addr.as_deref(), Some("10.0.0.5"));
        assert!(!opts.use_ssh_connection);
        assert_eq!((opts.port_low, opts.port_high), (60000, 60010));
        assert_eq!(opts.command_args, vec!["htop", "-d", "5"]);
    }

    #[test]
    fn bind_address_combines_with_locale_vars() {
        // The combination that no invocation could express before.
        let opts = parse(&["new", "-i", "1.2.3.4", "-l", "LANG=C.UTF-8"]);
        assert_eq!(opts.bind_addr.as_deref(), Some("1.2.3.4"));
        assert_eq!(opts.locale_vars.len(), 1);
    }

    #[test]
    fn single_port_is_a_range_of_one() {
        let opts = parse(&["new", "-p", "60001"]);
        assert_eq!((opts.port_low, opts.port_high), (60001, 60001));
    }

    #[test]
    fn morsh_extensions_are_accepted_in_new_syntax() {
        let opts = parse(&["new", "-s", "-D", "--log-file", "/tmp/x.log"]);
        assert!(opts.no_daemonize);
        assert_eq!(opts.log_file.as_deref(), Some("/tmp/x.log"));
    }

    #[test]
    fn log_file_accepts_equals_form() {
        let opts = parse(&["new", "--log-file=/tmp/y.log"]);
        assert_eq!(opts.log_file.as_deref(), Some("/tmp/y.log"));
    }

    #[test]
    fn clustered_and_inline_short_options() {
        let opts = parse(&["new", "-sv", "-c256"]);
        assert!(opts.use_ssh_connection);
        assert_eq!(opts.verbose, 1);
        assert_eq!(opts.colors, 256);
    }

    #[test]
    fn at_option_eats_its_argument() {
        let opts = parse(&["new", "-v", "-@", "new", "-c", "256"]);
        assert_eq!(opts.colors, 256);
        assert_eq!(opts.verbose, 1);
    }

    /// Regression: `-i 1.2.3.4` used to be swallowed by `trailing_var_arg`
    /// and run `1.2.3.4` as the command.  Stock mosh rejects it too (it parses
    /// as the legacy `<ip> <port>` form and the port fails to parse), so an
    /// error — not an execution — is the correct outcome.
    #[test]
    fn bind_address_without_new_is_not_treated_as_a_command() {
        assert!(!parse_err(&["-i", "1.2.3.4"]).is_empty());
        // With `new`, it means what it says.
        assert_eq!(
            parse(&["new", "-i", "1.2.3.4"]).bind_addr.as_deref(),
            Some("1.2.3.4")
        );
        assert!(parse(&["new", "-i", "1.2.3.4"]).command_args.is_empty());
    }

    #[test]
    fn legacy_positional_syntax() {
        let opts = parse(&["10.0.0.1"]);
        assert_eq!(opts.bind_addr.as_deref(), Some("10.0.0.1"));
        assert!(opts.command_args.is_empty());

        let opts = parse(&["10.0.0.1", "60000"]);
        assert_eq!(opts.bind_addr.as_deref(), Some("10.0.0.1"));
        assert_eq!((opts.port_low, opts.port_high), (60000, 60000));
    }

    #[test]
    fn no_arguments_is_valid() {
        let opts = parse(&[]);
        assert_eq!(opts, CliOptions::default());
    }

    #[test]
    fn later_bind_flag_wins() {
        assert!(parse(&["new", "-i", "1.2.3.4", "-s"]).use_ssh_connection);
        assert_eq!(
            parse(&["new", "-s", "-i", "1.2.3.4"]).bind_addr.as_deref(),
            Some("1.2.3.4")
        );
    }

    #[test]
    fn command_after_double_dash_keeps_its_own_flags() {
        let opts = parse(&["new", "-s", "--", "vim", "-u", "NONE"]);
        assert_eq!(opts.command_args, vec!["vim", "-u", "NONE"]);
        assert!(opts.use_ssh_connection);
    }

    #[test]
    fn help_and_version_are_detected() {
        assert_eq!(
            parse_args(&["--help".to_string()]),
            ParseOutcome::Help
        );
        assert_eq!(
            parse_args(&["new".to_string(), "-h".to_string()]),
            ParseOutcome::Help
        );
        assert_eq!(
            parse_args(&["--version".to_string()]),
            ParseOutcome::Version
        );
        // ...but not when they are part of the command.
        assert_eq!(
            parse(&["new", "--", "man", "--help"]).command_args,
            vec!["man", "--help"]
        );
    }

    #[test]
    fn bad_values_are_rejected() {
        assert!(parse_err(&["new", "-p", "0"]).contains("port range"));
        assert!(parse_err(&["new", "-p", "70000"]).contains("port range"));
        assert!(parse_err(&["new", "-p", "600:500"]).contains("port range"));
        assert!(parse_err(&["new", "-c", "many"]).contains("colors"));
        assert!(parse_err(&["new", "-l", "NOEQUALS"]).contains("NAME=VALUE"));
    }

    #[test]
    fn unknown_options_do_not_kill_the_server() {
        // mosh's getopt default arm prints usage but keeps going.
        let opts = parse(&["new", "-Z", "-s"]);
        assert!(opts.use_ssh_connection);
    }
}
