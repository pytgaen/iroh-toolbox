//! Frames, the closed set of pairing messages, and the channel that carries
//! them while hashing the transcript.
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

const FRAME_LIMIT: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Frame {
    Hello {
        version: u32,
        purpose: String,
        verify: bool,
        pake: String,
    },
    Confirm {
        mac: String,
    },
    Commit {
        commitment: String,
    },
    Nonce {
        nonce: String,
    },
    Reveal {
        nonce: String,
    },
    Ready {},
    Payload(Body),
    Done {
        mac: String,
    },
    Abort {
        reason: Reason,
    },
}

/// What a `payload` frame carries: the host's ticket (files, chat), the
/// joiner's transfer endpoint (files, chat) or a machine record (pair). Each
/// form has its own required field and `deny_unknown_fields` closes it, so a
/// frame matches exactly one form.
#[derive(Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(super) enum Body {
    Peer {
        peer: PeerRecord,
    },
    Ticket {
        ticket: String,
    },
    /// The endpoint ID the joiner will transfer from, lowercase hex.
    Endpoint {
        endpoint: String,
    },
}

/// A machine presenting itself: key, address salt and announced name, signed
/// with the machine key over the session transcript. Fields are text so that
/// the frame stays JSON; `deny_unknown_fields` keeps the record closed.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PeerRecord {
    pub(super) machine: String,
    pub(super) salt: String,
    pub(super) name: String,
    pub(super) signature: String,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reason {
    Mismatch,
    Failed,
    Declined,
    NameTaken,
}

impl Reason {
    /// QUIC application close codes: the reason survives even when the
    /// connection closes before the Abort frame is read.
    fn code(self) -> u32 {
        match self {
            Self::Mismatch => 10,
            Self::Failed => 11,
            Self::Declined => 12,
            Self::NameTaken => 13,
        }
    }

    fn from_code(code: u64) -> Option<Self> {
        [
            Self::Mismatch,
            Self::Failed,
            Self::Declined,
            Self::NameTaken,
        ]
        .into_iter()
        .find(|reason| u64::from(reason.code()) == code)
    }

    fn message(self) -> &'static str {
        match self {
            Self::Mismatch => {
                "the other machine uses a different command or --no-verify setting; run matching commands with a new code"
            }
            Self::Failed => WRONG_CODE,
            Self::Declined => {
                "the other machine did not confirm the confirmation numbers; create a new code"
            }
            Self::NameTaken => {
                "the other machine has no free name for this one; ask it to pair again with --name ALIAS"
            }
        }
    }
}

pub(super) struct Channel {
    conn: Connection,
    pub(super) send: SendStream,
    recv: RecvStream,
    transcript: Sha256,
}

/// Decodes one frame body. Do not echo untrusted bytes, tickets or keys in errors.
pub(super) fn parse_frame(bytes: &[u8]) -> Result<Frame> {
    serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("invalid pairing message"))
}

impl Channel {
    pub(super) fn new(conn: &Connection, send: SendStream, recv: RecvStream) -> Self {
        Self {
            conn: conn.clone(),
            send,
            recv,
            transcript: Sha256::new(),
        }
    }

    /// Explains a closed connection by the peer's abort reason, if it sent one.
    fn closed(&self, error: impl Into<anyhow::Error>) -> anyhow::Error {
        if let Some(ConnectionError::ApplicationClosed(close)) = self.conn.close_reason()
            && let Some(reason) = Reason::from_code(close.error_code.into())
        {
            return anyhow::anyhow!(reason.message());
        }
        error.into().context("pairing connection closed")
    }

    /// Fails with the peer's reason when it already closed the connection.
    pub(super) fn check_open(&self) -> Result<()> {
        if self.conn.close_reason().is_some() {
            return Err(self.closed(anyhow::anyhow!("the other machine ended the pairing")));
        }
        Ok(())
    }

    fn record(&mut self, bytes: &[u8]) {
        self.transcript.update((bytes.len() as u32).to_be_bytes());
        self.transcript.update(bytes);
    }

    pub(super) fn digest(&self) -> [u8; 32] {
        self.transcript.clone().finalize().into()
    }

    pub(super) async fn write(&mut self, frame: &Frame) -> Result<()> {
        let bytes = serde_json::to_vec(frame)?;
        ensure!(bytes.len() <= FRAME_LIMIT, "pairing message exceeds limit");
        self.record(&bytes);
        // The other machine may have aborted while this one was busy.
        if let Err(error) = self.send.write_u16(bytes.len() as u16).await {
            return Err(self.closed(error));
        }
        if let Err(error) = self.send.write_all(&bytes).await {
            return Err(self.closed(error));
        }
        Ok(())
    }

    pub(super) async fn read(&mut self) -> Result<Frame> {
        let size = match self.recv.read_u16().await {
            Ok(size) => usize::from(size),
            Err(error) => return Err(self.closed(error)),
        };
        ensure!(size <= FRAME_LIMIT, "pairing message exceeds limit");
        let mut bytes = vec![0; size];
        if let Err(error) = self.recv.read_exact(&mut bytes).await {
            return Err(self.closed(error));
        }
        let frame = parse_frame(&bytes)?;
        self.record(&bytes);
        if let Frame::Abort { reason } = frame {
            bail!(reason.message());
        }
        Ok(frame)
    }

