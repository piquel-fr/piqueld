use std::path::PathBuf;

use clap::{ArgGroup, Parser, Subcommand};

pub fn parse() -> Cli {
    Cli::parse()
}

/// CLI client for interacting with piqueld
#[derive(Parser, Debug)]
#[command(name = "piquelctl")]
#[command(about = "CLI for piqueld", long_about = None)]
#[command(group(
    ArgGroup::new("transport")
        .args(["uds", "tcp"])
        .multiple(false)  // ensures mutual exclusivity
        .required(false)  // neither is required
))]
pub struct Cli {
    #[arg(short = 'v', long = "verbose", global = true)]
    pub verbose: bool,
    /// Custom path to configuration
    #[arg(long = "config", value_name = "path", global = true)]
    pub config_path: Option<PathBuf>,

    /// Connect to a remote daemon over TCP (e.g. 127.0.0.1:7854)
    #[arg(short = 'H', long = "host", value_name = "HOST", global = true)]
    pub host: Option<String>,

    /// Path to the Unix socket to connect to
    #[arg(short = 's', long = "socket", value_name = "sock", global = true)]
    pub socket: Option<PathBuf>,

    /// Use Unix Domain Socket transport
    #[arg(long = "uds", global = true)]
    pub uds: bool,
    /// Use TCP transport
    #[arg(long = "tcp", global = true)]
    pub tcp: bool,

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
