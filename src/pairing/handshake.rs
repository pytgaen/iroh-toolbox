//! The cryptographic core: SPAKE2 handshake, key derivation, key confirmation
//! and the confirmation number. It knows no terminal, peer book or endpoint
//! binding: the person is reached through the [`Confirm`] callback.
use std::{future::Future, pin::Pin};

use anyhow::{Context, Result, bail, ensure};
use curve25519_dalek::{edwards::CompressedEdwardsY, traits::IsIdentity};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use iroh::{EndpointId, endpoint::Connection};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use super::frame::{Body, Channel, Frame, Reason, WRONG_CODE};

pub(super) const VERSION: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Purpose {
    Files,
    Chat,
    /// Machine pairing: exchanges signed identities instead of a ticket.
    Pair,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Chat => "chat",
            Self::Pair => "pair",
        }
    }

    fn command(self) -> &'static str {
        match self {
            Self::Files => "send/receive",
            Self::Chat => "chat serve/connect",
            Self::Pair => "pair",
        }
    }
}

fn decode(value: &str, length: usize) -> Result<Vec<u8>> {
    hex::decode(value)
        .ok()
        .filter(|bytes| bytes.len() == length)
        .context("invalid pairing message")
}

// --- Keys -------------------------------------------------------------------

pub(super) struct Keys {
    host: Zeroizing<[u8; 32]>,
    joiner: Zeroizing<[u8; 32]>,
    sas: Zeroizing<[u8; 32]>,
    pub(super) done: Zeroizing<[u8; 32]>,
}

impl Keys {
    pub(super) fn derive(shared: &[u8], transcript: &[u8; 32]) -> Self {
        let hkdf = Hkdf::<Sha256>::new(Some(transcript), shared);
        let key = |label: &[u8]| {
            let mut out = Zeroizing::new([0u8; 32]);
            hkdf.expand(label, &mut *out)
                .expect("32 bytes is a valid HKDF length");
            out
        };
        Self {
            host: key(b"iroh-toolbox/pairing/v1/confirm-host"),
            joiner: key(b"iroh-toolbox/pairing/v1/confirm-joiner"),
            sas: key(b"iroh-toolbox/pairing/v1/sas"),
            done: key(b"iroh-toolbox/pairing/v1/done"),
        }
    }
}

fn mac(key: &[u8; 32], parts: &[&[u8]]) -> Hmac<Sha256> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    for part in parts {
        mac.update(part);
    }
    mac
}

pub(super) fn tag(key: &[u8; 32], parts: &[&[u8]]) -> String {
    hex::encode(mac(key, parts).finalize().into_bytes())
}

pub(super) fn check_tag(key: &[u8; 32], parts: &[&[u8]], value: &str) -> bool {
    decode(value, 32).is_ok_and(|bytes| mac(key, parts).verify_slice(&bytes).is_ok())
}

/// Rejects small-order and mixed-order points, which `spake2` 0.4 accepts
/// (see notes/spake2-review.md).
pub(super) fn check_element(message: &[u8], side: u8) -> Result<()> {
    ensure!(
        message.len() == 33 && message[0] == side,
        "invalid pairing key exchange"
    );
    let bytes: [u8; 32] = message[1..].try_into().expect("length checked");
    let point = CompressedEdwardsY(bytes)
        .decompress()
        .context("invalid pairing key exchange")?;
    ensure!(
        point.is_torsion_free() && !point.is_identity(),
        "invalid pairing key exchange"
    );
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Host,
    Joiner,
}

/// What both machines must agree on for one pairing attempt.
#[derive(Clone, Copy)]
pub(super) struct Terms<'a> {
    pub(super) purpose: Purpose,
    pub(super) password: &'a str,
    pub(super) verify: bool,
}

pub(super) struct Session {
    pub(super) channel: Channel,
    pub(super) keys: Keys,
    pub(super) transcript: [u8; 32],
}

