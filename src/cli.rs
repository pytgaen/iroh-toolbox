use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    // Usage lines name the command as documented, not `iroh-toolbox.exe`.
    bin_name = "iroh-toolbox",
    version = concat!("v", env!("CARGO_PKG_VERSION")),
    long_version = include_str!(concat!(env!("OUT_DIR"), "/version.txt")),
    about = "Transfer files, chat, stream data and reach remote TCP services; diagnose connections"
)]
pub struct Cli {
    #[command(flatten)]
    pub options: Options,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Default)]
#[command(next_help_heading = "Global options")]
pub struct Options {
    /// TOML configuration file (or IROH_TOOLBOX_CONFIG).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    /// Use only this relay instead of iroh's public relays (or IROH_TOOLBOX_RELAY).
    #[arg(long, global = true)]
    pub relay: Option<String>,
    /// Connection timeout in seconds (default: 15).
    #[arg(long, global = true, value_parser = seconds)]
    pub timeout: Option<f64>,
    /// Disable direct UDP transports; use relays exclusively (public by default).
    #[arg(long, global = true)]
    pub relay_only: bool,
    /// Disable iroh relays; require reachable direct addresses in the ticket.
    #[arg(long, global = true, conflicts_with_all = ["relay", "relay_only"])]
    pub direct_only: bool,
    /// Hide copyable remote commands on stderr; ticket/data outputs are unchanged.
    #[arg(long, global = true)]
    pub no_hints: bool,
    /// More detail on stderr: -v every step and connection diagnostics, -vv raw periodic counters.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Subcommand)]
pub enum Command {
    /// Send a file or directory: print a one-use code, or push it to a paired machine's inbox.
    #[command(group(ArgGroup::new("peer_offer").args(["peers_only", "allow"]).multiple(true)))]
    Send {
        /// File or directory to send.
        path: PathBuf,
        /// peer:NAME[/SLOT] of a paired machine running `receive --inbox` (default slot: inbox); omit to print a one-use code.
        #[arg(conflicts_with_all = ["peers_only", "share", "allow", "slot", "port"])]
        target: Option<String>,
        /// Direct address HOST:PORT of the inbox, as printed by it; repeat for several. Required with --direct-only.
        #[arg(
            long,
            value_name = "HOST:PORT",
            requires = "target",
            conflicts_with = "peers_only"
        )]
        addr: Vec<String>,
        /// Offer the files to paired machines, which fetch them with `receive peer:NAME`; no code. Ends after one reception.
        #[arg(long)]
        peers_only: bool,
        /// With --peers-only or --allow: keep serving the same files to every receiver until Ctrl-C.
        #[arg(long, requires = "peer_offer")]
        share: bool,
        /// Offer the files to these paired machines only (repeatable or comma-separated), or `all`; implies --peers-only.
        #[arg(long, value_name = "NAME", value_delimiter = ',')]
        allow: Vec<String>,
        /// With --peers-only or --allow: slot to serve (default: send); receivers use peer:NAME/SLOT.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "", requires = "peer_offer")]
        slot: Option<String>,
        /// With --peers-only or --allow: fix the UDP port of the server endpoint (firewall rules, --direct-only).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..), requires = "peer_offer")]
        port: Option<u16>,
        /// Number of random words from the EFF word list in the code (default: 6).
        #[arg(long, value_parser = clap::value_parser!(u8).range(6..=32))]
        code_words: Option<u8>,
        #[command(flatten)]
        pairing: crate::pairing::Options,
        /// Deadline in seconds for the receiver to connect and finish once paired.
        #[arg(long, default_value = "3600", value_parser = seconds)]
        transfer_timeout: f64,
        /// Hide periodic progress and transfer phases on stderr.
        #[arg(long)]
        no_progress: bool,
    },
    /// Receive files with the code printed by `send`, or as an inbox for paired machines; verify BLAKE3 integrity.
    Receive {
        /// code:WORDS printed by `send` (`code:` or nothing asks for the words on the terminal), or peer:NAME[/SLOT] of a paired machine running `send PATH --peers-only` (default slot: send).
        #[arg(conflicts_with_all = ["inbox", "once", "allow", "slot", "port"])]
        target: Option<String>,
        /// With peer:NAME: direct address HOST:PORT of the sender, as printed by it; repeat for several. Required with --direct-only.
        #[arg(
            long,
            value_name = "HOST:PORT",
            requires = "target",
            conflicts_with = "inbox"
        )]
        addr: Vec<String>,
        /// Receive what paired machines push with `send PATH peer:NAME`, one transfer at a time, until Ctrl-C; never replaces existing files.
        #[arg(long)]
        inbox: bool,
        /// With --inbox: exit after one successful reception.
        #[arg(long, requires = "inbox")]
        once: bool,
        /// With --inbox: accept only these paired machines (repeatable or comma-separated), or `all`.
        #[arg(long, value_name = "NAME", value_delimiter = ',', requires = "inbox")]
        allow: Vec<String>,
        /// With --inbox: slot to serve (default: inbox); senders use peer:NAME/SLOT.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "", requires = "inbox")]
        slot: Option<String>,
        /// With --inbox: fix the UDP port of the server endpoint (firewall rules, --direct-only).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..), requires = "inbox")]
        port: Option<u16>,
        #[command(flatten)]
        pairing: crate::pairing::Options,
        /// Destination directory; received files and folders are published inside it.
        #[arg(long, default_value = ".")]
        output: PathBuf,
        /// Payload limit: bytes or integer B, kB, MB, GB, TB, KiB, MiB, GiB, TiB (default: 10GiB, 1GiB per offer with --inbox).
        #[arg(long, value_parser = byte_size)]
        max_bytes: Option<u64>,
        /// Deadline in seconds for each transfer; an interrupted one resumes with the same command.
        #[arg(long, default_value = "3600", value_parser = seconds)]
        transfer_timeout: f64,
        /// Hide periodic progress and transfer phases on stderr.
        #[arg(long)]
        no_progress: bool,
        /// Accept top-level names starting with a dot, such as .bashrc or .ssh/.
        #[arg(long)]
        hidden: bool,
        #[command(flatten)]
        replacement: ReplacementOptions,
    },
    /// Open a private two-person chat with /send and /receive: `chat serve` opens it, `chat connect` joins.
    #[command(arg_required_else_help = true, args_conflicts_with_subcommands = true)]
    Chat {
        #[command(subcommand)]
        command: Option<ChatCommand>,
        /// A target of the forms without a role, refused with the subcommand to use.
        #[arg(hide = true)]
        legacy: Option<String>,
        /// `chat --peers-only` without a role, refused with the subcommand to use.
        #[arg(long, hide = true)]
        peers_only: bool,
    },
    /// Connect stdin/stdout to a peer.
    Pipe {
        #[command(subcommand)]
        command: PipeCommand,
    },
    /// Forward TCP services, or serve a folder over HTTP, through iroh.
    Tunnel {
        #[command(subcommand)]
        command: TunnelCommand,
    },
    /// Verify registration with a public relay or the selected custom relay.
    Relay {
        #[command(subcommand)]
        command: RelayCommand,
    },
    /// Measure round trips between peers.
    Ping {
        #[command(subcommand)]
        command: PingCommand,
    },
    /// Measure application throughput.
    Bench {
        #[command(subcommand)]
        command: BenchCommand,
    },
    /// Inspect endpoint or blob tickets offline.
    Ticket {
        #[command(subcommand)]
        command: TicketCommand,
    },
    /// Pair with another machine once, with a one-use code: no code creates one, code:WORDS joins.
    Pair(PairOptions),
    /// Show this machine's name, short key, protection and paired machines; rename, rotate or protect it.
    #[command(args_conflicts_with_subcommands = true)]
    Id {
        #[command(subcommand)]
        command: Option<IdCommand>,
        /// Print the result as JSON: name, machine, protected, peers.
        #[arg(long)]
        json: bool,
    },
    /// List and manage the machines paired with this one.
    #[command(args_conflicts_with_subcommands = true)]
    Peers {
        #[command(subcommand)]
        command: Option<PeersCommand>,
        /// Print the list as JSON: {"peers": [{name, machine, announced, paired}]}.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum IdCommand {
    /// Set the name announced when pairing (default: the hostname).
    Rename {
        /// 1 to 32 lowercase letters, digits or hyphens.
        name: String,
    },
    /// Encrypt the machine key with a passphrase typed twice (on a protected key: change the passphrase).
    Protect,
    /// Write the machine key back unprotected, after asking its passphrase.
    Unprotect,
    /// Replace the identity: paired machines must be paired again; a protected key stays protected.
    Rotate {
        /// Change only the address salt and keep the machine key (for a removed or lost machine).
        #[arg(long)]
        address: bool,
    },
}

