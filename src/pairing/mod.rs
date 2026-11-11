//! Serverless code pairing (see docs/reference/pairing-protocol.md).
//!
//! The first words of a code derive a temporary iroh endpoint key: the joiner
//! reaches that rendezvous endpoint by its ID through iroh relays and address
//! lookup, without any mailbox server. The remaining words are the SPAKE2
//! password. Both sides pair from relay-only endpoints, so a squatter of the
//! rendezvous learns no direct address; the joiner announces the endpoint it
//! will transfer from inside the authenticated channel. Only tickets and
//! endpoint IDs cross the pairing connection; file data never does.
//!
//! - `code`: word list, generation, parsing, rendezvous key.
//! - `frame`: the closed set of messages, the channel and the transcript.
//! - `handshake`: SPAKE2, key derivation, key confirmation, confirmation
//!   number (no terminal, peer book or endpoint binding).
//! - `record`: machine records, signatures and the peer book logic.
//! - `session`: rendezvous, terminal interaction and the commands.
use std::{io::IsTerminal, time::Duration};

use anyhow::{Result, ensure};
use tokio::time::Instant;

use crate::config::Settings;

mod code;
mod frame;
mod handshake;
mod record;
mod session;

pub use code::{MIN_WORDS, check_code, normalize_code};
pub use record::Local;
pub use session::{
    Rendezvous, Setup, Side, open, pair_chat, pair_host, pair_join, prepare_join, read_answer,
    receive_ticket, send_ticket,
};

const ALPN: &[u8] = b"iroh-toolbox/pairing/1";
/// Typing mistakes allowed for the other machine's confirmation number.
const CONFIRM_ATTEMPTS: usize = 3;

type Pending<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

#[derive(clap::Args, Default)]
pub struct Options {
    /// Total pairing deadline, including human confirmation (default: 300 seconds, 120 with --no-verify).
    #[arg(long, value_parser = crate::cli::seconds)]
    pub pairing_timeout: Option<f64>,
    /// Skip the 6-digit confirmation number: whoever uses the code first is served. Needed on both machines.
    #[arg(long)]
    pub no_verify: bool,
}

impl Options {
    pub fn validate(&self, settings: &Settings) -> Result<()> {
        self.check(settings, false)
    }

    /// Like [`Options::validate`] for a reception that resumes an interrupted
    /// one: it does not pair, so no confirmation number is asked and no
    /// terminal is needed.
    pub fn validate_resuming(&self, settings: &Settings) -> Result<()> {
        self.check(settings, true)
    }

    fn check(&self, settings: &Settings, resuming: bool) -> Result<()> {
        ensure!(
            !settings.direct_only,
            "code pairing meets the other machine through a relay; remove --direct-only"
        );
        if self.verify() && !resuming {
            ensure!(
                std::io::stdin().is_terminal(),
                "the confirmation number needs an interactive terminal; pass --no-verify on both machines to skip it (whoever uses the code first is then served)"
            );
        }
        Ok(())
    }

    /// Whether the confirmation number is asked: the default.
    pub fn verify(&self) -> bool {
        !self.no_verify
    }

    fn deadline(&self) -> Instant {
        // Two people read numbers to each other, as for `pair`.
        let default = if self.verify() { 300.0 } else { 120.0 };
        Instant::now() + Duration::from_secs_f64(self.pairing_timeout.unwrap_or(default))
    }
}
