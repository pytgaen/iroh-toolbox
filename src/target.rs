//! Connection targets: `ticket:`, `code:` and `peer:`.
use std::{io::IsTerminal, net::SocketAddr};

use anyhow::{Context, Result, bail, ensure};
use iroh::{Endpoint, EndpointId, endpoint::Connection};

use crate::{
    config::Settings,
    identity::{Identity, check_slot_name},
    network,
    peers::PeerBook,
    slot::{self, Protocol, Remote},
};

pub enum Target {
    /// A one-time endpoint or blob ticket (or a bare endpoint ID).
    Ticket(String),
    /// A one-use pairing code.
    Code(String),
    /// A paired machine, by local name; `None` selects the command's default slot.
    Peer { name: String, slot: Option<String> },
}

/// Why `receive` and `chat connect` refuse tickets and other targets.
pub const CODE_ONLY: &str = "files and chat take code:WORDS, printed by `iroh-toolbox send PATH` or `chat serve` on the other machine, or peer:NAME of a paired machine; tickets are not accepted";

/// The code of a `code:WORDS` target; every other target is refused. A bare
/// `code:` yields an empty string: [`complete_code`] then reads the words from
/// the terminal, so they never appear in the process arguments.
pub fn parse_code(value: &str, command: &str) -> Result<String> {
    match value.strip_prefix("code:") {
        Some(code) => Ok(code.to_owned()),
        None if crate::pairing::check_code(value).is_ok() => {
            bail!("prefix the code with code:, for example `iroh-toolbox {command} code:<words>`")
        }
        None => bail!(CODE_ONLY),
    }
}

/// The words of a code: `code` as given, or typed at a `Code: ` prompt when
/// it is empty. Other users of this machine can read the arguments of a
/// running command, not what is typed.
pub async fn complete_code(code: String, command: &str) -> Result<String> {
    if !code.trim().is_empty() {
        return Ok(code);
    }
    ensure!(
        std::io::stdin().is_terminal(),
        "{command} reads the code from a terminal; run it in one, or pass `code:WORDS`"
    );
    let typed = read_hidden("Code: ".into()).await?.unwrap_or_default();
    ensure!(!typed.is_empty(), "no code entered");
    Ok(typed)
}

/// The terminal (descriptor and settings) to put back after a hidden read,
/// kept apart so that an interrupt can restore it too. Only the first save of a
/// run is kept: a second one would record the already quiet state.
#[cfg(unix)]
static SAVED_TERMINAL: std::sync::Mutex<Option<(libc::c_int, libc::termios)>> =
    std::sync::Mutex::new(None);

/// Puts the terminal back as it was before a hidden read, if one is running.
/// Called on every way out, including Ctrl+C. The lock is held across the
/// call, so a descriptor is never closed while another thread restores it.
#[cfg(unix)]
pub fn restore_terminal() {
    if let Some((fd, saved)) = SAVED_TERMINAL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        // SAFETY: `fd` is still open (its owner restores before closing it)
        // and `saved` came from tcgetattr.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
}

#[cfg(not(unix))]
pub fn restore_terminal() {}

/// Leaves the process with the terminal restored.
#[cfg(unix)]
fn leave(status: i32) -> ! {
    restore_terminal();
    std::process::exit(status)
}