pub(super) fn check_hello(
    version: u32,
    purpose: &str,
    verify: bool,
    expected: Purpose,
    local_verify: bool,
) -> Result<()> {
    ensure!(
        version == VERSION,
        "the other machine uses an incompatible iroh-toolbox version; update both machines"
    );
    ensure!(
        purpose == expected.as_str(),
        "the other machine is not running {}; run matching commands with a new code",
        expected.command()
    );
    // Pairing records a trusted machine: the confirmation number is never optional,
    // and `pair` has no --no-verify option to suggest.
    ensure!(
        expected != Purpose::Pair || verify,
        "the other machine did not ask for the confirmation number, which pair always requires; create a new code"
    );
    ensure!(
        verify == local_verify,
        "use --no-verify on both machines, or on neither; create a new code"
    );
    Ok(())
}

/// Host side: the joiner's Hello was already read (and recorded) by the caller.
async fn host_handshake(
    mut channel: Channel,
    hello: Frame,
    terms: Terms<'_>,
    ids: (EndpointId, EndpointId),
) -> Result<Session> {
    let Terms {
        purpose,
        password,
        verify,
    } = terms;
    let Frame::Hello {
        version,
        purpose: peer_purpose,
        verify: peer_verify,
        pake,
    } = hello
    else {
        bail!("invalid pairing message");
    };
    if let Err(error) = check_hello(version, &peer_purpose, peer_verify, purpose, verify) {
        channel.abort(Reason::Mismatch).await;
        return Err(error);
    }
    let joiner_message = decode(&pake, 33)?;
    check_element(&joiner_message, b'B')?;
    let (state, message) = Spake2::<Ed25519Group>::start_a(
        &Password::new(password),
        &Identity::new(ids.0.as_bytes()),
        &Identity::new(ids.1.as_bytes()),
    );
    channel
        .write(&Frame::Hello {
            version: VERSION,
            purpose: purpose.as_str().into(),
            verify,
            pake: hex::encode(&message),
        })
        .await?;
    let shared = Zeroizing::new(
        state
            .finish(&joiner_message)
            .map_err(|_| anyhow::anyhow!("invalid pairing key exchange"))?,
    );
    let transcript = channel.digest();
    let keys = Keys::derive(&shared, &transcript);
    channel
        .write(&Frame::Confirm {
            mac: tag(&keys.host, &[&transcript]),
        })
        .await?;
    let Frame::Confirm { mac } = channel.read().await? else {
        bail!("invalid pairing message");
    };
    if !check_tag(&keys.joiner, &[&transcript], &mac) {
        channel.abort(Reason::Failed).await;
        bail!(WRONG_CODE);
    }
    Ok(Session {
        channel,
        keys,
        transcript,
    })
}

async fn join_handshake(
    mut channel: Channel,
    terms: Terms<'_>,
    ids: (EndpointId, EndpointId),
) -> Result<Session> {
    let Terms {
        purpose,
        password,
        verify,
    } = terms;
    let (state, message) = Spake2::<Ed25519Group>::start_b(
        &Password::new(password),
        &Identity::new(ids.0.as_bytes()),
        &Identity::new(ids.1.as_bytes()),
    );
    channel
        .write(&Frame::Hello {
            version: VERSION,
            purpose: purpose.as_str().into(),
            verify,
            pake: hex::encode(&message),
        })
        .await?;
    let Frame::Hello {
        version,
        purpose: peer_purpose,
        verify: peer_verify,
        pake,
    } = channel.read().await?
    else {
        bail!("invalid pairing message");
    };
    if let Err(error) = check_hello(version, &peer_purpose, peer_verify, purpose, verify) {
        channel.abort(Reason::Mismatch).await;
        return Err(error);
    }
    let host_message = decode(&pake, 33)?;
    check_element(&host_message, b'A')?;
    let shared = Zeroizing::new(
        state
            .finish(&host_message)
            .map_err(|_| anyhow::anyhow!("invalid pairing key exchange"))?,
    );
    let transcript = channel.digest();
    let keys = Keys::derive(&shared, &transcript);
    let Frame::Confirm { mac } = channel.read().await? else {
        bail!("invalid pairing message");
    };
    if !check_tag(&keys.host, &[&transcript], &mac) {
        channel.abort(Reason::Failed).await;
        bail!(WRONG_CODE);
    }
    channel
        .write(&Frame::Confirm {
            mac: tag(&keys.joiner, &[&transcript]),
        })
        .await?;
    Ok(Session {
        channel,
        keys,
        transcript,
    })
}