#[derive(Subcommand)]
pub enum PeersCommand {
    /// Change the local name of a paired machine.
    Rename {
        /// Current local name.
        old: String,
        /// New local name: 1 to 32 lowercase letters, digits or hyphens.
        new: String,
    },
    /// Remove a paired machine; its next peer connection here is refused (open sessions and chat codes are unaffected).
    Rm {
        /// Local name of the machine to remove.
        name: String,
    },
}

#[derive(Args, Default)]
pub struct ReplacementOptions {
    /// Replace existing regular files without asking; preserve unrelated files in existing directories.
    #[arg(long, conflicts_with = "no_replace")]
    pub force: bool,
    /// Refuse an existing destination instead of asking before replacing or merging.
    #[arg(long)]
    pub no_replace: bool,
}

impl ReplacementOptions {
    pub fn mode(&self) -> crate::files::Replacement {
        if self.force {
            crate::files::Replacement::Force
        } else if self.no_replace {
            crate::files::Replacement::Refuse
        } else {
            crate::files::Replacement::Ask
        }
    }
}

#[derive(Args)]
pub struct PairOptions {
    /// code:WORDS to join the other machine's code (`code:` asks for the words on the terminal); omit to create a code.
    pub target: Option<String>,
    /// Name announced to the other machine for this pairing only (default: the name from `id`).
    #[arg(long = "as", value_name = "NAME")]
    pub announce: Option<String>,
    /// Local name for the other machine (default: the name it announces).
    #[arg(long, value_name = "ALIAS")]
    pub name: Option<String>,
    /// Number of random words from the EFF word list when creating a code (default: 6).
    #[arg(long, conflicts_with = "target", value_parser = clap::value_parser!(u8).range(6..=32))]
    pub code_words: Option<u8>,
    /// Total pairing deadline in seconds, including both confirmations (default: 300).
    #[arg(long, value_parser = seconds)]
    pub pairing_timeout: Option<f64>,
}

