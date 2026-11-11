#[macro_use]
mod progress;
mod bench;
mod chat;
mod cli;
mod config;
mod files;
#[cfg(test)]
mod fuzz;
mod identity;
mod network;
mod pair;
mod pairing;
mod partial;
mod peer_files;
mod peers;
mod ping;
mod publication;
mod slot;
mod streams;
mod takeover;
mod target;
mod web;

use anyhow::{Result, ensure};
use clap::Parser;
use cli::{
    BenchCommand, ChatCommand, Cli, Command, HttpCommand, IdCommand, PeersCommand, PingCommand,
    PipeCommand, RelayCommand, TcpCommand, TicketCommand, TunnelCommand,
};

/// Payload limit of a code or `receive peer:NAME` reception.
const DEFAULT_MAX_BYTES: u64 = 10 << 30;
/// Payload limit per offer of an inbox, which paired machines fill unattended.
const INBOX_MAX_BYTES: u64 = 1 << 30;

#[tokio::main(worker_threads = 2)]
async fn main() {
    let cli = Cli::parse();
    progress::init(cli.options.verbose);
    // Offline ticket inspection must not read config or open sockets.
    if let Command::Ticket {
        command: TicketCommand::Inspect { value, json },
    } = &cli.command
    {
        if let Err(error) = network::inspect_ticket(value, *json) {
            progress::print_error(&error, |text| text);
            std::process::exit(1);
        }
        return;
    }
    // The machine identity is local: no configuration or network needed.
    let local = match &cli.command {
        Command::Id { command, json } => Some(machine_id(command.as_ref(), *json)),
        Command::Peers { command, json } => Some(manage_peers(command.as_ref(), *json)),
        _ => None,
    };
    if let Some(result) = local {
        if let Err(error) = result {
            progress::print_error(&error, |text| text);
            std::process::exit(1);
        }
        return;
    }
    let settings = match config::Settings::load(&cli.options) {
        Ok(settings) => settings,
        Err(error) => {
            progress::print_error(&error, |text| text);
            std::process::exit(1);
        }
    };
    // Drop the cancelled operation (pairing endpoints and temporary directories)
    // before exiting, including while a human confirmation is pending.
    let interrupted = async {
        loop {
            let _ = tokio::signal::ctrl_c().await;
            // An open chat says goodbye to its peer, then exits with status 130.
            if !chat::handles_interrupt() {
                break;
            }
        }
    };
    let result = tokio::select! {
        result = run(cli.command, &settings) => Some(result),
        _ = interrupted => None,
    };
    let Some(result) = result else {
        target::restore_terminal();
        progress::keep_status();
        std::process::exit(130);
    };
    if let Err(error) = result {
        // Ctrl+C handled by an open chat.
        if error.is::<chat::Interrupted>() {
            progress::keep_status();
            std::process::exit(130);
        }
        progress::print_error(&error, |text| settings.redact(text));
        std::process::exit(1);
    }
    // Tokio's stdin uses a blocking reader that cannot be cancelled on EOF
    // from the remote peer. Endpoint shutdown has completed; do not wait for
    // an idle console reader when leaving a stdio command.
    progress::keep_status();
    std::process::exit(0);
}