/// The confirmation numbers of one session, seen from one machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Numbers {
    /// Shown here and read to the other person.
    pub(super) mine: String,
    /// Shown on the other machine; the person here must type it.
    pub(super) theirs: String,
}

/// Shows this machine's number, asks for the other machine's, and resolves to
/// whether it was typed correctly.
pub(super) type Confirm =
    Box<dyn FnOnce(Numbers) -> Pin<Box<dyn Future<Output = Result<bool>> + Send>> + Send>;

fn random_nonce() -> Result<[u8; 32]> {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("system randomness unavailable: {error}"))?;
    Ok(nonce)
}

fn commitment(nonce: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(b"iroh-toolbox/pairing/v1/commit")
        .chain_update(nonce)
        .finalize()
        .into()
}

/// The host's and the joiner's numbers: two independent 6-digit values from
/// one MAC, so that each machine checks a full number typed by its person.
fn confirmation_numbers(
    sas: &[u8; 32],
    transcript: &[u8; 32],
    host_nonce: &[u8],
    joiner_nonce: &[u8],
) -> (String, String) {
    let digest = mac(sas, &[transcript, host_nonce, joiner_nonce])
        .finalize()
        .into_bytes();
    let number = |bytes: &[u8]| {
        let value = u64::from_be_bytes(bytes.try_into().expect("8 bytes")) % 1_000_000;
        format!("{:03} {:03}", value / 1000, value % 1000)
    };
    (number(&digest[..8]), number(&digest[8..16]))
}

/// Short authentication string with a commitment, so that an attacker who knows
/// the code cannot choose values until both numbers match.
async fn confirm_number(session: &mut Session, role: Role, confirm: Confirm) -> Result<()> {
    let (host_nonce, joiner_nonce) = match role {
        Role::Host => {
            let nonce = random_nonce()?;
            session
                .channel
                .write(&Frame::Commit {
                    commitment: hex::encode(commitment(&nonce)),
                })
                .await?;
            let Frame::Nonce { nonce: peer } = session.channel.read().await? else {
                bail!("invalid pairing message");
            };
            let peer = decode(&peer, 32)?;
            session
                .channel
                .write(&Frame::Reveal {
                    nonce: hex::encode(nonce),
                })
                .await?;
            (nonce.to_vec(), peer)
        }
        Role::Joiner => {
            let Frame::Commit { commitment: value } = session.channel.read().await? else {
                bail!("invalid pairing message");
            };
            let committed = decode(&value, 32)?;
            let nonce = random_nonce()?;
            session
                .channel
                .write(&Frame::Nonce {
                    nonce: hex::encode(nonce),
                })
                .await?;
            let Frame::Reveal { nonce: peer } = session.channel.read().await? else {
                bail!("invalid pairing message");
            };
            let peer = decode(&peer, 32)?;
            if commitment(&peer)[..] != committed[..] {
                session.channel.abort(Reason::Failed).await;
                bail!("pairing failed: the other machine changed its confirmation value");
            }
            (peer, nonce.to_vec())
        }
    };
    let (host, joiner) = confirmation_numbers(
        &session.keys.sas,
        &session.transcript,
        &host_nonce,
        &joiner_nonce,
    );
    let numbers = match role {
        Role::Host => Numbers {
            mine: host,
            theirs: joiner,
        },
        Role::Joiner => Numbers {
            mine: joiner,
            theirs: host,
        },
    };
    let mut answer = confirm(numbers);
    match role {
        Role::Host => {
            if !answer.await? {
                session.channel.abort(Reason::Declined).await;
                bail!("confirmation numbers not confirmed; create a new code");
            }
            session.channel.write(&Frame::Ready {}).await?;
            ensure!(
                matches!(session.channel.read().await?, Frame::Ready {}),
                "invalid pairing message"
            );
        }
        Role::Joiner => {
            // The host confirms first; the joiner may answer before or after it.
            let mut host_ready = false;
            let accepted = loop {
                tokio::select! {
                    accepted = &mut answer => break accepted?,
                    frame = session.channel.read(), if !host_ready => {
                        ensure!(matches!(frame?, Frame::Ready {}), "invalid pairing message");
                        host_ready = true;
                    }
                }
            };
            if !accepted {
                session.channel.abort(Reason::Declined).await;
                bail!("confirmation numbers not confirmed; create a new code");
            }
            if !host_ready {
                ensure!(
                    matches!(session.channel.read().await?, Frame::Ready {}),
                    "invalid pairing message"
                );
            }
            session.channel.write(&Frame::Ready {}).await?;
        }
    }
    Ok(())
}