/// One line typed on the terminal with echo off, so that the code does not stay
/// on the screen or in its scrollback. Windows keeps the echo: hiding it there
/// needs a dependency this tool does not have.
async fn read_hidden(prompt: String) -> Result<Option<String>> {
    #[cfg(unix)]
    {
        let reading = tokio::task::spawn_blocking(move || -> std::io::Result<Option<String>> {
            use std::io::Write;
            let _restore = RestoreOnDrop;
            quiet_terminal(libc::STDIN_FILENO)?;
            eprint!("{prompt}");
            std::io::stderr().flush()?;
            let mut answer = String::new();
            let read = std::io::stdin().read_line(&mut answer);
            restore_terminal();
            // The Enter key was not echoed.
            eprintln!();
            let read = read?;
            Ok((read > 0).then(|| answer.trim().to_owned()))
        });
        // A terminated or hung-up process must not leave the terminal silent.
        use tokio::signal::unix::{SignalKind, signal};
        let (mut term, mut hup, mut quit) = (
            signal(SignalKind::terminate())?,
            signal(SignalKind::hangup())?,
            signal(SignalKind::quit())?,
        );
        let hidden = tokio::select! {
            result = reading => result?,
            _ = term.recv() => leave(143),
            _ = hup.recv() => leave(129),
            _ = quit.recv() => leave(131),
        };
        match hidden {
            Ok(answer) => Ok(answer),
            // Echo cannot be switched off here: type with echo rather than fail.
            Err(_) => {
                note!("warning: cannot hide what you type; the code will be visible");
                crate::pairing::read_answer("Code: ".into()).await
            }
        }
    }
    #[cfg(not(unix))]
    crate::pairing::read_answer(prompt).await
}

#[cfg(unix)]
struct RestoreOnDrop;

#[cfg(unix)]
impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Switches echo off on `fd` and remembers the previous settings for
/// [`restore_terminal`]. Returns the settings it applied.
#[cfg(unix)]
fn quiet_terminal(fd: libc::c_int) -> std::io::Result<libc::termios> {
    // SAFETY: plain termios calls on a terminal checked by the caller.
    let mut mode: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut mode) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut quiet = mode;
    quiet.c_lflag &= !libc::ECHO;
    {
        let mut saved = SAVED_TERMINAL.lock().unwrap_or_else(|e| e.into_inner());
        // Never replace a saved state: `mode` would be a quiet one.
        if saved.is_none() {
            *saved = Some((fd, mode));
        }
    }
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(quiet)
}

/// There is no controlling terminal to ask on. Never raised where there is no
/// termios (Windows), which refuses protected keys another way.
#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub struct NoTerminal;

impl std::fmt::Display for NoTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no controlling terminal")
    }
}

impl std::error::Error for NoTerminal {}

/// The longest passphrase accepted, in bytes. The buffer is allocated once at
/// this size, so that typing never reallocates (and leaves a copy behind).
#[cfg(unix)]
const MAX_SECRET: usize = 1024;

/// Restores the terminal on SIGINT, SIGTERM, SIGHUP and SIGQUIT while a secret is typed,
/// then lets the signal take its default effect. Async-signal-safe: a try_lock,
/// tcsetattr and signal calls only.
#[cfg(unix)]
extern "C" fn restore_and_die(signal: libc::c_int) {
    if let Ok(mut saved) = SAVED_TERMINAL.try_lock()
        && let Some((fd, mode)) = saved.take()
    {
        // SAFETY: async-signal-safe call on a descriptor its owner keeps open.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &mode) };
    }
    // SAFETY: restores the default action, then re-raises (delivered on return).
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Installs [`restore_and_die`] for the length of its life.
#[cfg(unix)]
struct SignalGuard(Vec<(libc::c_int, libc::sigaction)>);

#[cfg(unix)]
impl SignalGuard {
    fn install() -> Self {
        let mut previous = Vec::new();
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            // SAFETY: plain sigaction calls with a valid handler.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = restore_and_die as extern "C" fn(libc::c_int) as usize;
                libc::sigemptyset(&mut action.sa_mask);
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, &action, &mut old) == 0 {
                    previous.push((signal, old));
                }
            }
        }
        Self(previous)
    }
}

#[cfg(unix)]
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (signal, old) in &self.0 {
            // SAFETY: puts back what `install` replaced.
            unsafe { libc::sigaction(*signal, old, std::ptr::null_mut()) };
        }
    }
}

/// A descriptor closed on drop.
#[cfg(unix)]
struct Tty(libc::c_int);

#[cfg(unix)]
impl Drop for Tty {
    fn drop(&mut self) {
        // SAFETY: the descriptor was opened by `read_secret` and is closed once.
        unsafe { libc::close(self.0) };
    }
}

