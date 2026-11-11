//! Rendezvous, terminal interaction and the commands built on pairing.
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, ConnectionError},
};
use iroh_blobs::{BlobFormat, ticket::BlobTicket};
use tokio::time::{Instant, timeout, timeout_at};
use zeroize::Zeroizing;

use super::{
    ALPN, CONFIRM_ATTEMPTS, Options, Pending,
    code::{MAX_WORDS, MIN_WORDS, generate_code, parse_code, rendezvous_key},
    frame::{Channel, Frame},
    handshake::{
        Confirm, Numbers, Purpose, Terms, host_session, join_session, run_host, run_joiner,
    },
    record::{Choices, Held, Local, Outcome, STORED, host_pair, join_pair, pair_terms},
};
use crate::{config::Settings, network, peers::PeerBook, progress::Progress};

fn terminal_confirm() -> Confirm {
    Box::new(|numbers| Box::pin(terminal_numbers(numbers)))
}

/// Nothing is accepted without the other machine's number: a person cannot
/// confirm without exchanging numbers with the other person.
async fn terminal_numbers(numbers: Numbers) -> Result<bool> {
    note!("Your confirmation number: {}", numbers.mine);
    note!("Read it aloud to the other person, and type the number they read to you.");
    for attempt in 1..=CONFIRM_ATTEMPTS {
        let answer = read_answer("Their confirmation number (empty to cancel): ".into()).await?;
        let Some(answer) = answer.filter(|answer| !answer.is_empty()) else {
            return Ok(false);
        };
        if same_number(&answer, &numbers.theirs) {
            return Ok(true);
        }
        match CONFIRM_ATTEMPTS - attempt {
            0 => note!("Wrong confirmation number {CONFIRM_ATTEMPTS} times."),
            1 => note!("This is not the number the other machine shows; 1 attempt left."),
            left => note!("This is not the number the other machine shows; {left} attempts left."),
        }
    }
    Ok(false)
}

