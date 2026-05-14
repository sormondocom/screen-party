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
        /// Host-side stream cache duration in seconds (ring buffer for smooth client catch-up)
        #[arg(long, default_value_t = 10.0)]
        cache_secs: f32,
    },
    /// Connect to a screen sharing host
    Client {
        /// Hostname or IP address of the host
        #[arg(short = 'H', long)]
        host: String,
        /// Port the host is listening on
        #[arg(short, long, default_value_t = 7777)]
        port: u16,
        /// Display name shown to the host and in chat (e.g. "Alice")
        #[arg(short, long)]
        name: Option<String>,
    },
}