fn peer_names(book: &peers::PeerBook) -> String {
    let names: Vec<_> = book.iter().map(|peer| peer.name.as_str()).collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

fn machine_id(command: Option<&IdCommand>, json: bool) -> Result<()> {
    match command {
        None => {
            let (identity, created) = identity::Identity::load_or_create()?;
            if created {
                note!(
                    "created this machine's identity in {}",
                    identity::home()?.display()
                );
            }
            let (name, book) = (identity::machine_name()?, peers::PeerBook::load()?);
            let machine = identity::short_key(&identity.address().machine);
            if json {
                let peers: Vec<_> = book.iter().map(|peer| &peer.name).collect();
                out!(
                    "{}",
                    serde_json::json!({
                        "name": name,
                        "machine": machine,
                        "protected": identity.is_protected(),
                        "peers": peers,
                    })
                );
            } else {
                out!("Name:     {name} (announced when pairing)");
                out!("Machine:  {machine}");
                out!(
                    "Key:      {}",
                    if identity.is_protected() {
                        "protected by a passphrase"
                    } else {
                        "not protected (iroh-toolbox id protect)"
                    }
                );
                out!("Peers:    {}", peer_names(&book));
            }
        }
        Some(IdCommand::Rename { name }) => {
            identity::set_machine_name(name)?;
            note!("this machine is now named {name}; it is announced when pairing");
        }
        Some(IdCommand::Protect) => {
            if identity::Identity::protect()? {
                note!("the passphrase of the machine key changed");
            } else {
                note!(
                    "the machine key is now protected by a passphrase: every command that uses it asks for it on a terminal"
                );
            }
        }
        Some(IdCommand::Unprotect) => {
            if identity::Identity::unprotect()? {
                note!("the machine key is no longer protected by a passphrase");
            } else {
                note!("the machine key was not protected");
            }
        }
        Some(IdCommand::Rotate { address }) => {
            // Read the book first: a damaged one must fail before anything changes.
            // The machine name plays no part, so a bad `name` file never blocks a rotation.
            let book = peers::PeerBook::load()?;
            let (previous, _) = identity::Identity::rotate(*address)?;
            let paired = peer_names(&book);
            if *address {
                note!("This machine's address changed.");
                if book.is_empty() {
                    note!("No paired machine to pair again with.");
                } else {
                    note!("Pair again with: {paired}");
                }
            } else {
                let old = identity::short_key(&previous.machine);
                note!("This machine's identity changed: the machine key {old} is void.");
                if book.is_empty() {
                    note!("No paired machine to update.");
                } else {
                    note!(
                        "Paired machines: {paired}. On each of them, run `iroh-toolbox peers`, \
                         remove the entry whose MACHINE is {old} with `iroh-toolbox peers rm NAME`, \
                         then pair again."
                    );
                }
            }
            note!("Restart running servers: they keep using the previous identity.");
        }
    }
    Ok(())
}

fn manage_peers(command: Option<&PeersCommand>, json: bool) -> Result<()> {
    match command {
        None => list_peers(&peers::PeerBook::load()?, json),
        Some(PeersCommand::Rename { old, new }) => {
            peers::PeerBook::update(|book| book.rename(old, new))?;
            note!("{old} is now named {new}");
        }
        Some(PeersCommand::Rm { name }) => {
            peers::PeerBook::update(|book| book.remove(name))?;
            note!(
                "removed {name}; its next peer connection here is refused (tunnels, pipes, files, chat and --peers-only ping/bench)"
            );
            note!(
                "Open sessions continue, and a chat code it already holds stays valid: restart that chat to create a new code"
            );
        }
    }
    Ok(())
}

fn list_peers(book: &peers::PeerBook, json: bool) {
    if json {
        let peers: Vec<_> = book
            .iter()
            .map(|peer| {
                serde_json::json!({
                    "name": peer.name,
                    "machine": identity::short_key(&peer.machine()),
                    "announced": peer.announced,
                    "paired": peer.paired,
                })
            })
            .collect();
        out!("{}", serde_json::json!({ "peers": peers }));
        return;
    }
    if book.is_empty() {
        note!("No paired machines. Pair one with `iroh-toolbox pair`.");
        return;
    }
    let rows: Vec<[String; 3]> = book
        .iter()
        .map(|peer| {
            [
                peer.name.clone(),
                identity::short_key(&peer.machine()),
                peer.paired_date().to_owned(),
            ]
        })
        .collect();
    let header = ["NAME", "MACHINE", "PAIRED"].map(str::to_owned);
    let widths: Vec<usize> = (0..2)
        .map(|column| {
            rows.iter()
                .chain([&header])
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in [&header].into_iter().chain(&rows) {
        out!(
            "{:<w0$}  {:<w1$}  {}",
            row[0],
            row[1],
            row[2],
            w0 = widths[0],
            w1 = widths[1]
        );
    }
}

/// The optional `--slot` of ping and bench: absent for a ticket, empty for the command's slot.
/// `--peers-only` and `--allow` imply a slot, named after the command unless given.
fn optional_slot<'a>(
    slot: &'a Option<String>,
    port: Option<u16>,
    peers_only: bool,
    allow: &'a [String],
) -> slot::Serve<'a> {
    let peers_only = peers_only || !allow.is_empty();
    slot::Serve {
        slot: slot.as_deref().or(peers_only.then_some("")),
        allow,
        port,
        peers_only,
    }
}

async fn run(command: Command, settings: &config::Settings) -> Result<()> {
    match command {
        Command::Relay {
            command: RelayCommand::Check { json },
        } => network::relay_check(settings, json).await,
        Command::Ping {
            command:
                PingCommand::Serve {
                    max_connections,
                    slot,
                    peers_only,
                    allow,
                    port,
                },
        } => {
            let serve = optional_slot(&slot, port, peers_only, &allow);
            ping::serve(settings, max_connections, serve).await
        }
        Command::Ping {
            command:
                PingCommand::Connect {
                    target,
                    addr,
                    count,
                    interval,
                    json,
                },
        } => ping::connect(settings, &target, &addr, count, interval, json).await,
        Command::Bench {
            command:
                BenchCommand::Serve {
                    max_bytes,
                    transfer_timeout,
                    no_progress,
                    slot,
                    peers_only,
                    allow,
                    port,
                },
        } => {
            bench::serve(
                settings,
                max_bytes,
                transfer_timeout,
                no_progress,
                optional_slot(&slot, port, peers_only, &allow),
            )
            .await
        }
        Command::Bench {
            command:
                BenchCommand::Connect {
                    target,
                    addr,
                    size,
                    direction,
                    transfer_timeout,
                    json,
                    no_progress,
                },
        } => {
            bench::connect(
                settings,
                bench::ConnectOptions {
                    target: &target,
                    addrs: &addr,
                    size,
                    direction,
                    transfer_timeout,
                    json,
                    no_progress,
                },
            )
            .await
        }
        // --allow names the only receivers: it implies --peers-only.
        Command::Send {
            path,
            peers_only,
            share,
            allow,
            slot,
            port,
            transfer_timeout,
            no_progress,
            code_words,
            pairing,
            ..
        } if peers_only || !allow.is_empty() => {
            ensure!(
                code_words.is_none() && !pairing.no_verify && pairing.pairing_timeout.is_none(),
                "--code-words, --no-verify and --pairing-timeout apply to code transfers; --peers-only and --allow rely on the pairing of both machines"
            );
            let serve = slot::Serve {
                slot: Some(slot.as_deref().unwrap_or("")),
                allow: &allow,
                port,
                peers_only: false,
            };
            peer_files::offer(
                settings,
                peer_files::OfferOptions {
                    serve,
                    share,
                    path: &path,
                    transfer_timeout,
                    no_progress,
                },
            )
            .await
        }
        Command::Send {
            path,
            target: Some(target),
            addr,
            transfer_timeout,
            no_progress,
            code_words,
            pairing,
            ..
        } => {
            ensure!(
                code_words.is_none() && !pairing.no_verify && pairing.pairing_timeout.is_none(),
                "--code-words, --no-verify and --pairing-timeout apply to code transfers; `send PATH peer:NAME` relies on the pairing of both machines"
            );
            peer_files::send_to(
                settings,
                &path,
                &target,
                &addr,
                transfer_timeout,
                no_progress,
            )
            .await
        }
        Command::Send {
            path,
            target: None,
            transfer_timeout,
            no_progress,
            code_words,
            pairing,
            ..
        } => {
            files::send(
                settings,
                &path,
                transfer_timeout,
                no_progress,
                code_words.map_or(pairing::MIN_WORDS, usize::from),
                &pairing,
            )
            .await
        }
        Command::Receive {
            inbox: true,
            once,
            allow,
            slot,
            port,
            output,
            max_bytes,
            transfer_timeout,
            no_progress,
            hidden,
            pairing,
            replacement,
            ..
        } => {
            ensure!(
                !replacement.force,
                "an inbox never replaces or merges existing files; remove --force"
            );
            ensure!(
                !pairing.no_verify && pairing.pairing_timeout.is_none(),
                "--no-verify and --pairing-timeout apply to code receptions; an inbox admits paired machines"
            );
            let serve = slot::Serve {
                slot: Some(slot.as_deref().unwrap_or("")),
                allow: &allow,
                port,
                peers_only: false,
            };
            peer_files::inbox(
                settings,
                peer_files::InboxOptions {
                    serve,
                    once,
                    output: &output,
                    max_bytes: max_bytes.unwrap_or(INBOX_MAX_BYTES),
                    transfer_timeout,
                    no_progress,
                    hidden,
                },
            )
            .await
        }
        Command::Receive {
            target: Some(target),
            addr,
            output,
            max_bytes,
            transfer_timeout,
            no_progress,
            hidden,
            pairing,
            replacement,
            ..
        } if target.starts_with("peer:") => {
            ensure!(
                !pairing.no_verify && pairing.pairing_timeout.is_none(),
                "--no-verify and --pairing-timeout apply to code receptions; receive peer:NAME relies on the pairing of both machines"
            );
            peer_files::receive_from(
                settings,
                &target,
                &addr,
                &output,
                files::ReceiveOptions {
                    max_bytes: max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
                    transfer_timeout,
                    no_progress,
                    replacement: replacement.mode(),
                    confirmation: None,
                    hidden,
                },
            )
            .await
        }
        Command::Receive {
            target,
            addr,
            output,
            max_bytes,
            transfer_timeout,
            no_progress,
            hidden,
            pairing,
            replacement,
            ..
        } => {
            ensure!(
                addr.is_empty(),
                "--addr applies to peer: targets only; a code finds the other machine by itself"
            );
            let code = match &target {
                Some(target) => target::parse_code(target, "receive")?,
                None => String::new(),
            };
            let code = target::complete_code(code, "receive").await?;
            files::receive(
                settings,
                files::ReceiveTarget::Code {
                    code: &code,
                    pairing: &pairing,
                },
                &output,
                files::ReceiveOptions {
                    max_bytes: max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
                    transfer_timeout,
                    no_progress,
                    replacement: replacement.mode(),
                    confirmation: None,
                    hidden,
                },
            )
            .await
        }
        Command::Pipe {
            command:
                PipeCommand::Serve {
                    recv_only,
                    slot,
                    allow,
                    port,
                },
        } => {
            let serve = slot::Serve {
                slot: Some(&slot),
                allow: &allow,
                port,
                peers_only: false,
            };
            streams::pipe(settings, streams::PipeRole::Serve(serve), recv_only).await
        }
        Command::Chat {
            command,
            legacy,
            peers_only,
        } => match command {
            Some(ChatCommand::Serve(options)) => chat::serve(settings, options).await,
            Some(ChatCommand::Connect(options)) => chat::connect(settings, options).await,
            None => Err(chat::without_role(legacy.as_deref(), peers_only)),
        },
        Command::Pair(options) => pair::run(settings, options).await,
        Command::Pipe {
            command:
                PipeCommand::Connect {
                    target,
                    addr,
                    recv_only,
                },
        } => {
            streams::pipe(
                settings,
                streams::PipeRole::Connect(&target, &addr),
                recv_only,
            )
            .await
        }
        Command::Tunnel {
            command:
                TunnelCommand::Tcp {
                    command:
                        TcpCommand::Serve {
                            to,
                            max_connections,
                            slot,
                            allow,
                            port,
                        },
                },
        } => {
            let serve = slot::Serve {
                slot: Some(&slot),
                allow: &allow,
                port,
                peers_only: false,
            };
            streams::tunnel_listen(settings, &to, max_connections, serve).await
        }
        Command::Tunnel {
            command:
                TunnelCommand::Tcp {
                    command: TcpCommand::Connect { target, addr, bind },
                },
        } => streams::tunnel_connect(settings, &target, &bind, &addr).await,
        Command::Tunnel {
            command:
                TunnelCommand::Http {
                    command:
                        HttpCommand::Serve {
                            path,
                            site,
                            hidden,
                            max_connections,
                            slot,
                            allow,
                            port,
                        },
                },
        } => {
            // A wrong folder is reported before the peer book is read.
            let folder = web::Site::open(&path, site, hidden)?;
            let serve = slot::Serve {
                slot: Some(&slot),
                allow: &allow,
                port,
                peers_only: false,
            };
            streams::http_listen(settings, folder, max_connections, serve).await
        }
        Command::Tunnel {
            command:
                TunnelCommand::Http {
                    command: HttpCommand::Connect { target, addr, bind },
                },
        } => streams::http_connect(settings, &target, &bind, &addr).await,
        Command::Ticket { .. } | Command::Id { .. } | Command::Peers { .. } => {
            unreachable!("offline command handled before configuration")
        }
    }
}