/// Compares a typed number, spaces ignored, in constant time.
fn same_number(typed: &str, expected: &str) -> bool {
    let digits = |value: &str| -> Vec<u8> {
        value
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect()
    };
    let (typed, expected) = (digits(typed), digits(expected));
    typed.len() == expected.len()
        && typed
            .iter()
            .zip(&expected)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

// --- Rendezvous -------------------------------------------------------------

/// Publishes the rendezvous endpoint of a new code and serves exactly one
/// pairing attempt. Returns the joiner's endpoint identity.
/// A new code with its registered rendezvous endpoint, ready to serve one pairing.
pub struct Rendezvous {
    ep: Endpoint,
    code: String,
    password: Zeroizing<String>,
    deadline: Instant,
}

/// Creates a code and registers its rendezvous endpoint. Callers run it
/// concurrently with their main endpoint: both registrations take a relay round trip.
pub async fn open(settings: &Settings, options: &Options, words: usize) -> Result<Rendezvous> {
    let deadline = options.deadline();
    let code = generate_code(words.clamp(MIN_WORDS, MAX_WORDS))?;
    let parsed = parse_code(&code)?;
    let ep = timeout_at(
        deadline,
        network::bind_rendezvous(
            settings,
            rendezvous_key(&parsed.rendezvous),
            vec![ALPN.to_vec()],
        ),
    )
    .await
    .context("pairing timed out while registering the rendezvous endpoint")??;
    Ok(Rendezvous {
        ep,
        code,
        password: parsed.password,
        deadline,
    })
}

/// The first frame of a connection to a rendezvous endpoint.
async fn read_hello(conn: Connection) -> Result<(Connection, Channel, Frame)> {
    let (send, recv) = conn.accept_bi().await?;
    let mut channel = Channel::new(&conn, send, recv);
    let hello = channel.read().await?;
    network::mark_hello();
    Ok((conn, channel, hello))
}

/// The first connection that started a key exchange on the rendezvous endpoint.
struct Arrival {
    conn: Connection,
    local: EndpointId,
    channel: Channel,
    hello: Frame,
    password: Zeroizing<String>,
}

/// Shows the code and serves exactly one pairing attempt on its rendezvous
/// endpoint with `run`. The rendezvous connection's remote ID is never kept:
/// it is the joiner's temporary pairing endpoint, not the one it transfers from.
async fn serve_one<T>(
    settings: &Settings,
    rendezvous: Rendezvous,
    hint: impl FnOnce(&str),
    progress: &mut Progress,
    success_code: u32,
    run: impl FnOnce(Arrival) -> Pending<Result<T>>,
) -> Result<T> {
    let Rendezvous {
        ep,
        code,
        password,
        deadline,
    } = rendezvous;
    network::share(settings, &code, false)?;
    hint(&code);
    progress.step(
        "waiting for the other machine (keep the code private)",
        None,
    );
    let result = timeout_at(deadline, async {
        // Connections that never send a Hello do not consume the code; the
        // first started key exchange does, whatever its outcome. Attempts run
        // concurrently under the short handshake deadline, so a silent
        // connection cannot delay the real joiner.
        let (conn, channel, hello) =
            network::accept_first(&ep, settings.timeout, "pairing", read_hello)
                .await
                .context("pairing endpoint closed")?;
        let outcome = run(Arrival {
            conn: conn.clone(),
            local: ep.id(),
            channel,
            hello,
            password,
        })
        .await;
        conn.close(
            if outcome.is_ok() { success_code } else { 1u32 }.into(),
            b"pairing finished",
        );
        outcome
    })
    .await
    .context("pairing timed out; create a new code")
    .and_then(|result| result);
    ep.close().await;
    result
}

/// Serves one pairing attempt and delivers `payload`. Returns the transfer
/// endpoint the joiner announced inside the authenticated channel.
async fn host(
    settings: &Settings,
    options: &Options,
    rendezvous: Rendezvous,
    purpose: Purpose,
    payload: String,
    hint: impl FnOnce(&str),
    progress: &mut Progress,
) -> Result<EndpointId> {
    let verify = options.verify();
    serve_one(settings, rendezvous, hint, progress, 0, move |arrival| {
        Box::pin(async move {
            let Arrival {
                conn,
                local,
                channel,
                hello,
                password,
            } = arrival;
            let terms = Terms {
                purpose,
                password: &password,
                verify,
            };
            run_host(
                &conn,
                local,
                channel,
                hello,
                terms,
                payload,
                terminal_confirm(),
            )
            .await
        })
    })
    .await
}

/// How long a joining machine looks for the code's creator before saying so.
const WAITING_NOTE: Duration = Duration::from_secs(5);

/// The time left before `deadline`, in the unit a person reads.
fn remaining(deadline: Instant) -> String {
    let seconds = deadline.saturating_duration_since(Instant::now()).as_secs();
    if seconds >= 60 {
        format!("{} min", (seconds + 30) / 60)
    } else {
        format!("{seconds}s")
    }
}

/// The temporary endpoint a joiner pairs from: relay-only, so that whoever
/// answers at the rendezvous ID, the creator or a squatter, learns no direct
/// address of the joiner (see notes/rendezvous-exposure.md). Bind it with
/// [`prepare_join`], concurrently with the endpoint that will transfer.
pub struct Joiner {
    ep: Endpoint,
}

impl Joiner {
    async fn close(self) {
        self.ep.close().await;
    }
}

/// Binds the joiner's temporary pairing endpoint.
pub async fn prepare_join(settings: &Settings) -> Result<Joiner> {
    Ok(Joiner {
        ep: network::bind_joiner(settings).await?,
    })
}

/// Finds the rendezvous endpoint of `code` from `ep` and runs the joiner side
/// with `run`, which receives the connection, `ep`'s identity, the password and
/// the pairing deadline. The deadline bounds the exchange only: the caller gets
/// the connection back to watch how the host ends it.
async fn join_with<T>(
    settings: &Settings,
    options: &Options,
    code: &str,
    ep: &Endpoint,
    progress: &mut Progress,
    run: impl FnOnce(Connection, EndpointId, Zeroizing<String>, Instant) -> Pending<Result<T>>,
) -> Result<(T, Connection)> {
    let deadline = options.deadline();
    let parsed = parse_code(code)?;
    let mut addr = EndpointAddr::new(rendezvous_key(&parsed.rendezvous).public());
    if let Some(relay) = &settings.relay {
        addr = addr.with_relay_url(relay.clone());
    }
    progress.phase("pairing: looking for the other machine", None);
    let conn = timeout_at(deadline, async {
        // The host may still be registering: retry until the deadline.
        let mut search = std::pin::pin!(async {
            loop {
                match timeout(settings.timeout, ep.connect(addr.clone(), ALPN)).await {
                    Ok(Ok(conn)) => break conn,
                    _ => {
                        detail!("pairing: rendezvous not reachable yet; retrying");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
        tokio::select! {
            conn = search.as_mut() => conn,
            () = tokio::time::sleep(WAITING_NOTE) => {
                note!(
                    "waiting for the machine that created this code (up to {}; Ctrl-C to stop)",
                    remaining(deadline)
                );
                search.await
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("pairing timed out: no machine answered with this code; check the code and that the other command is still running"))?;
    progress.phase("pairing: checking the code", None);
    let outcome = timeout_at(
        deadline,
        run(conn.clone(), ep.id(), parsed.password, deadline),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("pairing timed out before both machines finished; nothing was stored")
    })??;
    Ok((outcome, conn))
}

/// Finds the rendezvous endpoint of `code` from the joiner's temporary
/// endpoint, announces `transfer` and returns the host's payload.
async fn join(
    settings: &Settings,
    options: &Options,
    purpose: Purpose,
    code: &str,
    joiner: Joiner,
    transfer: EndpointId,
    progress: &mut Progress,
) -> Result<String> {
    let verify = options.verify();
    let joined = join_with(
        settings,
        options,
        code,
        &joiner.ep,
        progress,
        move |conn, local, password, _| {
            Box::pin(async move {
                let terms = Terms {
                    purpose,
                    password: &password,
                    verify,
                };
                run_joiner(&conn, local, transfer, terms, terminal_confirm()).await
            })
        },
    )
    .await;
    let result = match joined {
        Ok((payload, conn)) => {
            // Let the host read the final confirmation before the connection closes.
            let _ = timeout(Duration::from_secs(2), conn.closed()).await;
            Ok(payload)
        }
        Err(error) => Err(error),
    };
    joiner.close().await;
    result
}

// --- Machine pairing --------------------------------------------------------

pub struct Setup {
    pub local: Local,
    pub choices: Choices,
    pub(super) confirm: Confirm,
}

impl Setup {
    /// Real book and prompts on the terminal.
    pub fn terminal(local: Local, name: Option<String>) -> Self {
        Self {
            local,
            choices: Choices {
                name,
                lock: Box::new(|| {
                    Box::pin(async {
                        let guard = tokio::task::spawn_blocking(PeerBook::lock).await??;
                        Ok(Box::new(guard) as Box<dyn Held>)
                    })
                }),
                ask_name: Box::new(|message| Box::pin(terminal_ask_name(message))),
            },
            confirm: terminal_confirm(),
        }
    }
}

/// One line from the terminal; `None` on end of input.
pub async fn read_answer(prompt: String) -> Result<Option<String>> {
    tokio::task::spawn_blocking(move || -> std::io::Result<Option<String>> {
        use std::io::Write;
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        let read = std::io::stdin().read_line(&mut answer)?;
        Ok((read > 0).then(|| answer.trim().to_owned()))
    })
    .await?
    .map_err(Into::into)
}

async fn terminal_ask_name(message: String) -> Result<Option<String>> {
    note!("{message}");
    Ok(
        read_answer("Another name for the other machine (empty to cancel): ".into())
            .await?
            .filter(|answer| !answer.is_empty()),
    )
}

/// Serves a code opened with [`open`] and pairs with whoever completes it.
pub async fn pair_host(
    settings: &Settings,
    rendezvous: Rendezvous,
    setup: Setup,
    progress: &mut Progress,
) -> Result<Outcome> {
    let hint = |code: &str| {
        note!("Code: {code}");
        code_hint(
            settings,
            &Options::default(),
            "On the other machine (keep the code private):",
            "pair code:",
        );
    };
    let deadline = rendezvous.deadline;
    let peer = serve_one(
        settings,
        rendezvous,
        hint,
        progress,
        STORED,
        move |arrival| {
            Box::pin(async move {
                let Arrival {
                    conn,
                    local,
                    channel,
                    hello,
                    password,
                } = arrival;
                let Setup {
                    local: me,
                    choices,
                    confirm,
                } = setup;
                let mut session =
                    host_session(&conn, local, channel, hello, pair_terms(&password), confirm)
                        .await?;
                host_pair(&mut session, me, choices, deadline).await
            })
        },
    )
    .await?;
    // The host's own commit succeeded, or `pair_host` would have failed.
    Ok(Outcome {
        peer,
        confirmed: true,
    })
}

/// Whether the host ended the connection with [`STORED`], within a short grace.
async fn host_stored(conn: &Connection) -> bool {
    let _ = timeout(Duration::from_secs(3), conn.closed()).await;
    matches!(
        conn.close_reason(),
        Some(ConnectionError::ApplicationClosed(close)) if u64::from(close.error_code) == u64::from(STORED)
    )
}

/// Joins `code` from the joiner's temporary endpoint and pairs with the machine that created it. The
/// pairing is stored before the host's close is awaited, so a late or missing
/// confirmation is reported in [`Outcome`], never as a failure.
pub async fn pair_join(
    settings: &Settings,
    options: &Options,
    code: &str,
    joiner: Joiner,
    setup: Setup,
    progress: &mut Progress,
) -> Result<Outcome> {
    let joined = join_with(
        settings,
        options,
        code,
        &joiner.ep,
        progress,
        move |conn, local, password, deadline| {
            Box::pin(async move {
                let Setup {
                    local: me,
                    choices,
                    confirm,
                } = setup;
                let mut session =
                    join_session(&conn, local, pair_terms(&password), confirm).await?;
                join_pair(&mut session, me, choices, deadline).await
            })
        },
    )
    .await;
    let outcome = match joined {
        Ok((peer, conn)) => Ok(Outcome {
            peer,
            confirmed: host_stored(&conn).await,
        }),
        Err(error) => Err(error),
    };
    joiner.close().await;
    outcome
}

// --- Commands ---------------------------------------------------------------

/// Options the other machine needs, appended to the copyable command.
fn remote_options(options: &Options) -> &'static str {
    if options.no_verify {
        " --no-verify"
    } else {
        ""
    }
}

/// Printed before the code when the confirmation number is off.
fn unverified_note(options: &Options) {
    if !options.verify() {
        note!("note: confirmation number disabled; whoever uses the code first is served");
    }
}

pub async fn send_ticket(
    settings: &Settings,
    options: &Options,
    rendezvous: Rendezvous,
    ticket: &BlobTicket,
    progress: &mut Progress,
) -> Result<EndpointId> {
    let hint = |code: &str| {
        // The words to type at the other machine's prompt; stdout keeps the bare code.
        unverified_note(options);
        note!("Code: {code}");
        if settings.no_hints {
            return;
        }
        let (globals, relay_shown) = network::remote_globals(settings);
        note!("On the receiving machine:");
        note!(
            "  iroh-toolbox{globals} receive code:{} --output ./received",
            remote_options(options)
        );
        note!("  then type the code at the prompt (it stays out of the process list)");
        network::relay_hint(settings, relay_shown);
    };
    host(
        settings,
        options,
        rendezvous,
        Purpose::Files,
        ticket.to_string(),
        hint,
        progress,
    )
    .await
}

pub async fn receive_ticket(
    settings: &Settings,
    options: &Options,
    code: &str,
    joiner: Joiner,
    transfer: &Endpoint,
    progress: &mut Progress,
) -> Result<BlobTicket> {
    let value = join(
        settings,
        options,
        Purpose::Files,
        code,
        joiner,
        transfer.id(),
        progress,
    )
    .await?;
    validate_ticket(&value, settings)
}

/// How an endpoint ticket is exchanged with a code.
pub enum Side<'a> {
    /// Creator: serve a code opened with [`open`].
    Create(Rendezvous),
    /// Joiner: the creator's code and the temporary pairing endpoint.
    Join(&'a str, Joiner),
}

/// Exchanges a chat endpoint ticket. `ep` is the chat endpoint: the joiner
/// announces its identity, the creator returns it. The joiner also returns
/// the creator's address.
pub async fn pair_chat(
    settings: &Settings,
    options: &Options,
    ep: &Endpoint,
    side: Side<'_>,
    progress: &mut Progress,
) -> Result<(EndpointId, Option<EndpointAddr>)> {
    let hint = |code: &str| {
        // A code admits whoever uses it first, paired or not.
        unverified_note(options);
        note!("Code: {code} (single-use: first admitted participant takes the session)");
        code_hint(
            settings,
            options,
            "On your partner's machine (keep the code private):",
            "chat connect code:",
        )
    };
    pair_endpoint(settings, options, ep, side, Purpose::Chat, hint, progress).await
}

fn code_hint(settings: &Settings, options: &Options, intro: &str, command: &str) {
    if settings.no_hints {
        return;
    }
    let (globals, relay_shown) = network::remote_globals(settings);
    note!("{intro}");
    note!(
        "  iroh-toolbox{globals} {command}{}",
        remote_options(options)
    );
    note!("  then type the code at the prompt (it stays out of the process list)");
    network::relay_hint(settings, relay_shown);
}

async fn pair_endpoint(
    settings: &Settings,
    options: &Options,
    ep: &Endpoint,
    side: Side<'_>,
    purpose: Purpose,
    hint: impl FnOnce(&str),
    progress: &mut Progress,
) -> Result<(EndpointId, Option<EndpointAddr>)> {
    let (code, joiner) = match side {
        Side::Join(code, joiner) => (code, joiner),
        Side::Create(rendezvous) => {
            let ticket = network::ticket(ep);
            let peer = host(
                settings, options, rendezvous, purpose, ticket, hint, progress,
            )
            .await?;
            return Ok((peer, None));
        }
    };
    let value = join(settings, options, purpose, code, joiner, ep.id(), progress).await?;
    let ticket: iroh_tickets::endpoint::EndpointTicket = value
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid paired endpoint ticket"))?;
    let addr = ticket.endpoint_addr().clone();
    network::check_addr(&addr, settings)?;
    Ok((addr.id, Some(addr)))
}

fn validate_ticket(value: &str, settings: &Settings) -> Result<BlobTicket> {
    let ticket: BlobTicket = value
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid paired blob ticket"))?;
    ensure!(
        ticket.format() == BlobFormat::HashSeq,
        "expected a collection blob ticket"
    );
    network::check_addr(ticket.addr(), settings)?;
    Ok(ticket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::{frame::Body, handshake::Role};
    use crate::{identity::Address, peers::Peer};
    use iroh::{RelayMode, SecretKey, endpoint::presets};

    #[test]
    fn the_confirmation_number_is_the_default_and_sets_the_deadline() {
        let remaining = |options: &Options| options.deadline().duration_since(Instant::now());
        let asked = Options::default();
        assert!(asked.verify());
        assert_eq!(remote_options(&asked), "");
        assert!(remaining(&asked) > Duration::from_secs(290));
        let skipped = Options {
            no_verify: true,
            ..Options::default()
        };
        assert!(!skipped.verify());
        assert_eq!(remote_options(&skipped), " --no-verify");
        assert!(remaining(&skipped) <= Duration::from_secs(120));
        let explicit = Options {
            pairing_timeout: Some(7.0),
            ..Options::default()
        };
        assert!(remaining(&explicit) <= Duration::from_secs(7));
    }

    async fn local_endpoint(key: Option<SecretKey>) -> Endpoint {
        let mut builder = Endpoint::builder(presets::Minimal)
            .clear_address_lookup()
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![ALPN.to_vec()]);
        if let Some(key) = key {
            builder = builder.secret_key(key);
        }
        builder.bind().await.unwrap()
    }

    /// Silent connections neither delay nor consume the code: the first
    /// connection that sends a hello is served, however many came before it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn silent_rendezvous_connections_do_not_delay_the_first_hello() {
        let host_ep = local_endpoint(Some(rendezvous_key("abacus-abdomen-abdominal"))).await;
        let addr = host_ep.addr();
        let accepting = host_ep.clone();
        let admitted = tokio::spawn(async move {
            network::accept_first(
                &accepting,
                network::HANDSHAKE_DEADLINE,
                "pairing",
                read_hello,
            )
            .await
        });
        let mute = local_endpoint(None).await;
        let mut held = Vec::new();
        for _ in 0..20 {
            held.push(mute.connect(addr.clone(), ALPN).await.unwrap());
        }
        let joiner_ep = local_endpoint(None).await;
        let conn = joiner_ep.connect(addr, ALPN).await.unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        let started = std::time::Instant::now();
        let mut channel = Channel::new(&conn, send, recv);
        channel
            .write(&Frame::Hello {
                version: 1,
                purpose: "files".into(),
                verify: false,
                pake: "x".into(),
            })
            .await
            .unwrap();
        let (_, _, hello) = timeout(Duration::from_secs(3), admitted)
            .await
            .expect("the hello must be served at once")
            .unwrap()
            .unwrap();
        assert!(matches!(hello, Frame::Hello { .. }));
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(held);
        joiner_ep.close().await;
        mute.close().await;
        host_ep.close().await;
    }

    fn auto(answer: bool, shown: std::sync::Arc<std::sync::Mutex<Vec<Numbers>>>) -> Confirm {
        Box::new(move |numbers| {
            shown.lock().unwrap().push(numbers);
            Box::pin(async move { Ok(answer) })
        })
    }

    /// Runs both roles over local sockets; returns (host result, joiner result).
    async fn pair_locally(
        host_password: &str,
        joiner_password: &str,
        verify: (bool, bool),
        answers: (bool, bool),
        purposes: (Purpose, Purpose),
        shown: std::sync::Arc<std::sync::Mutex<Vec<Numbers>>>,
    ) -> (Result<EndpointId>, Result<String>) {
        let host_ep = local_endpoint(Some(rendezvous_key("abacus-abdomen-abdominal"))).await;
        let joiner_ep = local_endpoint(None).await;
        let addr = host_ep.addr();
        let host_password = host_password.to_owned();
        let host_shown = shown.clone();
        let server = tokio::spawn(async move {
            let conn = host_ep.accept().await.unwrap().await.unwrap();
            let (send, recv) = conn.accept_bi().await.unwrap();
            let mut channel = Channel::new(&conn, send, recv);
            let result = async {
                let hello = channel.read().await?;
                run_host(
                    &conn,
                    host_ep.id(),
                    channel,
                    hello,
                    Terms {
                        purpose: purposes.0,
                        password: &host_password,
                        verify: verify.0,
                    },
                    "payload-ticket".into(),
                    auto(answers.0, host_shown),
                )
                .await
            }
            .await;
            conn.close(0u32.into(), b"done");
            host_ep.close().await;
            result
        });
        let conn = joiner_ep.connect(addr, ALPN).await.unwrap();
        let joined = timeout(
            Duration::from_secs(10),
            run_joiner(
                &conn,
                joiner_ep.id(),
                transfer_key().public(),
                Terms {
                    purpose: purposes.1,
                    password: joiner_password,
                    verify: verify.1,
                },
                auto(answers.1, shown),
            ),
        )
        .await
        .unwrap();
        let hosted = timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap();
        joiner_ep.close().await;
        (hosted, joined)
    }

    /// The key of the endpoint a test joiner transfers from: not the one it pairs from.
    fn transfer_key() -> SecretKey {
        SecretKey::from_bytes(&[0x7e; 32])
    }

    fn shown() -> std::sync::Arc<std::sync::Mutex<Vec<Numbers>>> {
        Default::default()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn matching_codes_deliver_the_payload() {
        let (host, joiner) = pair_locally(
            "a-b-c-d",
            "a-b-c-d",
            (false, false),
            (true, true),
            (Purpose::Files, Purpose::Files),
            shown(),
        )
        .await;
        // The host learns the announced transfer endpoint, once the `done` MACs hold.
        assert_eq!(host.unwrap(), transfer_key().public());
        assert_eq!(joiner.unwrap(), "payload-ticket");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_password_fails_on_both_sides_without_payload() {
        let (host, joiner) = pair_locally(
            "a-b-c-d",
            "a-b-c-x",
            (false, false),
            (true, true),
            (Purpose::Files, Purpose::Files),
            shown(),
        )
        .await;
        assert!(host.unwrap_err().to_string().contains("wrong code"));
        assert!(joiner.unwrap_err().to_string().contains("wrong code"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mismatched_command_or_verify_setting_is_refused() {
        for (verify, purposes) in [
            ((false, false), (Purpose::Files, Purpose::Chat)),
            ((true, false), (Purpose::Files, Purpose::Files)),
        ] {
            let (host, joiner) = pair_locally(
                "a-b-c-d",
                "a-b-c-d",
                verify,
                (true, true),
                purposes,
                shown(),
            )
            .await;
            assert!(host.is_err() && joiner.is_err());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_machine_asks_for_the_other_number_and_any_failure_aborts() {
        let numbers = shown();
        let (host, joiner) = pair_locally(
            "a-b-c-d",
            "a-b-c-d",
            (true, true),
            (true, true),
            (Purpose::Chat, Purpose::Chat),
            numbers.clone(),
        )
        .await;
        host.unwrap();
        assert_eq!(joiner.unwrap(), "payload-ticket");
        let numbers = numbers.lock().unwrap().clone();
        assert_eq!(numbers.len(), 2);
        assert_eq!(numbers[0].mine, numbers[1].theirs);
        assert_eq!(numbers[0].theirs, numbers[1].mine);
        for number in [&numbers[0].mine, &numbers[0].theirs] {
            assert!(
                number.len() == 7 && number.as_bytes()[3] == b' ',
                "{number}"
            );
        }
        for answers in [(false, true), (true, false)] {
            let (host, joiner) = pair_locally(
                "a-b-c-d",
                "a-b-c-d",
                (true, true),
                answers,
                (Purpose::Files, Purpose::Files),
                shown(),
            )
            .await;
            assert!(host.is_err() && joiner.is_err());
        }
    }

    #[test]
    fn typed_numbers_ignore_spaces_and_nothing_else() {
        assert!(same_number("481 207", "481 207"));
        assert!(same_number("481207", "481 207"));
        assert!(same_number(" 48 12 07 ", "481 207"));
        for wrong in ["481 208", "48120", "4812070", "", "yes", "481-207"] {
            assert!(!same_number(wrong, "481 207"), "{wrong}");
        }
    }

    // --- Machine pairing ---

    use std::sync::{Arc, Mutex};

    /// An in-memory book: nothing reaches the disk, and a test sees whether the
    /// pairing committed. Each test machine has its own, so one process can
    /// run both sides without sharing a configuration directory.
    struct Memory {
        book: PeerBook,
        committed: Arc<Mutex<Option<PeerBook>>>,
    }

    impl Held for Memory {
        fn book(&mut self) -> &mut PeerBook {
            &mut self.book
        }

        fn commit(self: Box<Self>) -> Pending<Result<()>> {
            *self.committed.lock().unwrap() = Some(self.book.clone());
            Box::pin(async { Ok(()) })
        }
    }

    /// What a test machine did: the book it committed and the questions it was asked.
    #[derive(Default, Clone)]
    struct Probe {
        committed: Arc<Mutex<Option<PeerBook>>>,
        names: Arc<Mutex<Vec<String>>>,
    }

    impl Probe {
        fn book(&self) -> Option<PeerBook> {
            self.committed.lock().unwrap().clone()
        }
    }

    struct Machine {
        seed: u8,
        announced: &'static str,
        book: PeerBook,
        name: Option<&'static str>,
        /// Answers to the name question, in order; empty means cancel.
        names: Vec<&'static str>,
    }

    impl Machine {
        fn new(seed: u8, announced: &'static str) -> Self {
            Self {
                seed,
                announced,
                book: PeerBook::default(),
                name: None,
                names: vec![],
            }
        }

        fn key(&self) -> SecretKey {
            SecretKey::from_bytes(&[self.seed; 32])
        }

        fn address(&self, salt: u8) -> Address {
            Address::from_parts(self.key().public(), [salt; 10])
        }

        fn setup(self, salt: u8) -> (Setup, Probe) {
            let probe = Probe::default();
            let (key, address) = (self.key(), self.address(salt));
            let local = Local::new(address, self.announced.into(), move |m| key.sign(m));
            let (book, committed) = (self.book, probe.committed.clone());
            let asked_names = probe.names.clone();
            let mut names = self.names.into_iter();
            let setup = Setup {
                local,
                choices: Choices {
                    name: self.name.map(Into::into),
                    lock: Box::new(move || {
                        Box::pin(async move {
                            Ok(Box::new(Memory { book, committed }) as Box<dyn Held>)
                        })
                    }),
                    ask_name: Box::new(move |message| {
                        asked_names.lock().unwrap().push(message);
                        let answer = names.next().map(str::to_owned);
                        Box::pin(async move { Ok(answer) })
                    }),
                },
                confirm: auto(true, shown()),
            };
            (setup, probe)
        }
    }

    fn in_a_minute() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// Runs `pair` between a host and a joiner over local sockets. A host that
    /// pairs successfully closes with [`STORED`] unless `host_confirms` is false.
    async fn pair_detailed(
        host: Setup,
        joiner: Setup,
        host_confirms: bool,
    ) -> (Result<Peer>, Result<Outcome>) {
        let host_ep = local_endpoint(Some(rendezvous_key("abacus-abdomen-abdominal"))).await;
        let joiner_ep = local_endpoint(None).await;
        let addr = host_ep.addr();
        let server = tokio::spawn(async move {
            let conn = host_ep.accept().await.unwrap().await.unwrap();
            let (send, recv) = conn.accept_bi().await.unwrap();
            let mut channel = Channel::new(&conn, send, recv);
            let result = async {
                let hello = channel.read().await?;
                let Setup {
                    local,
                    choices,
                    confirm,
                } = host;
                let terms = pair_terms("a-b-c-d");
                let mut session =
                    host_session(&conn, host_ep.id(), channel, hello, terms, confirm).await?;
                host_pair(&mut session, local, choices, in_a_minute()).await
            }
            .await;
            let code = if result.is_ok() && host_confirms {
                STORED
            } else {
                0
            };
            conn.close(code.into(), b"done");
            host_ep.close().await;
            result
        });
        let conn = joiner_ep.connect(addr, ALPN).await.unwrap();
        let joined = timeout(Duration::from_secs(10), async {
            let Setup {
                local,
                choices,
                confirm,
            } = joiner;
            let mut session =
                join_session(&conn, joiner_ep.id(), pair_terms("a-b-c-d"), confirm).await?;
            let peer = join_pair(&mut session, local, choices, in_a_minute()).await?;
            let confirmed = host_stored(&conn).await;
            Ok(Outcome { peer, confirmed })
        })
        .await
        .unwrap();
        let hosted = timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap();
        joiner_ep.close().await;
        (hosted, joined)
    }

    /// What a rogue joiner puts in its payload frame.
    enum Announce {
        /// This text as the announced endpoint, with a valid `done`.
        Text(String),
        /// A valid announcement, then a `done` that does not verify.
        BadDone,
        /// A ticket instead of an endpoint.
        Ticket,
    }

    /// A files host paired with a joiner that behaves as `announce` says; the
    /// host's result is returned. An announced ID is only ever returned after
    /// the joiner's `done` verified.
    async fn host_against_announcing_joiner(announce: Announce) -> Result<EndpointId> {
        let host_ep = local_endpoint(Some(rendezvous_key("abacus-abdomen-abdominal"))).await;
        let joiner_ep = local_endpoint(None).await;
        let addr = host_ep.addr();
        let terms = Terms {
            purpose: Purpose::Files,
            password: "a-b-c-d",
            verify: false,
        };
        let server = tokio::spawn(async move {
            let conn = host_ep.accept().await.unwrap().await.unwrap();
            let (send, recv) = conn.accept_bi().await.unwrap();
            let mut channel = Channel::new(&conn, send, recv);
            let hello = channel.read().await.unwrap();
            let result = run_host(
                &conn,
                host_ep.id(),
                channel,
                hello,
                terms,
                "payload-ticket".into(),
                auto(true, shown()),
            )
            .await;
            conn.close(1u32.into(), b"done");
            host_ep.close().await;
            result
        });
        let conn = joiner_ep.connect(addr, ALPN).await.unwrap();
        let mut session = join_session(&conn, joiner_ep.id(), terms, auto(true, shown()))
            .await
            .unwrap();
        assert!(matches!(
            session.channel.read().await.unwrap(),
            Frame::Payload(Body::Ticket { .. })
        ));
        let honest =
            crate::pairing::handshake::encode_endpoint(SecretKey::from_bytes(&[9; 32]).public());
        let body = match &announce {
            Announce::Text(text) => Body::Endpoint {
                endpoint: text.clone(),
            },
            Announce::BadDone => Body::Endpoint { endpoint: honest },
            Announce::Ticket => Body::Ticket {
                ticket: "blob".into(),
            },
        };
        session.channel.write(&Frame::Payload(body)).await.unwrap();
        let transcript = session.channel.digest();
        // The host may already have refused: its `done` is then never sent.
        if let Ok(Frame::Done { .. }) = session.channel.read().await {
            let mac = match announce {
                Announce::BadDone => hex::encode([0u8; 32]),
                _ => crate::pairing::handshake::tag(&session.keys.done, &[b"joiner", &transcript]),
            };
            let _ = session.channel.write(&Frame::Done { mac }).await;
        }
        let hosted = timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap();
        joiner_ep.close().await;
        hosted
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_invalid_or_unconfirmed_announced_endpoint_is_refused() {
        let key = SecretKey::from_bytes(&[9; 32]).public();
        let valid = crate::pairing::handshake::encode_endpoint(key);
        // Control: the same announcement with a verifying `done` is served.
        assert_eq!(
            host_against_announcing_joiner(Announce::Text(valid.clone()))
                .await
                .unwrap(),
            key
        );
        // Not a key: wrong length, not hex, upper case (one text per key),
        // empty, and a point that is not on the curve.
        let off_curve = (0..=255u8)
            .map(|first| {
                let mut bytes = [0u8; 32];
                bytes[0] = first;
                bytes[31] = 0x7f;
                hex::encode(bytes)
            })
            .find(|text| crate::pairing::handshake::decode_endpoint(text).is_err())
            .expect("some 32-byte strings are not public keys");
        for text in [
            valid[..62].to_owned(),
            format!("{valid}00"),
            "zz".repeat(32),
            valid.to_uppercase(),
            String::new(),
            off_curve,
        ] {
            let error = host_against_announcing_joiner(Announce::Text(text))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("invalid announced endpoint"),
                "{error}"
            );
        }
        // A `done` that does not cover the announcement: nothing is returned.
        assert!(
            host_against_announcing_joiner(Announce::BadDone)
                .await
                .is_err()
        );
        // The ticket form from a joiner is not an announcement.
        let error = host_against_announcing_joiner(Announce::Ticket)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("invalid pairing message"),
            "{error}"
        );
    }

    async fn pair_between(host: Setup, joiner: Setup) -> (Result<Peer>, Result<Peer>) {
        let (hosted, joined) = pair_detailed(host, joiner, true).await;
        (hosted, joined.map(|outcome| outcome.peer))
    }

    /// How a joiner that holds a valid record misbehaves after the host's `done`.
    #[derive(Clone, Copy)]
    enum Rogue {
        Drops,
        BadDone,
    }

    /// A host paired with a joiner that answers the records correctly, reads the
    /// host's `done`, then drops the connection or sends a wrong `done`.
    async fn host_against_rogue_joiner(host: Setup, rogue: Rogue) -> Result<Peer> {
        let host_ep = local_endpoint(Some(rendezvous_key("abacus-abdomen-abdominal"))).await;
        let joiner_ep = local_endpoint(None).await;
        let addr = host_ep.addr();
        let server = tokio::spawn(async move {
            let conn = host_ep.accept().await.unwrap().await.unwrap();
            let (send, recv) = conn.accept_bi().await.unwrap();
            let mut channel = Channel::new(&conn, send, recv);
            let hello = channel.read().await.unwrap();
            let Setup {
                local,
                choices,
                confirm,
            } = host;
            let mut session = host_session(
                &conn,
                host_ep.id(),
                channel,
                hello,
                pair_terms("a-b-c-d"),
                confirm,
            )
            .await
            .unwrap();
            let result = host_pair(&mut session, local, choices, in_a_minute()).await;
            conn.close(1u32.into(), b"done");
            host_ep.close().await;
            result
        });
        let conn = joiner_ep.connect(addr, ALPN).await.unwrap();
        let (local, _) = Machine::new(2, "laptop").setup(8);
        let local = local.local;
        let mut session = join_session(
            &conn,
            joiner_ep.id(),
            pair_terms("a-b-c-d"),
            auto(true, shown()),
        )
        .await
        .unwrap();
        let transcript = session.channel.digest();
        assert!(matches!(
            session.channel.read().await.unwrap(),
            Frame::Payload(Body::Peer { .. })
        ));
        let mine = local.record(Role::Joiner, &transcript);
        session
            .channel
            .write(&Frame::Payload(Body::Peer { peer: mine }))
            .await
            .unwrap();
        assert!(matches!(
            session.channel.read().await.unwrap(),
            Frame::Done { .. }
        ));
        match rogue {
            Rogue::Drops => conn.close(2u32.into(), b"gone"),
            Rogue::BadDone => session
                .channel
                .write(&Frame::Done {
                    mac: hex::encode([0u8; 32]),
                })
                .await
                .unwrap(),
        }
        let hosted = timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap();
        joiner_ep.close().await;
        hosted
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn machines_exchange_verified_records_and_store_each_other() {
        let (nas, laptop) = (Machine::new(1, "nas"), Machine::new(2, "laptop"));
        let (nas_address, laptop_address) = (nas.address(7), laptop.address(8));
        let (host, nas_probe) = nas.setup(7);
        let (joiner, laptop_probe) = laptop.setup(8);
        let (hosted, joined) = pair_between(host, joiner).await;
        let (hosted, joined) = (hosted.unwrap(), joined.unwrap());
        // Each side stores the other, under the name it announced.
        assert_eq!(
            (hosted.name.as_str(), hosted.address),
            ("laptop", laptop_address)
        );
        assert_eq!((joined.name.as_str(), joined.address), ("nas", nas_address));
        // No question is asked when the announced names are free.
        assert!(nas_probe.names.lock().unwrap().is_empty());
        assert!(laptop_probe.names.lock().unwrap().is_empty());
        let stored = nas_probe.book().unwrap();
        assert_eq!(stored.get("laptop").unwrap().address, laptop_address);
        assert_eq!(stored.get("laptop").unwrap().announced, "laptop");
        assert!(laptop_probe.book().unwrap().get("nas").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_local_name_option_names_the_other_machine() {
        let mut nas = Machine::new(1, "nas");
        nas.name = Some("work");
        let (host, nas_probe) = nas.setup(7);
        let (joiner, _) = Machine::new(2, "laptop").setup(8);
        let (hosted, joined) = pair_between(host, joiner).await;
        let (hosted, joined) = (hosted.unwrap(), joined.unwrap());
        assert_eq!(hosted.name, "work");
        assert_eq!(joined.name, "nas");
        assert!(nas_probe.names.lock().unwrap().is_empty());
        assert_eq!(nas_probe.book().unwrap().iter().count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_machine_again_keeps_its_name_and_refreshes_its_address() {
        let laptop = Machine::new(2, "laptop");
        let mut nas = Machine::new(1, "nas");
        let mut before = PeerBook::default();
        before
            .add_pairing(laptop.address(3), "laptop", Some("work"))
            .unwrap();
        nas.book = before;
        let (host, nas_probe) = nas.setup(7);
        let (joiner, _) = laptop.setup(8);
        let (hosted, joined) = pair_between(host, joiner).await;
        hosted.unwrap();
        joined.unwrap();
        let stored = nas_probe.book().unwrap();
        let peer = stored.get("work").unwrap();
        assert_eq!(stored.iter().count(), 1);
        assert_eq!(peer.address.salt(), &[8; 10]);
        assert!(nas_probe.names.lock().unwrap().is_empty());
    }

    /// A book where a machine other than `Machine::new(2, ..)` already uses `name`.
    fn book_using(name: &str) -> PeerBook {
        let mut book = PeerBook::default();
        book.add_pairing(Machine::new(9, "other").address(1), name, None)
            .unwrap();
        book
    }

    fn book_with_laptop_of_another_machine() -> PeerBook {
        book_using("laptop")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn name_conflict_on_either_side_aborts_with_reason_13_and_stores_nothing() {
        for host_conflicts in [true, false] {
            let (mut nas, mut laptop) = (Machine::new(1, "nas"), Machine::new(2, "laptop"));
            if host_conflicts {
                nas.book = book_using("laptop");
            } else {
                laptop.book = book_using("nas");
            }
            let (host, nas_probe) = nas.setup(7);
            let (joiner, laptop_probe) = laptop.setup(8);
            let (hosted, joined) = pair_between(host, joiner).await;
            let (conflicted, other, probe) = if host_conflicts {
                (hosted, joined, &nas_probe)
            } else {
                (joined, hosted, &laptop_probe)
            };
            let conflicted = conflicted.unwrap_err().to_string();
            assert!(conflicted.contains("name taken"), "{conflicted}");
            // The other machine reports close code 13.
            let other = other.unwrap_err().to_string();
            assert!(other.contains("no free name"), "{other}");
            assert!(nas_probe.book().is_none() && laptop_probe.book().is_none());
            let message = probe.names.lock().unwrap()[0].clone();
            assert!(message.contains("used by machine"), "{message}");
            assert!(message.contains("rotated its identity"), "{message}");
            assert!(message.contains("peers rm"), "{message}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn another_name_resolves_a_conflict() {
        let mut nas = Machine::new(1, "nas");
        nas.book = book_with_laptop_of_another_machine();
        nas.names = vec!["Bad Name!", "laptop-2"];
        let (host, nas_probe) = nas.setup(7);
        let (joiner, _) = Machine::new(2, "laptop").setup(8);
        let (hosted, joined) = pair_between(host, joiner).await;
        assert_eq!(hosted.unwrap().name, "laptop-2");
        joined.unwrap();
        // The invalid answer is refused and asked again.
        assert_eq!(nas_probe.names.lock().unwrap().len(), 2);
        assert_eq!(nas_probe.book().unwrap().iter().count(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forged_or_misattributed_records_are_refused_and_nothing_is_stored() {
        // The host signs with a key other than the one it announces.
        let impostor = SecretKey::from_bytes(&[5; 32]);
        let nas = Machine::new(1, "nas");
        let address = nas.address(7);
        let (mut host, nas_probe) = nas.setup(7);
        host.local = Local::new(address, "nas".into(), move |m| impostor.sign(m));
        let (joiner, laptop_probe) = Machine::new(2, "laptop").setup(8);
        let (hosted, joined) = pair_between(host, joiner).await;
        assert!(
            joined
                .unwrap_err()
                .to_string()
                .contains("signature is invalid")
        );
        assert!(hosted.is_err());
        assert!(nas_probe.book().is_none() && laptop_probe.book().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_rejects_a_forged_joiner_record_on_its_own_check() {
        let impostor = SecretKey::from_bytes(&[5; 32]);
        let laptop = Machine::new(2, "laptop");
        let address = laptop.address(8);
        let (mut joiner, laptop_probe) = laptop.setup(8);
        joiner.local = Local::new(address, "laptop".into(), move |m| impostor.sign(m));
        let (host, nas_probe) = Machine::new(1, "nas").setup(7);
        let (hosted, _) = pair_between(host, joiner).await;
        // The error is the host's own verdict, not the joiner's closed connection.
        let error = hosted.unwrap_err().to_string();
        assert!(error.contains("signature is invalid"), "{error}");
        assert!(nas_probe.book().is_none() && laptop_probe.book().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn host_stores_nothing_when_the_joiner_drops_or_sends_a_bad_done() {
        for rogue in [Rogue::Drops, Rogue::BadDone] {
            let (host, probe) = Machine::new(1, "nas").setup(7);
            let error = host_against_rogue_joiner(host, rogue).await.unwrap_err();
            let error = error.to_string();
            if matches!(rogue, Rogue::BadDone) {
                assert!(error.contains("did not confirm"), "{error}");
            }
            assert!(probe.book().is_none(), "{error}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn joiner_learns_whether_the_host_stored_the_pairing() {
        for confirms in [true, false] {
            let (host, _) = Machine::new(1, "nas").setup(7);
            let (joiner, laptop_probe) = Machine::new(2, "laptop").setup(8);
            let (hosted, joined) = pair_detailed(host, joiner, confirms).await;
            hosted.unwrap();
            let outcome = joined.unwrap();
            // The joiner keeps its own record either way.
            assert!(laptop_probe.book().unwrap().get("nas").is_some());
            assert_eq!(outcome.confirmed, confirms);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pair_requires_the_confirmation_number_and_its_own_purpose() {
        for (verify, purposes) in [
            ((false, false), (Purpose::Pair, Purpose::Pair)),
            ((true, false), (Purpose::Pair, Purpose::Pair)),
            ((true, true), (Purpose::Pair, Purpose::Files)),
            ((true, true), (Purpose::Files, Purpose::Pair)),
        ] {
            let (host, joiner) = pair_locally(
                "a-b-c-d",
                "a-b-c-d",
                verify,
                (true, true),
                purposes,
                shown(),
            )
            .await;
            assert!(host.is_err() && joiner.is_err(), "{verify:?} {purposes:?}");
        }
    }

    /// A leaked blob ticket is useless in code mode: after pairing, the sender
    /// serves only the transfer endpoint the joiner announced inside the
    /// authenticated channel, not the endpoint it paired from.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paired_sender_serves_only_the_paired_receiver() {
        use iroh_blobs::{format::collection::Collection, store::mem::MemStore};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("file");
        std::fs::write(&source, b"authorized bytes").unwrap();
        let prepared = crate::files::PreparedSend::new(&source, true)
            .await
            .unwrap();
        let blobs_ep = Endpoint::builder(presets::Minimal)
            .clear_address_lookup()
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![iroh_blobs::ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let terms = Terms {
            purpose: Purpose::Files,
            password: "a-b-c-d",
            verify: false,
        };

        // Pairing: the joiner pairs from `pairing_ep`, receives the ticket and
        // announces `joiner_ep`, the endpoint it transfers from.
        let host_ep = local_endpoint(Some(rendezvous_key("abacus-abdomen-abdominal"))).await;
        let pairing_ep = local_endpoint(None).await;
        let joiner_ep = local_endpoint(None).await;
        let rendezvous = host_ep.addr();
        let payload = prepared.ticket(&blobs_ep).to_string();
        let host = tokio::spawn(async move {
            let conn = host_ep.accept().await.unwrap().await.unwrap();
            let (send, recv) = conn.accept_bi().await.unwrap();
            let mut channel = Channel::new(&conn, send, recv);
            let hello = channel.read().await.unwrap();
            let announced = run_host(
                &conn,
                host_ep.id(),
                channel,
                hello,
                terms,
                payload,
                auto(true, shown()),
            )
            .await
            .unwrap();
            assert_ne!(announced, conn.remote_id());
            conn.close(0u32.into(), b"done");
            host_ep.close().await;
            announced
        });
        let conn = pairing_ep.connect(rendezvous, ALPN).await.unwrap();
        let ticket: BlobTicket = run_joiner(
            &conn,
            pairing_ep.id(),
            joiner_ep.id(),
            terms,
            auto(true, shown()),
        )
        .await
        .unwrap()
        .parse()
        .unwrap();
        let paired = host.await.unwrap();
        assert_eq!(paired, joiner_ep.id());
        assert_ne!(paired, pairing_ep.id());

        // Sender side, as in `files::send` after pairing.
        let sender = tokio::spawn(async move {
            let conn = crate::files::accept_receiver(&blobs_ep, paired).await?;
            let mut progress = Progress::new("send", false);
            crate::files::serve_blobs(&prepared.store, conn, &mut progress).await?;
            blobs_ep.close().await;
            Ok::<_, anyhow::Error>(())
        });

        // A third party holding the ticket is refused.
        let intruder = local_endpoint(None).await;
        let conn = intruder
            .connect(ticket.addr().clone(), iroh_blobs::ALPN)
            .await
            .unwrap();
        let closed = timeout(Duration::from_secs(5), conn.closed())
            .await
            .unwrap();
        assert!(
            closed.to_string().contains("not the paired receiver"),
            "{closed}"
        );
        intruder.close().await;

        // So is the endpoint the joiner paired from: the rendezvous identity
        // authenticated the pairing, not the transfer.
        let conn = pairing_ep
            .connect(ticket.addr().clone(), iroh_blobs::ALPN)
            .await
            .unwrap();
        let closed = timeout(Duration::from_secs(5), conn.closed())
            .await
            .unwrap();
        assert!(
            closed.to_string().contains("not the paired receiver"),
            "{closed}"
        );
        pairing_ep.close().await;

        // The paired receiver downloads the content.
        let conn = joiner_ep
            .connect(ticket.addr().clone(), iroh_blobs::ALPN)
            .await
            .unwrap();
        let store = MemStore::new();
        let local = store
            .remote()
            .local(ticket.hash_and_format())
            .await
            .unwrap();
        store
            .remote()
            .execute_get(conn.clone(), local.missing())
            .await
            .unwrap();
        let collection = Collection::load(ticket.hash(), store.as_ref())
            .await
            .unwrap();
        let (_, hash) = collection.iter().next().unwrap();
        assert_eq!(
            &store.blobs().get_bytes(*hash).await.unwrap()[..],
            b"authorized bytes"
        );
        conn.close(0u32.into(), b"verified");
        timeout(Duration::from_secs(5), sender)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        joiner_ep.close().await;
    }
    /// A local official relay on loopback (`IROH_TEST_RELAY_BINARY`).
    struct TestRelay {
        child: std::process::Child,
        url: iroh::RelayUrl,
        _directory: tempfile::TempDir,
    }

    impl TestRelay {
        fn start() -> Self {
            let binary = std::env::var_os("IROH_TEST_RELAY_BINARY")
                .filter(|v| !v.is_empty())
                .expect("IROH_TEST_RELAY_BINARY must point to the official iroh-relay");
            let directory = tempfile::tempdir().unwrap();
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let config = directory.path().join("relay.toml");
            std::fs::write(
                &config,
                format!(
                    "http_bind_addr = \"127.0.0.1:{port}\"\nenable_quic_addr_discovery = false\nenable_metrics = false\n"
                ),
            )
            .unwrap();
            let child = std::process::Command::new(binary)
                .args(["--dev", "--config-path"])
                .arg(&config)
                .env("RUST_LOG", "off")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let started = std::time::Instant::now();
            while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "relay did not start"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Self {
                child,
                url: format!("http://127.0.0.1:{port}").parse().unwrap(),
                _directory: directory,
            }
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Accepts connections on `ep` without ever reading from them, as a
    /// rendezvous does before any `hello`. Returns the direct addresses of the
    /// peer paths seen from this side after `wait`.
    async fn serve_silently(ep: Endpoint, wait: Duration) -> Vec<String> {
        let accepting = ep.clone();
        let seen = tokio::spawn(async move {
            let incoming = accepting.accept().await.expect("an incoming connection");
            let conn = incoming.await.expect("handshake");
            tokio::time::sleep(wait).await;
            paths_of(&conn)
        });
        timeout(wait + Duration::from_secs(15), seen)
            .await
            .unwrap()
            .unwrap()
    }

    fn paths_of(conn: &Connection) -> Vec<String> {
        conn.paths()
            .iter()
            .map(|path| path.remote_addr().to_string())
            .collect()
    }

    /// Connects a stranger to `target` through the relay only (as a resolver
    /// would hand out the published record: the relay URL, no IP address),
    /// stays silent, and returns the stranger's paths and the target's paths.
    async fn stranger_sees(
        settings: &Settings,
        target: Endpoint,
        relay: &iroh::RelayUrl,
    ) -> (Vec<String>, Vec<String>) {
        let wait = Duration::from_secs(6);
        let target_id = target.id();
        let target_side = tokio::spawn(serve_silently(target, wait));
        let stranger = network::bind(settings, vec![]).await.unwrap();
        let conn = stranger
            .connect(
                EndpointAddr::new(target_id).with_relay_url(relay.clone()),
                ALPN,
            )
            .await
            .unwrap();
        tokio::time::sleep(wait).await;
        let stranger_side = paths_of(&conn);
        let target_side = target_side.await.unwrap();
        stranger.close().await;
        (stranger_side, target_side)
    }

    fn has_direct(paths: &[String]) -> bool {
        paths.iter().any(|p| p.starts_with("ip:"))
    }

    /// Item E: a stranger who derives the rendezvous ID and connects, without
    /// a `hello`, must not obtain a direct path to the creator; a regular
    /// keyed endpoint (control) does yield one on the same host.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
    async fn rendezvous_exposes_no_direct_address_to_a_stranger() {
        let relay = TestRelay::start();
        let settings = Settings {
            relay: Some(relay.url.clone()),
            token: None,
            timeout: Duration::from_secs(10),
            relay_only: false,
            direct_only: false,
            no_hints: true,
        };
        // Control: the same keyed endpoint without the relay-only rendezvous.
        let control = network::bind_keyed(
            &settings,
            rendezvous_key("control-words-here"),
            vec![ALPN.to_vec()],
        )
        .await
        .unwrap();
        let (stranger, creator) = stranger_sees(&settings, control, &relay.url).await;
        eprintln!("control   : stranger paths {stranger:?}, creator paths {creator:?}");
        let control_direct = has_direct(&stranger);

        let rendezvous = open(&settings, &Options::default(), MIN_WORDS)
            .await
            .unwrap();
        let addr = rendezvous.ep.addr();
        eprintln!("rendezvous addr: {addr:?}");
        let (stranger, creator) = stranger_sees(&settings, rendezvous.ep.clone(), &relay.url).await;
        eprintln!("rendezvous: stranger paths {stranger:?}, creator paths {creator:?}");
        assert!(
            addr.ip_addrs().next().is_none(),
            "the rendezvous advertises direct addresses"
        );
        assert!(
            !has_direct(&stranger),
            "the stranger got a direct path: {stranger:?}"
        );
        assert!(
            !has_direct(&creator),
            "the creator got a direct path: {creator:?}"
        );
        assert!(
            control_direct,
            "control inconclusive: no direct path on this host even without the mitigation"
        );
    }

    /// Binds a squatter with direct transports on the rendezvous key of a fresh
    /// code, lets `joiner` dial it as a real joiner would, and returns the
    /// paths the squatter sees plus the joiner's advertised IP addresses.
    async fn squatter_sees(settings: &Settings, joiner: Joiner) -> (Vec<String>, usize) {
        let code = generate_code(MIN_WORDS).unwrap();
        let parsed = parse_code(&code).unwrap();
        let squatter = network::bind_keyed(
            settings,
            rendezvous_key(&parsed.rendezvous),
            vec![ALPN.to_vec()],
        )
        .await
        .unwrap();
        let advertised = joiner.ep.addr().ip_addrs().count();
        let seen = tokio::spawn(serve_silently(squatter.clone(), Duration::from_secs(6)));
        let options = Options {
            pairing_timeout: Some(9.0),
            no_verify: true,
        };
        let transfer = SecretKey::from_bytes(&[0x7e; 32]).public();
        let mut progress = Progress::new("test", false);
        let outcome = join(
            settings,
            &options,
            Purpose::Files,
            &code,
            joiner,
            transfer,
            &mut progress,
        )
        .await;
        assert!(outcome.is_err(), "a squatter must not complete a pairing");
        let paths = seen.await.unwrap();
        squatter.close().await;
        (paths, advertised)
    }

    /// The joiner side of item E: whoever squats the rendezvous ID with direct
    /// transports must learn no direct address of the joiner. A joiner that
    /// pairs from its transfer endpoint (the old behavior, built here as the
    /// control) does hand the squatter a direct path on the same host.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires an official iroh-relay binary; run mise run test:pairing"]
    async fn joiner_exposes_no_direct_address_to_a_squatter() {
        let relay = TestRelay::start();
        let settings = Settings {
            relay: Some(relay.url.clone()),
            token: None,
            timeout: Duration::from_secs(10),
            relay_only: false,
            direct_only: false,
            no_hints: true,
        };
        // Control: the joiner pairs from a regular endpoint with direct transports.
        let regular = Joiner {
            ep: network::bind_resolving(&settings, vec![]).await.unwrap(),
        };
        let (seen, advertised) = squatter_sees(&settings, regular).await;
        eprintln!("control: squatter paths {seen:?}, joiner advertises {advertised} IP addresses");
        let control_direct = has_direct(&seen);

        let joiner = prepare_join(&settings).await.unwrap();
        let (seen, advertised) = squatter_sees(&settings, joiner).await;
        eprintln!("joiner: squatter paths {seen:?}, joiner advertises {advertised} IP addresses");
        assert_eq!(
            advertised, 0,
            "the pairing endpoint advertises IP addresses"
        );
        assert!(!seen.is_empty(), "the squatter saw no connection at all");
        assert!(
            !has_direct(&seen),
            "the squatter got a direct path to the joiner: {seen:?}"
        );
        assert!(
            control_direct,
            "control inconclusive: no direct path on this host even without the mitigation"
        );
    }
}
