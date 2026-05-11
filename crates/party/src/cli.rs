use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "party", about = "Screen Party — live screen sharing")]
pub struct Cli {
    #[command(subcommand)]
    pub mode: Mode,
}

#[derive(Subcommand)]
pub enum Mode {
    /// Host a screen sharing session
    Host {
        /// TCP port to listen on
        #[arg(short, long, default_value_t = 7777)]
        port: u16,
        /// Generate (or overwrite) a PGP identity before starting
        #[arg(long)]
        generate_key: bool,
        /// Require clients to perform interactive key-exchange confirmation
        #[arg(short, long)]
        interactive: bool,
        /// Require explicit /approve for each connecting client before they see the stream
        #[arg(short = 'A', long)]
        approve: bool,
    },
    /// Connect to a screen sharing host
    Client {
        /// Hostname or IP address of the host
        #[arg(short = 'H', long)]
        host: String,
        /// Port the host is listening on
        #[arg(short, long, default_value_t = 7777)]
        port: u16,
        /// Interactively confirm the host's key fingerprint before connecting
        #[arg(short, long)]
        interactive: bool,
        /// Display name shown to the host and in chat (e.g. "Alice")
        #[arg(short, long)]
        name: Option<String>,
    },
}
