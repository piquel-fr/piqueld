use clap::{Parser, Subcommand};
use piquelcore::config::SOCKET_PATH;

pub fn parse() -> Cli {
    Cli::parse()
}

/// CLI client for interacting with piqueld
#[derive(Parser, Debug)]
#[command(name = "piquelctl")]
#[command(about = "CLI for piqueld", long_about = None)]
pub struct Cli {
    /// Connect to a remote daemon over TCP (e.g. 127.0.0.1:7854)
    #[arg(short = 'H', long = "host", value_name = "HOST", global = true)]
    pub host: Option<String>,

    /// Path to the Unix socket to connect to
    #[arg(
        short = 's',
        long = "socket",
        value_name = "SOCK",
        default_value = SOCKET_PATH,
        global = true
    )]
    pub socket: String,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Returns the hostname of the daemon
    Hostname,
    /// Just echoes the message
    Echo { message: String },
}