#[cfg(unix)]
fn write_tty(tty: &Tty, text: &str) {
    // SAFETY: a valid descriptor and a valid buffer; a short write only
    // shortens the prompt.
    unsafe { libc::write(tty.0, text.as_ptr().cast(), text.len()) };
}

/// Reads a secret (a passphrase) typed on the controlling terminal (`/dev/tty`,
/// so standard input may be a pipe) with echo off, byte by byte in
/// non-canonical mode, so that nothing is ever echoed and Ctrl+C or Ctrl+D
/// cancel the prompt (no signal is raised, and this blocking read may run on
/// the thread that would handle one). The terminal is restored on every way
/// out, including an external SIGINT, SIGTERM, SIGHUP and SIGQUIT. The bytes go through raw reads
/// into one preallocated buffer of at most [`MAX_SECRET`] bytes: no buffered
/// reader, no reallocation, nothing left behind. There is no fallback to a
/// visible prompt: an unhideable terminal is an error, and so is the lack of a
/// controlling terminal ([`NoTerminal`]). Not supported where there is no
/// termios (Windows).
#[cfg(unix)]
#[cfg_attr(test, allow(dead_code))]
pub fn read_secret(prompt: &str) -> Result<zeroize::Zeroizing<String>> {
    // SAFETY: a plain open of a constant path.
    let fd = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(NoTerminal.into());
    }
    let tty = Tty(fd);
    let _restore = RestoreOnDrop;
    let _signals = SignalGuard::install();
    let mut quiet = quiet_terminal(fd).context("cannot switch off the terminal echo")?;
    quiet.c_lflag &= !(libc::ICANON | libc::ISIG);
    quiet.c_cc[libc::VMIN] = 1;
    quiet.c_cc[libc::VTIME] = 0;
    // SAFETY: `fd` is a valid descriptor and `quiet` is a complete termios.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot switch off the terminal echo");
    }
    write_tty(&tty, prompt);
    let mut bytes = zeroize::Zeroizing::new(Vec::<u8>::with_capacity(MAX_SECRET));
    let mut byte = zeroize::Zeroizing::new([0u8; 1]);
    let outcome = loop {
        // SAFETY: reads at most one byte into a one-byte buffer.
        let read = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
        if read < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            restore_terminal();
            write_tty(&tty, "\n");
            return Err(error).context("cannot read the passphrase");
        }
        if read == 0 {
            break Err("no passphrase entered");
        }
        match byte[0] {
            b'\n' | b'\r' => break Ok(()),
            0x03 | 0x04 => break Err("passphrase entry cancelled"),
            0x7f | 0x08 => {
                // Drop one whole UTF-8 character.
                while let Some(last) = bytes.pop() {
                    if last & 0xC0 != 0x80 {
                        break;
                    }
                }
            }
            b => {
                if bytes.len() == MAX_SECRET {
                    break Err("the passphrase is too long (1024 bytes at most)");
                }
                bytes.push(b);
            }
        }
    };
    restore_terminal();
    // The Enter key was not echoed.
    write_tty(&tty, "\n");
    if let Err(message) = outcome {
        bail!(message);
    }
    ensure!(
        std::str::from_utf8(&bytes).is_ok(),
        "the passphrase must be valid UTF-8 text"
    );
    // Moves the buffer into the string: no copy, and already checked.
    let text = String::from_utf8(std::mem::take(&mut *bytes)).unwrap_or_default();
    Ok(zeroize::Zeroizing::new(text))
}

#[cfg(not(unix))]
#[cfg_attr(test, allow(dead_code))]
pub fn read_secret(_prompt: &str) -> Result<zeroize::Zeroizing<String>> {
    bail!(
        "this platform cannot hide a typed passphrase; passphrase protection is not available here"
    )
}

const FORMS: &str = "use ticket:…, code:…, peer:NAME or a ticket printed by the other machine";

