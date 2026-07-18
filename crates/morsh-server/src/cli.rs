use clap::Parser;
use std::process;

/// Starts a morsh-server that listens for a morsh-client connection.
#[derive(Parser, Debug)]
#[command(name = "morsh-server", version)]
pub struct Cli {
    /// Optional "new" subcommand for stock mosh compatibility
    #[command(subcommand)]
    pub command: Option<NewSubcommand>,

    /// Bind to this port/range (default: random)
    #[arg(short = 'p', value_name = "PORT[:PORT2]")]
    pub port: Option<u16>,

    /// Bind to this address (default: 0.0.0.0)
    #[arg(short = 'i', value_name = "LOCALADDR")]
    pub bind_addr: Option<String>,

    /// Use SSH_CONNECTION for bind IP
    #[arg(short = 's')]
    pub use_ssh_connection: bool,

    /// Run in foreground (morsh extension)
    #[arg(short = 'D', long = "no-daemonize")]
    pub no_daemonize: bool,

    /// Set locale-related environment variable (repeatable)
    #[arg(short = 'l', value_name = "NAME=VALUE")]
    pub locale_vars: Vec<String>,

    /// Write logs to FILE (morsh extension)
    #[arg(long = "log-file", value_name = "FILE")]
    pub log_file: Option<String>,

    /// Terminal color count (ignored)
    #[arg(short = 'c', value_name = "COLORS", hide = true)]
    pub color_count: Option<u32>,

    /// Command and arguments to run (everything after --)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command_args: Vec<String>,
}

#[derive(Parser, Debug)]
pub enum NewSubcommand {
    /// Start a new server (stock mosh compatibility)
    New,
}

#[derive(Debug)]
pub struct CliOptions {
    pub port: u16,
    pub bind_addr: Option<String>,
    pub use_ssh_connection: bool,
    pub no_daemonize: bool,
    pub log_file: Option<String>,
    pub locale_vars: Vec<(String, String)>,
    pub command_args: Vec<String>,
}

pub fn parse() -> CliOptions {
    let cli = Cli::parse();

    let locale_vars: Vec<(String, String)> = cli
        .locale_vars
        .iter()
        .map(|val| {
            let Some((name, value)) = val.split_once('=') else {
                eprintln!("morsh-server: -l argument must be NAME=VALUE, got '{val}'");
                process::exit(1);
            };
            (name.to_string(), value.to_string())
        })
        .collect();

    CliOptions {
        port: cli.port.unwrap_or(0),
        bind_addr: cli.bind_addr,
        use_ssh_connection: cli.use_ssh_connection,
        no_daemonize: cli.no_daemonize,
        log_file: cli.log_file,
        locale_vars,
        command_args: cli.command_args,
    }
}