/// The endpoint ID a joiner announces: lowercase hex of the 32-byte key, and a
/// valid public key. Nothing is echoed on failure.
pub(super) fn encode_endpoint(id: EndpointId) -> String {
    hex::encode(id.as_bytes())
}

pub(super) fn decode_endpoint(value: &str) -> Result<EndpointId> {
    let invalid = || anyhow::anyhow!("invalid announced endpoint");
    let bytes: [u8; 32] = hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(invalid)?;
    // Canonical form only: one text per key.
    ensure!(hex::encode(bytes) == value, invalid());
    EndpointId::from_bytes(&bytes).map_err(|_| invalid())
}

/// Host: deliver `payload`, read the joiner's announced transfer endpoint, then
/// exchange transcript MACs. The endpoint is returned only once the joiner's
/// `done` MAC covers it.
async fn host_finish(session: &mut Session, payload: String) -> Result<EndpointId> {
    session
        .channel
        .write(&Frame::Payload(Body::Ticket { ticket: payload }))
        .await?;
    let Frame::Payload(Body::Endpoint { endpoint }) = session.channel.read().await? else {
        bail!("invalid pairing message");
    };
    // An invalid key ends the pairing before anything is signed or served.
    let announced = decode_endpoint(&endpoint)?;
    let transcript = session.channel.digest();
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
        "pairing failed: the other machine did not confirm the ticket"
    );
    Ok(announced)
}

/// Joiner: receive the host's payload, announce `transfer` and confirm both.
async fn join_finish(session: &mut Session, transfer: EndpointId) -> Result<String> {
    let Frame::Payload(Body::Ticket { ticket: payload }) = session.channel.read().await? else {
        bail!("invalid pairing message");
    };
    session
        .channel
        .write(&Frame::Payload(Body::Endpoint {
            endpoint: encode_endpoint(transfer),
        }))
        .await?;
    let transcript = session.channel.digest();
    let Frame::Done { mac } = session.channel.read().await? else {
        bail!("invalid pairing message");
    };
    ensure!(
        check_tag(&session.keys.done, &[b"host", &transcript], &mac),
        "pairing failed: the ticket was not confirmed"
    );
    session
        .channel
        .write(&Frame::Done {
            mac: tag(&session.keys.done, &[b"joiner", &transcript]),
        })
        .await?;
    session.channel.send.finish()?;
    Ok(payload)
}

/// Host handshake and, when required, the confirmation number.
pub(super) async fn host_session(
    conn: &Connection,
    local: EndpointId,
    channel: Channel,
    hello: Frame,
    terms: Terms<'_>,
    confirm: Confirm,
) -> Result<Session> {
    let ids = (local, conn.remote_id());
    let mut session = host_handshake(channel, hello, terms, ids).await?;
    if terms.verify {
        confirm_number(&mut session, Role::Host, confirm).await?;
    }
    Ok(session)
}

/// Joiner handshake and, when required, the confirmation number.
pub(super) async fn join_session(
    conn: &Connection,
    local: EndpointId,
    terms: Terms<'_>,
    confirm: Confirm,
) -> Result<Session> {
    let (send, recv) = conn.open_bi().await?;
    let ids = (conn.remote_id(), local);
    let mut session = join_handshake(Channel::new(conn, send, recv), terms, ids).await?;
    if terms.verify {
        confirm_number(&mut session, Role::Joiner, confirm).await?;
    }
    Ok(session)
}

/// Runs the host role on an accepted pairing connection and returns the
/// transfer endpoint the joiner announced.
pub(super) async fn run_host(
    conn: &Connection,
    local: EndpointId,
    channel: Channel,
    hello: Frame,
    terms: Terms<'_>,
    payload: String,
    confirm: Confirm,
) -> Result<EndpointId> {
    let mut session = host_session(conn, local, channel, hello, terms, confirm).await?;
    host_finish(&mut session, payload).await
}

