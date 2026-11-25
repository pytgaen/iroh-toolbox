//! Why a pairing finds nobody: the reason of each failed attempt on the
//! joining side, and whether the creator's rendezvous record is readable.
//!
//! iroh logs these causes without returning them, so both machines used to
//! wait in silence (for example when their builds derive different
//! rendezvous endpoints). Nothing here touches the protocol, and nothing
//! prints a code, a password word or a relay token.
use std::{convert::Infallible, time::Duration};

use iroh::{
    Endpoint,
    address_lookup::AddressLookupFailed,
    endpoint::{ConnectError, ConnectWithOptsError},
};
use tokio::time::{Instant, sleep, sleep_until};

use crate::takeover::{check, dns_lookup};

/// A failed attempt is repeated unchanged only every this many attempts.
const REPEAT_EVERY: u32 = 10;
/// How long a code may stay unreadable on DNS before the creator is warned.
const WARN_AFTER: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_secs(2);

/// One line: lookup errors contain line breaks and indentation.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn timeout_reason(limit: Duration) -> String {
    format!("no answer within {}s", limit.as_secs())
}

/// iroh found no address for the endpoint; `lookups` are the errors of the
/// address lookup services, which tell "record not found" from "DNS unreachable".
fn no_address_reason(lookups: &[String]) -> String {
    let base = "no address published for this endpoint";
    if lookups.is_empty() {
        base.to_owned()
    } else {
        format!("{base} ({})", lookups.join("; "))
    }
}

/// The cause of a failed connection attempt, on one line, before redaction.
pub(super) fn failure_reason(error: &ConnectError) -> String {
    let reason = match error {
        ConnectError::Connect {
            source: ConnectWithOptsError::NoAddress { source, .. },
            ..
        } => {
            let lookups: Vec<String> = match source {
                AddressLookupFailed::NoResults { errors, .. } => {
                    errors.iter().map(|error| format!("{error:#}")).collect()
                }
                _ => Vec::new(),
            };
            no_address_reason(
                &lookups
                    .iter()
                    .map(|text| one_line(text))
                    .collect::<Vec<_>>(),
            )
        }
        other => format!("{other:#}"),
    };
    one_line(&reason)
}

/// Failed attempts of one search, and which of them to print.
#[derive(Default)]
pub(super) struct Attempts {
    count: u32,
    last: Option<String>,
}

impl Attempts {
    /// Records a failure; the line to print when its reason is new or on every
    /// tenth attempt, so a search that repeats each second does not flood.
    pub(super) fn record(&mut self, reason: String) -> Option<String> {
        self.count += 1;
        let changed = self.last.as_deref() != Some(reason.as_str());
        let line = (changed || self.count.is_multiple_of(REPEAT_EVERY))
            .then(|| format!("pairing: attempt {} failed: {reason}; retrying", self.count));
        self.last = Some(reason);
        line
    }

    pub(super) fn last(&self) -> Option<&str> {
        self.last.as_deref()
    }
}

pub(super) fn timed_out_message(last: Option<&str>) -> String {
    let mut message = "pairing timed out: no machine answered with this code; check the code, that the other command is still running, and that both machines run the same build (compare the Commit line of `iroh-toolbox --version`)".to_owned();
    if let Some(reason) = last {
        message.push_str(&format!(" (last attempt: {reason})"));
    }
    message
}

/// What the creator learns from reading its rendezvous record back.
#[derive(Debug, PartialEq, Eq)]
enum Event {
    Published,
    PublishedLate,
    NotVisible,
    Differs,
}

#[derive(Default)]
struct Readback {
    warned: bool,
    differs: bool,
}

impl Readback {
    /// A read-back: `Some(true)` is this endpoint's record, `Some(false)` another
    /// one, `None` unreadable. The bool tells that polling is over.
    fn verdict(&mut self, verdict: Option<bool>) -> (Option<Event>, bool) {
        match verdict {
            Some(true) => {
                let event = if self.warned {
                    Event::PublishedLate
                } else {
                    Event::Published
                };
                (Some(event), true)
            }
            Some(false) => {
                let first = !std::mem::replace(&mut self.differs, true);
                (first.then_some(Event::Differs), false)
            }
            None => {
                self.differs = false;
                (None, false)
            }
        }
    }

    /// The warning delay elapsed without a readable record.
    fn overdue(&mut self) -> Option<Event> {
        (!std::mem::replace(&mut self.warned, true)).then_some(Event::NotVisible)
    }
}

