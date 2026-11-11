//! Machine records: the signed statement, its verification, and the peer book
//! logic of a machine pairing.
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use iroh::{PublicKey, Signature};
use tokio::time::Instant;

use super::{
    Pending,
    frame::{Body, Frame, PeerRecord, Reason},
    handshake::{Purpose, Role, Session, Terms, check_tag, tag},
};
use crate::{
    identity::{Address, check_slot_name, short_key},
    peers::{BookGuard, Peer, PeerBook},
};

type Signer = Box<dyn Fn(&[u8]) -> Signature + Send + Sync>;

/// What this machine presents: its address, the name it announces and the
/// means to sign with its machine key.
pub struct Local {
    address: Address,
    announced: String,
    signer: Signer,
}

impl Local {
    pub fn new(
        address: Address,
        announced: String,
        signer: impl Fn(&[u8]) -> Signature + Send + Sync + 'static,
    ) -> Self {
        Self {
            address,
            announced,
            signer: Box::new(signer),
        }
    }

    pub(super) fn record(&self, role: Role, transcript: &[u8; 32]) -> PeerRecord {
        let statement = statement(role, transcript, &self.address, &self.announced);
        PeerRecord {
            machine: self.address.machine.to_z32(),
            salt: self.address.salt_z32(),
            name: self.announced.clone(),
            signature: hex::encode((self.signer)(&statement).to_bytes()),
        }
    }
}

/// The signed statement. Variable-length fields carry a one-byte length (names
/// are at most 32 bytes), so no two statements can encode the same bytes.
fn statement(role: Role, transcript: &[u8; 32], address: &Address, name: &str) -> Vec<u8> {
    let role: &[u8] = match role {
        Role::Host => b"host",
        Role::Joiner => b"joiner",
    };
    let mut out = b"iroh-toolbox/pair/v1".to_vec();
    out.push(role.len() as u8);
    out.extend_from_slice(role);
    out.extend_from_slice(transcript);
    out.extend_from_slice(address.machine.as_bytes());
    out.extend_from_slice(address.salt());
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    out
}

/// Checks the other machine's record: valid name, not our own key, and a strict
/// Ed25519 signature by the announced key over this session's transcript.
fn check_record(
    record: &PeerRecord,
    role: Role,
    transcript: &[u8; 32],
    own: &PublicKey,
) -> Result<(Address, String)> {
    let invalid = || anyhow::anyhow!("pairing failed: the other machine sent an invalid record");
    check_slot_name(&record.name).map_err(|_| {
        anyhow::anyhow!("pairing failed: the other machine announced an invalid name")
    })?;
    let address = Address::from_z32(&record.machine, &record.salt).ok_or_else(invalid)?;
    ensure!(
        address.machine != *own,
        "pairing failed: the other machine presented this machine's own key"
    );
    let bytes: [u8; Signature::LENGTH] = hex::decode(&record.signature)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(invalid)?;
    // `PublicKey::verify` is Ed25519 strict verification.
    address
        .machine
        .verify(
            &statement(role, transcript, &address, &record.name),
            &Signature::from_bytes(&bytes),
        )
        .map_err(|_| anyhow::anyhow!("pairing failed: the other machine's signature is invalid"))?;
    Ok((address, record.name.clone()))
}

/// The peer book under its lock, edited in memory until [`Held::commit`].
pub trait Held: Send {
    fn book(&mut self) -> &mut PeerBook;
    /// Writes the book and releases the lock.
    fn commit(self: Box<Self>) -> Pending<Result<()>>;
}

impl Held for BookGuard {
    fn book(&mut self) -> &mut PeerBook {
        self
    }

    /// The write syncs to disk: keep it off the async workers.
    fn commit(self: Box<Self>) -> Pending<Result<()>> {
        Box::pin(
            async move { tokio::task::spawn_blocking(move || BookGuard::commit(&self)).await? },
        )
    }
}

/// Application close code of a host that stored the pairing. The joiner has no
/// other way to know that the host's `commit` succeeded after its last frame.
pub(super) const STORED: u32 = 14;
/// Margin before the deadline below which a machine no longer sends `done`:
/// the other side could not store the pairing in time, leaving one-sided entries.
const DONE_MARGIN: Duration = Duration::from_secs(5);

fn ensure_time(deadline: Instant) -> Result<()> {
    ensure!(
        deadline.saturating_duration_since(Instant::now()) >= DONE_MARGIN,
        "pairing ran out of time before it could be completed; nothing was stored; pair again"
    );
    Ok(())
}

/// A finished pairing: the stored peer, and whether the host confirmed that it
/// stored this machine too.
pub struct Outcome {
    pub peer: Peer,
    pub confirmed: bool,
}