pub fn parse(value: &str) -> Result<Target> {
    if let Some(ticket) = value.strip_prefix("ticket:") {
        return Ok(Target::Ticket(ticket.to_owned()));
    }
    if value.starts_with("id:") {
        bail!(
            "id: targets no longer exist; pair the machines with `iroh-toolbox pair`, then use peer:NAME"
        );
    }
    if let Some(code) = value.strip_prefix("code:") {
        return Ok(Target::Code(code.to_owned()));
    }
    if value.starts_with("peer:") {
        let (target, service) = parse_service(value)?;
        ensure!(
            service.is_none(),
            "a /SERVICE segment is accepted by `tunnel tcp connect` only; use peer:NAME[/SLOT]"
        );
        return Ok(target);
    }
    // Tickets name their own kind; bare endpoint IDs remain accepted.
    if value.starts_with("endpoint")
        || value.starts_with("blob")
        || value.parse::<EndpointId>().is_ok()
    {
        return Ok(Target::Ticket(value.to_owned()));
    }
    bail!("unrecognized target; {FORMS}")
}

/// Parses a target that may name a service of a tunnel slot:
/// `peer:NAME/SLOT/SERVICE`. Other targets are parsed as by [`parse`].
pub fn parse_service(value: &str) -> Result<(Target, Option<String>)> {
    let Some(rest) = value.strip_prefix("peer:") else {
        return Ok((parse(value)?, None));
    };
    let mut segments = rest.splitn(3, '/');
    let name = segments.next().unwrap_or_default();
    let slot = segments.next();
    let service = segments.next();
    check_slot_name(name).context("invalid peer name")?;
    if let Some(slot) = slot {
        check_slot_name(slot).context("invalid slot name")?;
    }
    if let Some(service) = service {
        check_slot_name(service).context("invalid service name")?;
    }
    let target = Target::Peer {
        name: name.to_owned(),
        slot: slot.map(str::to_owned),
    };
    Ok((target, service.map(str::to_owned)))
}

impl Target {
    /// The paired machine and slot behind a `peer:` target, with the direct
    /// addresses of `--addr`. Tunnel and pipe clients accept nothing else.
    pub fn remote(
        &self,
        command: &str,
        protocol: Protocol,
        addrs: &[SocketAddr],
        settings: &Settings,
    ) -> Result<Remote> {
        match self {
            Self::Peer { name, slot } => {
                let address = PeerBook::load()?.resolve(name)?;
                ensure!(
                    !settings.direct_only || !addrs.is_empty(),
                    "the server cannot be found without a relay; pass --addr HOST:PORT printed by the server"
                );
                // The server may ask for this machine's proof: a protected key
                // is decrypted now, before any listener or readiness line, so
                // that the passphrase prompt comes first and comes once.
                Identity::load_or_create()?;
                Ok(Remote {
                    peer: name.clone(),
                    address,
                    slot: slot
                        .clone()
                        .unwrap_or_else(|| protocol.default_slot().to_owned()),
                    direct: addrs.to_vec(),
                })
            }
            // Tunnels and pipes give access to data: paired machines only.
            Self::Ticket(_) | Self::Code(_) if protocol.requires_client() => bail!(
                "tunnel and pipe connect to paired machines only; pair with `iroh-toolbox pair`, then use peer:NAME"
            ),
            Self::Ticket(_) | Self::Code(_) => {
                bail!("{command} accepts a ticket or peer:NAME only")
            }
        }
    }
}

/// Parses repeated `--addr HOST:PORT` values, resolving host names.
pub async fn direct_addrs(values: &[String]) -> Result<Vec<SocketAddr>> {
    let mut addrs = Vec::new();
    for value in values {
        let resolved = tokio::net::lookup_host(value.as_str())
            .await
            .map_err(|_| anyhow::anyhow!("--addr {value}: expected HOST:PORT"))?;
        let found: Vec<_> = resolved.collect();
        ensure!(!found.is_empty(), "--addr {value}: no address found");
        addrs.extend(found);
    }
    Ok(addrs)
}

/// A connect command's target with the direct addresses it was given.
pub struct Destination {
    pub target: Target,
    pub addrs: Vec<SocketAddr>,
}

impl Destination {
    /// Parses `target` and the `--addr` values; `--addr` applies to `peer:` only.
    pub async fn parse(target: &str, addrs: &[String]) -> Result<Self> {
        Self::with_addrs(parse(target)?, addrs).await
    }

