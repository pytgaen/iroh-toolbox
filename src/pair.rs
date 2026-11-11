//! The `pair` command: pairs two machines once, so that each can later join
//! the peer services the other runs (see docs/reference/pairing-protocol.md).
//! Pairing starts no service and grants no slot: each server narrows the
//! paired machines with its own `--allow`.
use std::{io::IsTerminal, sync::Arc};

use anyhow::{Result, bail, ensure};

use crate::{
    cli::PairOptions,
    config::Settings,
    identity::{self, Identity, check_slot_name, short_key},
    pairing,
    peers::PeerBook,
    progress::Progress,
    target::{self, Target},
};

pub async fn run(settings: &Settings, options: PairOptions) -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal(),
        "pair needs an interactive terminal on each machine: both people compare a confirmation number"
    );
    ensure!(
        !settings.direct_only,
        "pair meets the other machine through a relay; remove --direct-only"
    );
    let code = match options.target.as_deref().map(target::parse).transpose()? {
        None => None,
        Some(Target::Code(code)) => {
            let code = target::complete_code(code, "pair").await?;
            pairing::check_code(&code)?;
            Some(code)
        }
        Some(_) => bail!("pair accepts code:WORDS only; run it without a target to create a code"),
    };
    for name in [&options.announce, &options.name].into_iter().flatten() {
        check_slot_name(name)?;
    }
    // Fail now on an unsafe or malformed book, not after both people confirmed.
    PeerBook::load()?;
    let (identity, created) = Identity::load_or_create()?;
    if created {
        note!(
            "created this machine's identity in {}",
            identity::home()?.display()
        );
    }
    let announced = match options.announce {
        Some(name) => name,
        None => identity::machine_name()?,
    };
    note!("This machine: {announced}");
    let identity = Arc::new(identity);
    let address = identity.address();
    let local = pairing::Local::new(address, announced, move |message| identity.sign(message));
    let setup = pairing::Setup::terminal(local, options.name);
    // The code is always verified, so the options only carry the deadline.
    // The default leaves time to dictate the code and compare the number.
    let pairing = pairing::Options {
        pairing_timeout: options.pairing_timeout.or(Some(300.0)),
        no_verify: false,
    };
    let mut progress = Progress::new("pair", true);
    let outcome = match code {
        Some(code) => {
            // Relay-only pairing endpoint, closed when the pairing ends.
            let joiner = pairing::prepare_join(settings).await?;
            pairing::pair_join(settings, &pairing, &code, joiner, setup, &mut progress).await?
        }
        None => {
            let words = options.code_words.map_or(pairing::MIN_WORDS, usize::from);
            let rendezvous = pairing::open(settings, &pairing, words).await?;
            pairing::pair_host(settings, rendezvous, setup, &mut progress).await?
        }
    };
    let peer = outcome.peer;
    note!(
        "Paired with {} (machine {})",
        peer.name,
        short_key(&peer.machine())
    );
    if !outcome.confirmed {
        note!(
            "warning: {0} did not confirm that it stored this pairing; pair again, or remove it with `iroh-toolbox peers rm {0}`",
            peer.name
        );
    }
    Ok(())
}