/// Takes the book lock; may wait for another writer.
pub type Lock = Box<dyn FnOnce() -> Pending<Result<Box<dyn Held>>> + Send>;
/// Shows why a name cannot be used and returns another one; `None` cancels.
pub type AskName = Box<dyn FnMut(String) -> Pending<Result<Option<String>>> + Send>;

/// Local decisions of one pairing. Prompts are injectable like [`Confirm`] so
/// that tests can drive both machines.
pub struct Choices {
    /// `--name`: the local name of the other machine.
    pub name: Option<String>,
    pub lock: Lock,
    pub ask_name: AskName,
}

/// The book entry that already uses `name` for a machine other than `machine`.
fn holder<'a>(book: &'a PeerBook, machine: &PublicKey, name: &str) -> Option<&'a Peer> {
    book.get(name).filter(|peer| peer.machine() != *machine)
}

/// Verifies the other machine's record, takes the book lock and settles the
/// local name. Nothing is stored yet: the returned book is edited in
/// memory and committed only once the other machine confirmed with `done`.
async fn settle(
    session: &mut Session,
    role: Role,
    transcript: &[u8; 32],
    record: &PeerRecord,
    local: &Local,
    choices: Choices,
) -> Result<(Box<dyn Held>, Peer)> {
    let Choices {
        name,
        lock,
        mut ask_name,
    } = choices;
    let (address, announced) = check_record(record, role, transcript, &local.address.machine)?;
    let mut held = lock().await?;
    let existing = held.book().by_machine(&address.machine).cloned();
    // A known machine keeps its local name unless `--name` is given.
    let mut name = name
        .or_else(|| existing.as_ref().map(|peer| peer.name.clone()))
        .unwrap_or(announced.clone());
    loop {
        let message = if let Err(problem) = check_slot_name(&name) {
            format!("{problem}.")
        } else if let Some(owner) = holder(held.book(), &address.machine, &name) {
            format!(
                "The name \"{name}\" is used by machine {}. If that machine rotated its identity, remove the old entry first with `iroh-toolbox peers rm {name}`.",
                short_key(&owner.machine())
            )
        } else {
            break;
        };
        session.channel.check_open()?;
        match ask_name(message)
            .await?
            .map(|answer| answer.trim().to_owned())
        {
            Some(answer) if !answer.is_empty() => name = answer,
            _ => {
                session.channel.abort(Reason::NameTaken).await;
                bail!("name taken; retry with --name ALIAS");
            }
        }
    }
    let peer = held
        .book()
        .add_pairing(address, &announced, Some(&name))?
        .clone();
    Ok((held, peer))
}

/// Host: exchange signed records, settle locally, then exchange transcript MACs.
pub(super) async fn host_pair(
    session: &mut Session,
    local: Local,
    choices: Choices,
    deadline: Instant,
) -> Result<Peer> {
    let transcript = session.channel.digest();
    let mine = local.record(Role::Host, &transcript);
    session
        .channel
        .write(&Frame::Payload(Body::Peer { peer: mine }))
        .await?;
    let Frame::Payload(Body::Peer { peer: theirs }) = session.channel.read().await? else {
        bail!("invalid pairing message");
    };
    let (held, peer) = settle(session, Role::Joiner, &transcript, &theirs, &local, choices).await?;
    let transcript = session.channel.digest();
    ensure_time(deadline)?;
    session
        .channel
        .write(&Frame::Done {
            mac: tag(&session.keys.done, &[b"host", &transcript]),
        })
        .await?;
    let Frame::Done { mac } = session.channel.read().await? else {
        bail!("invalid pairing message");
    };
    ensure!(
        check_tag(&session.keys.done, &[b"joiner", &transcript], &mac),
        "pairing failed: the other machine did not confirm the pairing"
    );
    held.commit().await?;
    Ok(peer)
}

/// Joiner: mirror of [`host_pair`]; commits after answering the host's `done`.
pub(super) async fn join_pair(
    session: &mut Session,
    local: Local,
    choices: Choices,
    deadline: Instant,
) -> Result<Peer> {
    let transcript = session.channel.digest();
    let Frame::Payload(Body::Peer { peer: theirs }) = session.channel.read().await? else {
        bail!("invalid pairing message");
    };
    let mine = local.record(Role::Joiner, &transcript);
    session
        .channel
        .write(&Frame::Payload(Body::Peer { peer: mine }))
        .await?;
    let (held, peer) = settle(session, Role::Host, &transcript, &theirs, &local, choices).await?;
    let transcript = session.channel.digest();
    let Frame::Done { mac } = session.channel.read().await? else {
        bail!("invalid pairing message");
    };
    ensure!(
        check_tag(&session.keys.done, &[b"host", &transcript], &mac),
        "pairing failed: the pairing was not confirmed"
    );
    ensure_time(deadline)?;
    session
        .channel
        .write(&Frame::Done {
            mac: tag(&session.keys.done, &[b"joiner", &transcript]),
        })
        .await?;
    session.channel.send.finish()?;
    held.commit().await?;
    Ok(peer)
}