#[derive(Subcommand)]
pub enum ChatCommand {
    /// Open a chat and print a one-use code: the first machine that uses it joins.
    Serve(ChatServeOptions),
    /// Join a chat with the code printed by `chat serve`.
    Connect(ChatConnectOptions),
}

#[derive(Args)]
pub struct ChatServeOptions {
    /// Number of random words from the EFF word list in the code (default: 6).
    #[arg(long, value_parser = clap::value_parser!(u8).range(6..=32))]
    pub code_words: Option<u8>,
    /// Open the chat to paired machines only: no code, no relay needed.
    #[arg(long)]
    pub peers_only: bool,
    /// Admit only these paired machines (repeatable or comma-separated), or `all` for every paired machine; implies --peers-only: no code.
    #[arg(long, value_name = "NAME", value_delimiter = ',')]
    pub allow: Vec<String>,
    /// Slot that paired machines join with peer:NAME/SLOT (default: chat).
    #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    pub slot: Option<String>,
    /// Fix the UDP port of the slot endpoint (firewall rules, --direct-only with --peers-only or --allow).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,
    #[command(flatten)]
    pub session: ChatOptions,
}

#[derive(Args)]
pub struct ChatConnectOptions {
    /// code:WORDS printed by `chat serve` (`code:` asks for the words on the terminal), or peer:NAME[/SLOT] of a paired machine running `chat serve` (default slot: chat).
    pub target: String,
    /// With peer:NAME: direct address HOST:PORT of the chat, as printed by it; repeat for several. Required with --direct-only.
    #[arg(long, value_name = "HOST:PORT")]
    pub addr: Vec<String>,
    #[command(flatten)]
    pub session: ChatOptions,
}