    /// Like [`Destination::parse`], with the service a `peer:NAME/SLOT/SERVICE`
    /// target names.
    pub async fn parse_service(target: &str, addrs: &[String]) -> Result<(Self, Option<String>)> {
        let (target, service) = parse_service(target)?;
        Ok((Self::with_addrs(target, addrs).await?, service))
    }

    async fn with_addrs(target: Target, addrs: &[String]) -> Result<Self> {
        ensure!(
            addrs.is_empty() || matches!(target, Target::Peer { .. }),
            "--addr applies to peer: targets only; a ticket carries its own addresses"
        );
        Ok(Self {
            target,
            addrs: direct_addrs(addrs).await?,
        })
    }
}

/// Binds the client endpoint a target needs: a peer may need address lookup.
pub async fn bind_client(settings: &Settings, target: &Target) -> Result<Endpoint> {
    match target {
        Target::Peer { .. } if !settings.direct_only => {
            network::bind_resolving(settings, vec![]).await
        }
        _ => network::bind(settings, vec![]).await,
    }
}

/// Connects a command's client to a ticket or a slot of a paired machine.
pub async fn connect(
    ep: &Endpoint,
    destination: &Destination,
    alpn: &[u8],
    protocol: Protocol,
    command: &str,
    settings: &Settings,
) -> Result<Connection> {
    match &destination.target {
        Target::Ticket(ticket) => network::connect(ep, ticket, alpn, settings).await,
        target => {
            let remote = target.remote(command, protocol, &destination.addrs, settings)?;
            slot::connect(ep, &remote, protocol, settings).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_are_recognized_by_prefix_or_ticket_kind() {
        assert!(
            matches!(parse("ticket:endpointabc").unwrap(), Target::Ticket(t) if t == "endpointabc")
        );
        assert!(matches!(parse("endpointabc").unwrap(), Target::Ticket(_)));
        assert!(matches!(parse("blobabc").unwrap(), Target::Ticket(_)));
        let id = iroh::SecretKey::from_bytes(&[3; 32]).public().to_string();
        assert!(matches!(parse(&id).unwrap(), Target::Ticket(_)));
        assert!(matches!(parse("code:a-b-c").unwrap(), Target::Code(c) if c == "a-b-c"));
        assert!(
            matches!(parse("peer:nas").unwrap(), Target::Peer { name, slot: None } if name == "nas")
        );
        assert!(
            matches!(parse("peer:nas/ssh").unwrap(), Target::Peer { name, slot: Some(s) } if name == "nas" && s == "ssh")
        );
        let machine = iroh::SecretKey::from_bytes(&[5; 32]).public().to_z32();
        for bad in [
            "nas".to_owned(),
            "abacus-abdomen".to_owned(),
            "peer:".to_owned(),
            "peer:NAS".to_owned(),
            "peer:nas/SSH".to_owned(),
            "peer:nas/".to_owned(),
            "peer:all".to_owned(),
            "id:".to_owned(),
            format!("id:{machine}+ybndrfg8ejkmcpqx/ssh"),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
        let error = parse("peer:nas/dev/web").err().unwrap().to_string();
        assert!(error.contains("tunnel tcp connect"), "{error}");
        let (target, service) = parse_service("peer:nas/dev/web").unwrap();
        assert!(
            matches!(target, Target::Peer { name, slot: Some(s) } if name == "nas" && s == "dev")
        );
        assert_eq!(service.as_deref(), Some("web"));
        assert!(parse_service("peer:nas/dev").unwrap().1.is_none());
        assert!(parse_service("ticket:endpointabc").unwrap().1.is_none());
        for bad in [
            "peer:nas//web",
            "peer:nas/dev/",
            "peer:nas/dev/Web",
            "peer:nas/dev/web/x",
            "peer:nas/dev/all",
        ] {
            assert!(parse_service(bad).is_err(), "{bad}");
        }
        let error = parse("id:abc").err().unwrap().to_string();
        assert!(error.contains("peer:NAME"), "{error}");
    }
}