pub(super) fn pair_terms(password: &str) -> Terms<'_> {
    Terms {
        purpose: Purpose::Pair,
        password,
        verify: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_done_is_sent_when_the_deadline_is_too_close() {
        assert!(ensure_time(Instant::now() + Duration::from_secs(60)).is_ok());
        assert!(ensure_time(Instant::now() + Duration::from_secs(1)).is_err());
    }

    fn signed(role: Role, transcript: &[u8; 32], key: &SecretKey, name: &str) -> PeerRecord {
        let address = Address::from_parts(key.public(), [4; 10]);
        let signature = key.sign(&statement(role, transcript, &address, name));
        PeerRecord {
            machine: key.public().to_z32(),
            salt: address.salt_z32(),
            name: name.into(),
            signature: hex::encode(signature.to_bytes()),
        }
    }

    #[test]
    fn records_are_bound_to_role_transcript_key_and_name() {
        let (key, own) = (
            SecretKey::from_bytes(&[3; 32]),
            SecretKey::from_bytes(&[4; 32]),
        );
        let transcript = [9u8; 32];
        let record = signed(Role::Host, &transcript, &key, "nas");
        let check = |record: &PeerRecord, role, transcript| {
            check_record(record, role, transcript, &own.public()).map(|(_, name)| name)
        };
        assert_eq!(check(&record, Role::Host, &transcript).unwrap(), "nas");
        // Wrong role, wrong transcript, altered name or machine, damaged signature.
        assert!(check(&record, Role::Joiner, &transcript).is_err());
        assert!(check(&record, Role::Host, &[8u8; 32]).is_err());
        let renamed = PeerRecord {
            name: "evil".into(),
            ..signed(Role::Host, &transcript, &key, "nas")
        };
        assert!(check(&renamed, Role::Host, &transcript).is_err());
        let moved = PeerRecord {
            machine: own.public().to_z32(),
            ..signed(Role::Host, &transcript, &key, "nas")
        };
        assert!(check(&moved, Role::Host, &transcript).is_err());
        let mut damaged = signed(Role::Host, &transcript, &key, "nas");
        damaged.signature = format!("00{}", &damaged.signature[2..]);
        assert!(check(&damaged, Role::Host, &transcript).is_err());
        damaged.signature = "zz".into();
        assert!(check(&damaged, Role::Host, &transcript).is_err());
        // A record carrying this machine's own key is refused even when well signed.
        let echoed = signed(Role::Host, &transcript, &own, "nas");
        let error = check(&echoed, Role::Host, &transcript).unwrap_err();
        assert!(error.to_string().contains("own key"), "{error}");
    }

    #[test]
    fn announced_names_must_be_valid_slot_names() {
        let (key, own) = (
            SecretKey::from_bytes(&[3; 32]),
            SecretKey::from_bytes(&[4; 32]),
        );
        let transcript = [9u8; 32];
        for name in [
            "",
            "NAS",
            "a b",
            "all",
            "none",
            &"x".repeat(33),
            &"x".repeat(300),
        ] {
            let record = signed(Role::Host, &transcript, &key, name);
            assert!(
                check_record(&record, Role::Host, &transcript, &own.public()).is_err(),
                "{name}"
            );
        }
    }

    /// Frozen vector: a change to an expected value is a wire-format change
    /// (signatures of the previous format stop verifying).
    #[test]
    fn the_signed_pair_statement_matches_the_frozen_bytes() {
        let key = SecretKey::from_bytes(&[3; 32]);
        let address = Address::from_parts(key.public(), [4; 10]);
        let transcript = [9u8; 32];
        for (role, expected) in [
            (
                Role::Host,
                "69726f682d746f6f6c626f782f706169722f763104686f73740909090909090909090909090909090909090909090909090909090909090909ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d104040404040404040404036e6173",
            ),
            (
                Role::Joiner,
                "69726f682d746f6f6c626f782f706169722f7631066a6f696e65720909090909090909090909090909090909090909090909090909090909090909ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d104040404040404040404036e6173",
            ),
        ] {
            let bytes = statement(role, &transcript, &address, "nas");
            assert_eq!(hex::encode(bytes), expected);
        }
    }
}