fn show(event: Option<Event>) {
    match event {
        Some(Event::Published) => detail!("pairing: rendezvous address published on dns.iroh.link"),
        Some(Event::PublishedLate) => note!("note: this code's address is now published"),
        Some(Event::NotVisible) => note!(
            "warning: this code's address is not visible on dns.iroh.link; the other machine cannot find this machine until it is (check that this machine can reach https://dns.iroh.link)"
        ),
        Some(Event::Differs) => {
            detail!("pairing: the rendezvous address record is not this machine's; checking again")
        }
        None => {}
    }
}

/// Reads the rendezvous record of `ep` back from n0's DNS until it is there,
/// and warns once if that takes too long. Never ends by itself: race it
/// against the accept, so that it stops with it.
pub(super) async fn watch_publication(ep: &Endpoint) -> Infallible {
    if let Some(lookup) = dns_lookup(ep) {
        let warn_at = Instant::now() + WARN_AFTER;
        let mut state = Readback::default();
        loop {
            tokio::select! {
                verdict = check(&lookup, ep) => {
                    let (event, done) = state.verdict(verdict);
                    show(event);
                    if done {
                        break;
                    }
                }
                () = sleep_until(warn_at), if !state.warned => show(state.overdue()),
            }
            tokio::select! {
                () = sleep(POLL) => {}
                () = sleep_until(warn_at), if !state.warned => show(state.overdue()),
            }
        }
    }
    std::future::pending().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    #[test]
    fn reasons_are_one_line_and_say_what_failed() {
        assert_eq!(
            timeout_reason(Duration::from_secs(30)),
            "no answer within 30s"
        );
        assert_eq!(
            no_address_reason(&[]),
            "no address published for this endpoint"
        );
        assert_eq!(
            no_address_reason(&["Service 'dns' failed: not found".into()]),
            "no address published for this endpoint (Service 'dns' failed: not found)"
        );
        assert_eq!(
            no_address_reason(&["a".into(), "b".into()]),
            "no address published for this endpoint (a; b)"
        );
        assert_eq!(
            one_line("All services failed\n    Service 'dns':\n\t  timeout "),
            "All services failed Service 'dns': timeout"
        );
    }

    #[test]
    fn reasons_are_redacted_by_the_settings() {
        let settings = Settings {
            relay: None,
            token: Some("s3cret".into()),
            timeout: Duration::from_secs(30),
            relay_only: false,
            direct_only: false,
            no_hints: false,
        };
        assert_eq!(
            settings.redact(one_line("relay said\n s3cret rejected")),
            "relay said [REDACTED] rejected"
        );
    }

    #[test]
    fn an_attempt_is_printed_when_its_reason_changes_else_every_tenth() {
        let mut attempts = Attempts::default();
        assert_eq!(
            attempts.record("a".into()).as_deref(),
            Some("pairing: attempt 1 failed: a; retrying")
        );
        for number in 2..=9 {
            assert_eq!(attempts.record("a".into()), None, "attempt {number}");
        }
        assert_eq!(
            attempts.record("a".into()).as_deref(),
            Some("pairing: attempt 10 failed: a; retrying")
        );
        assert_eq!(
            attempts.record("b".into()).as_deref(),
            Some("pairing: attempt 11 failed: b; retrying")
        );
        assert_eq!(attempts.record("b".into()), None);
        assert_eq!(attempts.last(), Some("b"));
    }

    #[test]
    fn the_timeout_message_keeps_its_markers_and_adds_the_last_reason() {
        let plain = timed_out_message(None);
        assert!(plain.starts_with("pairing timed out: no machine answered with this code"));
        assert!(plain.contains("Commit line of `iroh-toolbox --version`"));
        assert!(!plain.contains("last attempt"));
        let detailed = timed_out_message(Some("no answer within 30s"));
        assert!(detailed.starts_with(&plain));
        assert!(detailed.ends_with(" (last attempt: no answer within 30s)"));
    }

    #[test]
    fn the_creator_is_warned_once_and_told_when_the_record_appears() {
        let mut state = Readback::default();
        assert_eq!(state.verdict(None), (None, false));
        assert_eq!(state.verdict(Some(false)), (Some(Event::Differs), false));
        assert_eq!(state.verdict(Some(false)), (None, false));
        assert_eq!(state.overdue(), Some(Event::NotVisible));
        assert_eq!(state.overdue(), None);
        assert_eq!(
            state.verdict(Some(true)),
            (Some(Event::PublishedLate), true)
        );

        let mut quick = Readback::default();
        assert_eq!(quick.verdict(Some(true)), (Some(Event::Published), true));
    }
}