/// Options of a chat session, on both sides.
#[derive(Args)]
pub struct ChatOptions {
    /// Total deadline in seconds for pairing with the code (default: 300, 120 with --no-verify).
    #[arg(long, value_parser = seconds)]
    pub pairing_timeout: Option<f64>,
    /// Skip the 6-digit confirmation number when pairing with the code: whoever uses the code first is served. Needed on both machines.
    #[arg(long)]
    pub no_verify: bool,
    /// Maximum received payload per offer; accepts sizes such as 10GiB.
    #[arg(long, default_value = "10GiB", value_parser = byte_size)]
    pub max_bytes: u64,
    /// Preparation/transfer deadline in seconds, per file offer.
    #[arg(long, default_value = "3600", value_parser = seconds)]
    pub transfer_timeout: f64,
    /// Hide file preparation and transfer progress on stderr.
    #[arg(long)]
    pub no_progress: bool,
    /// When messages show their time; default from IROH_TOOLBOX_TIMESTAMPS, else auto.
    #[arg(long, value_enum, value_name = "WHEN")]
    pub timestamps: Option<Timestamps>,
}

/// When chat messages show the time: `auto` in an interactive terminal only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Timestamps {
    Auto,
    Always,
    Never,
}

#[derive(Subcommand)]
pub enum RelayCommand {
    /// Register with the relay and report the result, including authentication errors.
    Check {
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum PingCommand {
    /// Answer pings: print a ticket, or serve a slot of paired machines.
    Serve {
        /// Concurrent clients served; further connections are refused.
        #[arg(long, default_value = "64", value_parser = positive)]
        max_connections: u64,
        /// Serve a slot that paired machines reach with peer:NAME/SLOT, with no ticket; default name: the command.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
        slot: Option<String>,
        /// Serve only paired machines (any in the peer book; measurements only). Implies a slot.
        #[arg(long)]
        peers_only: bool,
        /// With --peers-only, narrow to these paired machines (repeatable or comma-separated), or `all`; implies --peers-only.
        #[arg(long, value_name = "NAME", value_delimiter = ',')]
        allow: Vec<String>,
        /// Fix the UDP port of the server endpoint (firewall rules, --direct-only).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Ping a server and report round trips and the connection path.
    Connect {
        /// Ticket printed by the server, or peer:NAME[/SLOT] (default slot: ping).
        target: String,
        /// Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only.
        #[arg(long, value_name = "HOST:PORT")]
        addr: Vec<String>,
        /// Number of pings.
        #[arg(long, default_value = "5", value_parser = positive)]
        count: u64,
        /// Seconds between pings.
        #[arg(long, default_value = "1", value_parser = seconds)]
        interval: f64,
        /// Print the results as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Direction {
    Upload,
    Download,
    Both,
}

#[derive(Subcommand)]
pub enum BenchCommand {
    /// Answer throughput measurements: print a ticket, or serve a slot of paired machines.
    Serve {
        /// Per-direction limit: bytes or integer B, kB, MB, GB, TB, KiB, MiB, GiB, TiB.
        #[arg(long, default_value = "1GiB", value_parser = byte_size)]
        max_bytes: u64,
        /// Deadline in seconds for each measurement.
        #[arg(long, default_value = "60", value_parser = seconds)]
        transfer_timeout: f64,
        /// Hide periodic progress on stderr.
        #[arg(long)]
        no_progress: bool,
        /// Serve a slot that paired machines reach with peer:NAME/SLOT, with no ticket; default name: the command.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
        slot: Option<String>,
        /// Serve only paired machines (any in the peer book; measurements only). Implies a slot.
        #[arg(long)]
        peers_only: bool,
        /// With --peers-only, narrow to these paired machines (repeatable or comma-separated), or `all`; implies --peers-only.
        #[arg(long, value_name = "NAME", value_delimiter = ',')]
        allow: Vec<String>,
        /// Fix the UDP port of the server endpoint (firewall rules, --direct-only).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Measure upload and download throughput with a server.
    Connect {
        /// Ticket printed by the server, or peer:NAME[/SLOT] (default slot: bench).
        target: String,
        /// Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only.
        #[arg(long, value_name = "HOST:PORT")]
        addr: Vec<String>,
        /// Per-direction size: bytes or integer B, kB, MB, GB, TB, KiB, MiB, GiB, TiB.
        #[arg(long = "bytes", default_value = "64MiB", value_parser = byte_size)]
        size: u64,
        /// Direction to measure.
        #[arg(long, value_enum, default_value = "both")]
        direction: Direction,
        /// Deadline in seconds for each direction.
        #[arg(long, default_value = "60", value_parser = seconds)]
        transfer_timeout: f64,
        /// Print the results as JSON.
        #[arg(long)]
        json: bool,
        /// Hide periodic progress on stderr.
        #[arg(long)]
        no_progress: bool,
    },
}

#[derive(Subcommand)]
pub enum PipeCommand {
    /// Wait for a paired machine and connect it to stdin/stdout.
    Serve {
        /// Only receive: close the outgoing direction at once instead of reading stdin.
        #[arg(long)]
        recv_only: bool,
        /// Slot to serve; paired machines reach it with peer:NAME/SLOT.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_value = "pipe", default_missing_value = "pipe")]
        slot: String,
        /// Paired machines to serve (repeatable or comma-separated), or `all` for every paired machine. Required.
        #[arg(long, value_name = "NAME", value_delimiter = ',')]
        allow: Vec<String>,
        /// Fix the UDP port of the server endpoint (firewall rules, --direct-only).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Connect stdin/stdout to a paired machine running `pipe serve`.
    Connect {
        /// peer:NAME[/SLOT] of a paired machine (default slot: pipe).
        target: String,
        /// Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only.
        #[arg(long, value_name = "HOST:PORT")]
        addr: Vec<String>,
        /// Only receive: close the outgoing direction at once instead of reading stdin.
        #[arg(long)]
        recv_only: bool,
    },
}

#[derive(Subcommand)]
pub enum TunnelCommand {
    /// Forward TCP services, such as a running web server or SSH.
    Tcp {
        #[command(subcommand)]
        command: TcpCommand,
    },
    /// Serve a folder, read-only, to the browsers of paired machines.
    Http {
        #[command(subcommand)]
        command: HttpCommand,
    },
}

#[derive(Subcommand)]
pub enum HttpCommand {
    /// Serve a folder, read-only, to paired machines.
    Serve {
        /// Folder to serve, read-only; to forward a running web server, use `tunnel tcp serve`.
        path: PathBuf,
        /// Serve a static site: folders open their index.html, pages run their scripts, 404.html answers missing pages.
        #[arg(long)]
        site: bool,
        /// Also serve files and folders whose name starts with a dot.
        #[arg(long)]
        hidden: bool,
        /// Concurrent peer connections served; further connections are refused.
        #[arg(long, default_value = "64", value_parser = positive)]
        max_connections: u64,
        /// Slot to serve; paired machines reach it with peer:NAME/SLOT.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_value = "http", default_missing_value = "http")]
        slot: String,
        /// Paired machines to serve (repeatable or comma-separated), or `all` for every paired machine. Required.
        #[arg(long, value_name = "NAME", value_delimiter = ',')]
        allow: Vec<String>,
        /// Fix the UDP port of the server endpoint (firewall rules, --direct-only).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Open a paired machine's folder in your browser through a local address.
    Connect {
        /// peer:NAME[/SLOT] of a paired machine (default slot: http).
        target: String,
        /// Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only.
        #[arg(long, value_name = "HOST:PORT")]
        addr: Vec<String>,
        /// Local HOST:PORT that the browser opens.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
    },
}

#[derive(Subcommand)]
pub enum TcpCommand {
    /// Forward connections of paired machines to local TCP services.
    Serve {
        /// Destination HOST:PORT or SERVICE=HOST:PORT; repeat to share several services.
        #[arg(long, required = true)]
        to: Vec<String>,
        /// Concurrent peer connections served; further connections are refused.
        #[arg(long, default_value = "64", value_parser = positive)]
        max_connections: u64,
        /// Slot to serve; paired machines reach it with peer:NAME/SLOT.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_value = "tunnel", default_missing_value = "tunnel")]
        slot: String,
        /// Paired machines to serve (repeatable or comma-separated), or `all` for every paired machine. Required.
        #[arg(long, value_name = "NAME", value_delimiter = ',')]
        allow: Vec<String>,
        /// Fix the UDP port of the server endpoint (firewall rules, --direct-only).
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Reach a paired machine's TCP services through local ports.
    Connect {
        /// peer:NAME[/SLOT[/SERVICE]] of a paired machine (default slot: tunnel); without SERVICE, every service of the slot.
        target: String,
        /// Direct address HOST:PORT of the server, as printed by it; repeat for several. Required with --direct-only.
        #[arg(long, value_name = "HOST:PORT")]
        addr: Vec<String>,
        /// Local HOST:PORT for the slot's sole or selected service, or SERVICE=HOST:PORT to move one service of the group (repeatable). Without it, each service listens on 127.0.0.1 at the port its server announces.
        #[arg(long, value_name = "[SERVICE=]HOST:PORT")]
        bind: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum TicketCommand {
    /// Decode a ticket printed by `ping` or `bench` without connecting.
    Inspect {
        /// The ticket to decode.
        value: String,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn positive(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| "must be a positive integer".into())
}

fn byte_size(value: &str) -> Result<u64, String> {
    let end = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let multiplier = match &value[end..] {
        "" | "B" => 1,
        "kB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        _ => return Err(
            "use bytes or an integer with B, kB, MB, GB, TB, KiB, MiB, GiB or TiB (case-sensitive)"
                .into(),
        ),
    };
    value[..end]
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|n| *n > 0)
        .ok_or_else(|| "size must be positive and fit in 64-bit bytes".into())
}

pub fn seconds(value: &str) -> Result<f64, String> {
    value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= 31536000.0)
        .ok_or_else(|| "must be finite, positive seconds (at most one year)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_exact_and_reject_ambiguous_or_overflowing_input() {
        for (input, expected) in [
            ("64MiB", 67_108_864),
            ("1GiB", 1_073_741_824),
            ("1MB", 1_000_000),
            ("1kB", 1_000),
            ("1KiB", 1024),
            ("42B", 42),
            ("18446744073709551615", u64::MAX),
            ("1TB", 1_000_000_000_000),
            ("1TiB", 1 << 40),
        ] {
            assert_eq!(byte_size(input).unwrap(), expected, "{input}");
        }
        for input in [
            "",
            "0",
            "0MiB",
            "-1",
            "+1",
            "1.5MiB",
            "1 MiB",
            "1mib",
            "1KB",
            "1e6",
            "18446744073709551616",
            "18446744073709551615KiB",
        ] {
            assert!(byte_size(input).is_err(), "{input}");
        }
    }

    #[test]
    fn targets_codes_and_slots_parse_as_documented() {
        for args in [
            vec!["tool", "receive", "code:a-b", "--code", "a-b"],
            vec!["tool", "send", "file", "--code"],
            vec!["tool", "send", "file", "--code-words", "2"],
            vec!["tool", "chat", "serve", "--code-words", "5"],
            vec!["tool", "chat", "connect", "code:a-b", "--code-words", "6"],
            vec!["tool", "chat", "serve", "--code"],
            vec!["tool", "chat", "serve", "code:a-b"],
            vec!["tool", "chat", "connect"],
            vec!["tool", "chat", "connect", "peer:nas", "--peers-only"],
            vec!["tool", "chat", "connect", "peer:nas", "--allow", "a"],
            vec!["tool", "chat", "connect", "peer:nas", "--slot", "x"],
            vec!["tool", "chat", "connect", "peer:nas", "--port", "7777"],
            vec!["tool", "chat", "serve", "--addr", "1.2.3.4:5"],
            // `chat` alone shows the help of its subcommands.
            vec!["tool", "chat"],
            vec!["tool", "chat", "code:a-b", "serve"],
            vec!["tool", "receive", "code:a-b", "--force", "--no-replace"],
            vec!["tool", "receive", "code:a-b", "--confirm"],
            vec!["tool", "pipe", "serve", "--code"],
            vec!["tool", "pipe", "connect", "code:a-b", "--no-verify"],
            vec!["tool", "pipe", "serve", "--port", "0"],
            // Inbox options belong to the inbox; a target is a client.
            vec!["tool", "receive", "code:a-b", "--inbox"],
            vec!["tool", "receive", "--once"],
            vec!["tool", "receive", "--allow", "nas"],
            vec!["tool", "receive", "--slot", "depot"],
            vec!["tool", "receive", "--port", "7777"],
            vec!["tool", "send", "file", "--addr", "1.2.3.4:5"],
            vec!["tool", "send", "file", "peer:nas", "--peers-only"],
            vec!["tool", "send", "file", "--share"],
            vec!["tool", "send", "file", "--slot", "photos"],
            vec!["tool", "send", "file", "--port", "7777"],
            vec!["tool", "receive", "--addr", "1.2.3.4:5"],
            // A target never takes server options, whatever else is given.
            vec!["tool", "send", "file", "peer:nas", "--share"],
            vec!["tool", "send", "file", "peer:nas", "--allow", "a"],
            vec!["tool", "send", "file", "peer:nas", "--slot", "x"],
            vec!["tool", "send", "file", "peer:nas", "--port", "7777"],
            vec![
                "tool",
                "send",
                "file",
                "--peers-only",
                "--addr",
                "1.2.3.4:5",
            ],
            vec!["tool", "receive", "peer:nas", "--once"],
            vec!["tool", "receive", "peer:nas", "--allow", "a"],
            vec!["tool", "receive", "peer:nas", "--slot", "x"],
            vec!["tool", "receive", "peer:nas", "--port", "7777"],
            vec!["tool", "receive", "code:a-b", "--once"],
            vec!["tool", "receive", "--inbox", "--addr", "1.2.3.4:5"],
            // Per-slot grants are gone: servers narrow with --allow.
            vec!["tool", "peers", "access", "laptop", "ssh,web"],
            vec!["tool", "pair", "--access", "all"],
        ] {
            assert!(Cli::try_parse_from(args.clone()).is_err(), "{args:?}");
        }
        for args in [
            vec!["tool", "send", "file", "--code-words", "8"],
            vec![
                "tool",
                "send",
                "file",
                "--no-verify",
                "--pairing-timeout",
                "3",
            ],
            vec!["tool", "receive", "code:a-b"],
            // No target and a bare `code:` read the words at a prompt.
            vec!["tool", "receive"],
            vec!["tool", "receive", "code:"],
            vec!["tool", "chat", "connect", "code:"],
            vec!["tool", "pair", "code:"],
            // Other targets parse, then are refused with the one-use code message.
            vec!["tool", "receive", "blobticket"],
            vec![
                "tool",
                "receive",
                "code:a-b",
                "--pairing-timeout",
                "3",
                "--no-verify",
            ],
            vec!["tool", "chat", "serve"],
            vec!["tool", "chat", "serve", "--code-words", "8", "--no-verify"],
            vec!["tool", "chat", "connect", "endpoint-ticket"],
            vec!["tool", "chat", "connect", "code:a-b", "--no-verify"],
            vec![
                "tool",
                "chat",
                "connect",
                "peer:nas/equipe",
                "--addr",
                "1.2.3.4:5",
            ],
            vec![
                "tool",
                "chat",
                "serve",
                "--peers-only",
                "--allow",
                "laptop",
                "--slot",
                "equipe",
                "--port",
                "7777",
            ],
            vec!["tool", "chat", "serve", "--allow", "laptop"],
            // Forms without a role parse, then are refused with the subcommand to use.
            vec!["tool", "chat", "code:a-b"],
            vec!["tool", "chat", "peer:nas"],
            vec!["tool", "chat", "--peers-only"],
            vec!["tool", "send", "file", "peer:laptop"],
            vec![
                "tool",
                "send",
                "file",
                "peer:laptop/depot",
                "--addr",
                "1.2.3.4:5",
            ],
            vec!["tool", "receive", "--inbox"],
            vec!["tool", "receive", "peer:nas/photos", "--addr", "1.2.3.4:5"],
            vec!["tool", "send", "file", "--peers-only"],
            // --allow implies --peers-only, with its options.
            vec!["tool", "send", "file", "--allow", "laptop"],
            vec![
                "tool", "send", "file", "--allow", "laptop", "--share", "--slot", "photos",
                "--port", "7777",
            ],
            vec![
                "tool",
                "send",
                "file",
                "--peers-only",
                "--share",
                "--allow",
                "a,b",
                "--slot",
                "photos",
                "--port",
                "7777",
            ],
            vec![
                "tool",
                "receive",
                "--inbox",
                "--allow",
                "nas,phone",
                "--once",
                "--slot",
                "--port",
                "7777",
            ],
            vec!["tool", "id"],
            vec!["tool", "id", "rotate", "--address"],
            vec!["tool", "id", "--json"],
            vec!["tool", "id", "rename", "nas"],
            vec!["tool", "peers"],
            vec!["tool", "peers", "--json"],
            vec!["tool", "peers", "rename", "laptop", "work"],
            vec!["tool", "peers", "rm", "laptop"],
            vec!["tool", "ping", "serve", "--slot", "web"],
            vec!["tool", "ping", "serve", "--peers-only", "--allow", "laptop"],
            vec!["tool", "ping", "serve", "--allow", "laptop"],
            vec!["tool", "ping", "serve", "--allow", "all"],
            vec!["tool", "send", "file", "--allow", "all"],
            vec!["tool", "receive", "--inbox", "--allow", "all"],
            vec!["tool", "chat", "serve", "--allow", "all"],
            vec!["tool", "pipe", "serve", "--allow", "all"],
            vec![
                "tool",
                "tunnel",
                "tcp",
                "serve",
                "--to",
                "127.0.0.1:22",
                "--allow",
                "all",
            ],
            vec!["tool", "tunnel", "http", "serve", ".", "--allow", "all"],
            vec!["tool", "bench", "serve", "--peers-only"],
            vec!["tool", "tunnel", "tcp", "serve", "--to", "127.0.0.1:22"],
            vec![
                "tool", "tunnel", "http", "serve", "./site", "--site", "--hidden",
            ],
            vec![
                "tool", "tunnel", "http", "connect", "peer:nas", "--bind", "[::1]:0",
            ],
            vec!["tool", "ping", "connect", "peer:nas", "--addr", "1.2.3.4:5"],
        ] {
            assert!(Cli::try_parse_from(args.clone()).is_ok(), "{args:?}");
        }
        // Tunnel and pipe servers always run on a slot: the command's by default.
        let pipe = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Pipe {
                command:
                    PipeCommand::Serve {
                        slot,
                        allow,
                        port,
                        recv_only,
                    },
            } => (slot, allow, port, recv_only),
            _ => unreachable!(),
        };
        assert_eq!(
            pipe(&["tool", "pipe", "serve"]),
            ("pipe".into(), vec![], None, false)
        );
        assert_eq!(
            pipe(&[
                "tool",
                "pipe",
                "serve",
                "--slot",
                "ssh",
                "--allow",
                "a,b",
                "--allow",
                "c",
                "--port",
                "7777",
                "--recv-only",
            ]),
            (
                "ssh".into(),
                vec!["a".into(), "b".into(), "c".into()],
                Some(7777),
                true
            )
        );
        // A served folder has its own default slot, beside a TCP tunnel.
        let http = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Tunnel {
                command:
                    TunnelCommand::Http {
                        command:
                            HttpCommand::Serve {
                                slot, site, hidden, ..
                            },
                    },
            } => (slot, site, hidden),
            _ => unreachable!(),
        };
        assert_eq!(
            http(&["tool", "tunnel", "http", "serve", "."]),
            ("http".into(), false, false)
        );
        // The optional slot of ping and bench keeps ticket mode without it.
        let ping = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Ping {
                command: PingCommand::Serve { slot, .. },
            } => slot,
            _ => unreachable!(),
        };
        assert_eq!(ping(&["tool", "ping", "serve"]), None);
        assert_eq!(
            ping(&["tool", "ping", "serve", "--slot"]),
            Some(String::new())
        );
        assert_eq!(
            ping(&["tool", "ping", "serve", "--slot", "web"]),
            Some("web".into())
        );
    }
}