/// Runs the joiner role on an open pairing connection and returns the host's
/// payload. `local` is the pairing endpoint's identity, bound into SPAKE2;
/// `transfer` is the endpoint the host will serve.
pub(super) async fn run_joiner(
    conn: &Connection,
    local: EndpointId,
    transfer: EndpointId,
    terms: Terms<'_>,
    confirm: Confirm,
) -> Result<String> {
    let mut session = join_session(conn, local, terms, confirm).await?;
    join_finish(&mut session, transfer).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_exchange_rejects_identity_and_torsion_points() {
        let (_, honest) = Spake2::<Ed25519Group>::start_b(
            &Password::new(b"pw"),
            &Identity::new(b"a"),
            &Identity::new(b"b"),
        );
        assert!(check_element(&honest, b'B').is_ok());
        assert!(check_element(&honest, b'A').is_err());
        let mut identity = vec![b'B'];
        identity.extend_from_slice(
            curve25519_dalek::EdwardsPoint::default()
                .compress()
                .as_bytes(),
        );
        assert!(check_element(&identity, b'B').is_err());
        let point = CompressedEdwardsY(honest[1..].try_into().unwrap())
            .decompress()
            .unwrap()
            + curve25519_dalek::constants::EIGHT_TORSION[1];
        let mut mixed = vec![b'B'];
        mixed.extend_from_slice(point.compress().as_bytes());
        assert!(check_element(&mixed, b'B').is_err());
    }

    #[test]
    fn pair_without_confirmation_number_gets_a_pair_specific_message() {
        let error = check_hello(VERSION, "pair", false, Purpose::Pair, true).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("confirmation number"), "{message}");
        assert!(!message.contains("--verify"), "{message}");
        assert!(!message.contains("no-verify"), "{message}");
    }

    fn hex_of(bytes: &[u8; 32]) -> String {
        hex::encode(bytes)
    }

    /// Frozen vector: a change to an expected value is a wire-format change
    /// (two machines would derive different keys from the same exchange).
    #[test]
    fn key_derivation_matches_the_frozen_vector() {
        let keys = Keys::derive(&[0x11; 32], &[0x22; 32]);
        assert_eq!(
            hex_of(&keys.host),
            "c7e7d94d5ef1d3046c37300143d3a894bde24772dcfc3ea655364cecc5b35789"
        );
        assert_eq!(
            hex_of(&keys.joiner),
            "729e87382972d67dcb93d5b588b02be7e59b7cc8c411c50c72b4f47a42b3af94"
        );
        assert_eq!(
            hex_of(&keys.sas),
            "72e543fba035a0571ad9e4fee0e5e76460eda468e4b11f8aad42089c1b707911"
        );
        assert_eq!(
            hex_of(&keys.done),
            "6312e9a43f033e7e5fd76cadb8e8149f4b40be76880121438ac71f3640650c10"
        );
    }

    /// Frozen vector: a change to an expected value is a wire-format change.
    #[test]
    fn commitment_matches_the_frozen_vector() {
        assert_eq!(
            hex_of(&commitment(&[0x33; 32])),
            "c5c8b4df7cc47041d3052334d2ac0fa3a02c909fc758992f961d99c1429637a9"
        );
        assert_eq!(
            hex_of(&commitment(&[])),
            "dfcb912230b6e7979c067a4badc9081028c8b751b2aac893f1aa19fea71bd9b0"
        );
    }

    /// Frozen vector: a change to an expected value is a wire-format change
    /// (the two people would read different numbers).
    #[test]
    fn confirmation_numbers_match_the_frozen_vector() {
        let keys = Keys::derive(&[0x11; 32], &[0x22; 32]);
        let numbers = |host: u8, joiner: u8| {
            confirmation_numbers(&keys.sas, &[0x22; 32], &[host; 32], &[joiner; 32])
        };
        assert_eq!(numbers(0x44, 0x55), ("547 794".into(), "184 356".into()));
        assert_eq!(numbers(0x55, 0x44), ("339 423".into(), "448 177".into()));
        assert_eq!(numbers(0x00, 0xff), ("165 076".into(), "597 104".into()));
    }
}