    pub(super) async fn abort(&mut self, reason: Reason) {
        let _ = timeout(
            Duration::from_millis(500),
            self.write(&Frame::Abort { reason }),
        )
        .await;
        self.conn.close(reason.code().into(), b"pairing aborted");
    }
}

pub(super) const WRONG_CODE: &str =
    "pairing failed: wrong code, or someone else tried to use it; create a new code";

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    #[test]
    fn payload_frames_stay_closed_and_match_one_form() {
        let ticket = serde_json::to_string(&Frame::Payload(Body::Ticket {
            ticket: "blob".into(),
        }))
        .unwrap();
        assert_eq!(ticket, r#"{"type":"payload","ticket":"blob"}"#);
        let endpoint = format!(r#"{{"type":"payload","endpoint":"{ID}"}}"#);
        let written = serde_json::to_string(&Frame::Payload(Body::Endpoint {
            endpoint: ID.into(),
        }))
        .unwrap();
        assert_eq!(written, endpoint);
        // Each form round-trips to its own variant.
        assert!(matches!(
            serde_json::from_str::<Frame>(&ticket).unwrap(),
            Frame::Payload(Body::Ticket { ticket }) if ticket == "blob"
        ));
        assert!(matches!(
            serde_json::from_str::<Frame>(&endpoint).unwrap(),
            Frame::Payload(Body::Endpoint { endpoint }) if endpoint == ID
        ));
        // `Ready {}` keeps its wire form and rejects unknown fields.
        assert_eq!(
            serde_json::to_string(&Frame::Ready {}).unwrap(),
            r#"{"type":"ready"}"#
        );
        assert!(matches!(
            serde_json::from_str::<Frame>(r#"{"type":"ready"}"#).unwrap(),
            Frame::Ready {}
        ));
        assert!(serde_json::from_str::<Frame>(r#"{"type":"ready","x":1}"#).is_err());
        let record = r#"{"machine":"a","salt":"b","name":"c","signature":"d"}"#;
        let peer = format!(r#"{{"type":"payload","peer":{record}}}"#);
        assert!(matches!(
            serde_json::from_str::<Frame>(&peer).unwrap(),
            Frame::Payload(Body::Peer { .. })
        ));
        let extra = r#"{"type":"payload","peer":{"machine":"a","salt":"b","name":"c","signature":"d","x":1}}"#;
        // Refused: a record with an unknown field, an empty payload, the old
        // `ticket: null`, unknown fields beside any form, and any two forms.
        for refused in [
            extra.to_owned(),
            r#"{"type":"payload"}"#.to_owned(),
            r#"{"type":"payload","ticket":null}"#.to_owned(),
            format!(r#"{{"type":"payload","endpoint":"{ID}","x":1}}"#),
            r#"{"type":"payload","ticket":"blob","x":1}"#.to_owned(),
            format!(r#"{{"type":"payload","peer":{record},"x":1}}"#),
            format!(r#"{{"type":"payload","ticket":"blob","endpoint":"{ID}"}}"#),
            format!(r#"{{"type":"payload","endpoint":"{ID}","peer":{record}}}"#),
            format!(r#"{{"type":"payload","ticket":"blob","peer":{record}}}"#),
            format!(r#"{{"type":"payload","ticket":"blob","endpoint":"{ID}","peer":{record}}}"#),
            r#"{"type":"payload","endpoint":null}"#.to_owned(),
        ] {
            assert!(
                serde_json::from_str::<Frame>(&refused).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn pairing_frame_parser_survives_mutated_and_random_input() {
        let endpoint = format!(r#"{{"type":"payload","endpoint":"{ID}"}}"#);
        let peer =
            r#"{"type":"payload","peer":{"machine":"a","salt":"b","name":"c","signature":"d"}}"#;
        crate::fuzz::check_decoder(
            &[
                r#"{"type":"hello","version":1,"purpose":"files","verify":true,"pake":"00"}"#,
                r#"{"type":"confirm","mac":"00"}"#,
                r#"{"type":"commit","commitment":"00"}"#,
                r#"{"type":"nonce","nonce":"00"}"#,
                r#"{"type":"reveal","nonce":"00"}"#,
                r#"{"type":"ready"}"#,
                r#"{"type":"payload","ticket":"blob"}"#,
                endpoint.as_str(),
                peer,
                r#"{"type":"done","mac":"00"}"#,
                r#"{"type":"abort","reason":"mismatch"}"#,
            ],
            &[
                ("hello", &["version", "purpose", "verify", "pake"]),
                ("confirm", &["mac"]),
                ("commit", &["commitment"]),
                ("nonce", &["nonce"]),
                ("reveal", &["nonce"]),
                ("ready", &[]),
                ("payload", &["ticket", "endpoint", "peer"]),
                ("done", &["mac"]),
                ("abort", &["reason"]),
            ],
            parse_frame,
        );
    }
}
