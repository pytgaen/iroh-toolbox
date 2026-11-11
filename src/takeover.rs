//! Address takeover warning (notes/peers-spec.md, "Address takeover warning").
//!
//! Every holder of a machine's address salt can bind a slot's endpoint key. A
//! relay delivers a key's traffic to its newest registration only, and n0's DNS
//! keeps the newest record signed by that key, so another holder can take a
//! running slot's traffic. A server cannot prevent it; it tells the user:
//!
//! - **At the relay.** The relay tells the registration it displaces
//!   `SameEndpointIdConnected`, then `Healthy` once it is active again. iroh
//!   1.4.1 only logs both, so a `tracing` layer catches them and finds the slot
//!   through the span its endpoint was bound in: iroh parents every task of an
//!   endpoint to the span active at bind time.
//! - **In DNS** (public relays): the server reads its own record back and
//!   compares it with what it publishes.
//!
//! Only the slot name is printed: never a key, address, relay URL, record or
//! remote text.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex, Once, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use iroh::{
    Endpoint, EndpointAddr, Watcher,
    address_lookup::{
        AddrFilter, AddressLookup, AddressLookupBuilder, EndpointData, EndpointInfo, PkarrResolver,
        pkarr::{DEFAULT_PKARR_TTL, DEFAULT_REPUBLISH_INTERVAL},
    },
};
use n0_future::StreamExt;
use tracing::{
    Event, Level, Metadata, Span, Subscriber,
    field::{Field, Visit},
    level_filters::LevelFilter,
    span,
    subscriber::Interest,
};
use tracing_subscriber::{
    Layer,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
};

/// The span a slot endpoint is bound in; its `watch` field names the [`Watch`].
const SPAN: &str = "slot-endpoint";
/// iroh's relay transport: its spans lead from the endpoint to the relay
/// connection, its events carry the relay's status.
const RELAY_TARGET: &str = "iroh::socket::transports::relay";
/// How iroh-relay 1.4.1 words the statuses that iroh logs.
const DISPLACED: &str = "Another endpoint connected with the same endpoint id";
const HEALTHY: &str = "The connection is healthy";

/// A slot record that differs is checked again one TTL later before the
/// warning, so that a change of home relay is not taken for a takeover.
const CONFIRM: Duration = Duration::from_secs(DEFAULT_PKARR_TTL as u64 + 5);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

static WATCHES: Mutex<BTreeMap<u64, Weak<Watch>>> = Mutex::new(BTreeMap::new());
static NEXT: AtomicU64 = AtomicU64::new(1);

/// What a running slot server reports about its address. Dropping it (with
/// the server) silences it: an exiting process says nothing.
pub struct Watch {
    id: u64,
    slot: String,
    /// Another registration of the slot key is active at the relay.
    displaced: AtomicBool,
    /// The slot's DNS record is not the one this server publishes.
    replaced: AtomicBool,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Watch {
    pub fn new(slot: &str) -> Arc<Self> {
        install();
        let watch = Arc::new(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            slot: slot.to_owned(),
            displaced: AtomicBool::new(false),
            replaced: AtomicBool::new(false),
            task: Mutex::new(None),
        });
        WATCHES
            .lock()
            .unwrap()
            .insert(watch.id, Arc::downgrade(&watch));
        watch
    }

    /// The span to bind the slot's endpoint in.
    pub fn span(&self) -> Span {
        tracing::info_span!("slot-endpoint", watch = self.id)
    }

    /// Follows the bound endpoint: relay reconnections, and with `dns` (public
    /// relays) its DNS record.
    pub fn monitor(self: &Arc<Self>, ep: &Endpoint, dns: bool) {
        let (ep, watch) = (ep.clone(), Arc::downgrade(self));
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = ep.closed() => {}
                _ = reconnections(&ep, &watch) => {}
                _ = records(&ep, &watch), if dns => {}
            }
        });
        *self.task.lock().unwrap() = Some(task);
    }

    fn relay_displaced(&self) {
        if !self.displaced.swap(true, Ordering::Relaxed) {
            note!(
                "warning: another machine has taken slot {}'s address at the relay; paired machines cannot reach this slot through it",
                self.slot
            );
            remedy();
        }
    }

    fn relay_restored(&self) {
        if self.displaced.swap(false, Ordering::Relaxed) {
            note!(
                "note: slot {} is reachable through the relay again",
                self.slot
            );
        }
    }

    fn record_replaced(&self) {
        if !self.replaced.swap(true, Ordering::Relaxed) {
            note!(
                "warning: another machine has replaced slot {}'s address record; paired machines may reach it instead of this server",
                self.slot
            );
            remedy();
        }
    }

    fn record_restored(&self) {
        if self.replaced.swap(false, Ordering::Relaxed) {
            note!(
                "note: slot {}'s address record points to this server again",
                self.slot
            );
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        WATCHES.lock().unwrap().remove(&self.id);
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}

/// The server cannot know who holds the address: current peers know it too.
fn remedy() {
    note!(
        "  It holds this machine's address: a paired, removed or lost machine, or a copy of this machine's identity file."
    );
    note!(
        "  If you removed a machine, run `iroh-toolbox id rotate --address`, then restart this server; `--direct-only` servers are not affected."
    );
}

fn watch(id: u64) -> Option<Arc<Watch>> {
    WATCHES.lock().unwrap().get(&id).and_then(Weak::upgrade)
}

/// Installs the layer once per process; nothing else of iroh's logs is kept.
fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(RelayStatus);
        // Nothing else installs a subscriber; if something did, it wins.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Reconnecting makes this registration the newest again: the slot is back.
async fn reconnections(ep: &Endpoint, watch: &Weak<Watch>) {
    let mut statuses = ep.home_relay_status();
    let connected = |relays: &[iroh::endpoint::RelayStatus]| {
        relays.iter().any(iroh::endpoint::RelayStatus::is_connected)
    };
    let mut was = connected(&statuses.get());
    let mut statuses = statuses.stream();
    while let Some(relays) = statuses.next().await {
        let now = connected(&relays);
        if now
            && !was
            && let Some(watch) = watch.upgrade()
        {
            watch.relay_restored();
        }
        was = now;
    }
}

/// Reads the slot's DNS record back once the endpoint is online, then at
/// iroh's republication interval.
async fn records(ep: &Endpoint, watch: &Weak<Watch>) {
    let Ok(lookup) = PkarrResolver::n0_dns().into_address_lookup(ep) else {
        return;
    };
    let mut differs = false;
    let mut delay = CONFIRM;
    loop {
        tokio::time::sleep(delay).await;
        delay = DEFAULT_REPUBLISH_INTERVAL;
        let verdict = check(&lookup, ep).await;
        let Some(watch) = watch.upgrade() else {
            return;
        };
        let slot = &watch.slot;
        match verdict {
            Some(true) => {
                detail!("slot {slot}: its address record is this server's");
                differs = false;
                watch.record_restored();
            }
            Some(false) if differs => watch.record_replaced(),
            Some(false) => {
                detail!("slot {slot}: its address record differs; checking again");
                differs = true;
                delay = CONFIRM;
            }
            None => detail!("slot {slot}: cannot read its address record"),
        }
    }
}

/// Whether the slot's record is this endpoint's; `None` when it cannot tell
/// (no relay connection, no record, lookup failure).
async fn check(lookup: &impl AddressLookup, ep: &Endpoint) -> Option<bool> {
    let ours = ep.addr();
    // Without a relay connection, the record may lag behind.
    ours.relay_urls().next()?;
    let mut items = lookup.resolve(ep.id())?;
    let item = tokio::time::timeout(LOOKUP_TIMEOUT, items.next())
        .await
        .ok()??
        .ok()?;
    Some(publishes(&ours, item.endpoint_info()))
}

/// Whether `record` (verified under the slot key) is what an endpoint with the
/// address `ours` publishes: iroh's publisher keeps relay addresses only.
fn publishes(ours: &EndpointAddr, record: &EndpointInfo) -> bool {
    let data = EndpointData::from(ours.clone());
    let expected: BTreeSet<_> = data
        .apply_filter(&AddrFilter::relay_only())
        .addrs()
        .cloned()
        .collect();
    let published: BTreeSet<_> = record.addrs().cloned().collect();
    expected == published && record.user_data().is_none()
}

/// The `tracing` layer: keeps the spans from a slot's endpoint to its relay
/// connection, and reacts to the relay status events in them.
struct RelayStatus;

/// The callsites the layer needs; every other one stays disabled.
fn wanted(metadata: &Metadata<'_>) -> bool {
    let target = metadata.target();
    if metadata.is_span() {
        (target == module_path!() && metadata.name() == SPAN)
            || (target == "iroh::endpoint" && metadata.name() == "endpoint")
            || target.starts_with(RELAY_TARGET)
    } else {
        target.starts_with(RELAY_TARGET) && *metadata.level() <= Level::INFO
    }
}

/// The [`Watch`] of a slot span, kept in the span's extensions.
struct Watched(u64);

#[derive(Default)]
struct WatchField(Option<u64>);

impl Visit for WatchField {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "watch" {
            self.0 = Some(value);
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_owned();
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S> Layer<S> for RelayStatus
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        if wanted(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        wanted(metadata)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::INFO)
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let metadata = attrs.metadata();
        if metadata.target() != module_path!() || metadata.name() != SPAN {
            return;
        }
        let mut field = WatchField::default();
        attrs.record(&mut field);
        if let (Some(watch), Some(span)) = (field.0, ctx.span(id)) {
            span.extensions_mut().insert(Watched(watch));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if !event.metadata().target().starts_with(RELAY_TARGET) {
            return;
        }
        let mut message = Message::default();
        event.record(&mut message);
        let displaced = if message.0.contains(DISPLACED) {
            true
        } else if message.0.contains(HEALTHY) {
            false
        } else {
            return;
        };
        let Some(scope) = ctx.event_scope(event) else {
            return;
        };
        let Some(id) = scope
            .into_iter()
            .find_map(|span| span.extensions().get::<Watched>().map(|watched| watched.0))
        else {
            return;
        };
        if let Some(watch) = watch(id) {
            if displaced {
                watch.relay_displaced();
            } else {
                watch.relay_restored();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{RelayUrl, SecretKey, TransportAddr};

    const ACTOR: &str = "iroh::socket::transports::relay::actor";

    /// Logs `text` the way iroh's relay connection of `watch`'s endpoint does,
    /// through a span the layer ignores.
    fn relay_logs(watch: Option<&Watch>, text: &str) {
        let _slot = watch.map(|watch| watch.span().entered());
        let _endpoint = tracing::info_span!(target: "iroh::endpoint", "endpoint").entered();
        let _ignored = tracing::info_span!(target: "iroh::socket", "actor").entered();
        let _relay =
            tracing::info_span!(target: "iroh::socket::transports::relay", "relay-actor").entered();
        let _connection = tracing::info_span!(target: ACTOR, "active-relay").entered();
        if text.contains(HEALTHY) {
            tracing::info!(target: ACTOR, "Relay server reports: {text}");
        } else {
            tracing::warn!(target: ACTOR, "Relay server reports problem: {text}");
        }
    }

    fn with_layer(test: impl FnOnce()) {
        let subscriber = tracing_subscriber::registry().with(RelayStatus);
        tracing::subscriber::with_default(subscriber, test);
    }

    const DISPLACED_STATUS: &str =
        "Another endpoint connected with the same endpoint id. No more messages will be received.";
    const HEALTHY_STATUS: &str =
        "The connection is healthy and has recovered from previous problems";

    #[test]
    fn a_relay_status_reaches_the_slot_whose_endpoint_logged_it() {
        with_layer(|| {
            let (web, ssh) = (Watch::new("web"), Watch::new("ssh"));
            relay_logs(Some(&web), DISPLACED_STATUS);
            assert!(web.displaced.load(Ordering::Relaxed));
            assert!(!ssh.displaced.load(Ordering::Relaxed));
            relay_logs(Some(&web), HEALTHY_STATUS);
            assert!(!web.displaced.load(Ordering::Relaxed));
        });
    }

    #[test]
    fn other_logs_and_endpoints_change_nothing() {
        with_layer(|| {
            let web = Watch::new("web");
            // Another status, an endpoint that is not a slot, another target.
            relay_logs(
                Some(&web),
                "The relay is rate-limiting traffic received from this endpoint",
            );
            relay_logs(None, DISPLACED_STATUS);
            {
                let _slot = web.span().entered();
                tracing::warn!(target: "iroh::socket", "{DISPLACED_STATUS}");
            }
            assert!(!web.displaced.load(Ordering::Relaxed));
            // A dropped watch (an exiting server) is silent.
            let gone = Watch::new("gone");
            let span = gone.span();
            drop(gone);
            let _slot = span.entered();
            tracing::warn!(target: ACTOR, "Relay server reports problem: {DISPLACED_STATUS}");
        });
    }

    #[test]
    fn only_the_relay_chain_and_its_statuses_are_enabled() {
        with_layer(|| {
            let kept = [
                tracing::info_span!(target: "iroh::endpoint", "endpoint"),
                tracing::info_span!(target: "iroh::socket::transports::relay", "relay-actor"),
                tracing::info_span!(target: ACTOR, "active-relay"),
            ];
            assert!(kept.iter().all(|span| !span.is_disabled()));
            // A span on every packet, another span of the endpoint task.
            let dropped = [
                tracing::info_span!(target: "iroh::socket::transports", "poll_send"),
                tracing::info_span!(target: "iroh::endpoint", "connect"),
            ];
            assert!(dropped.iter().all(Span::is_disabled));
            assert!(tracing::event_enabled!(target: ACTOR, Level::INFO));
            assert!(!tracing::event_enabled!(target: ACTOR, Level::DEBUG));
            assert!(!tracing::event_enabled!(target: "iroh::endpoint", Level::WARN));
        });
    }

    fn url(text: &str) -> RelayUrl {
        text.parse().unwrap()
    }

    /// A record as n0's DNS returns it: signed by the slot key, then parsed.
    fn signed(key: &SecretKey, info: EndpointInfo) -> EndpointInfo {
        let packet = info.to_pkarr_signed_packet(key, DEFAULT_PKARR_TTL).unwrap();
        EndpointInfo::from_pkarr_signed_packet(&packet).unwrap()
    }

    #[test]
    fn a_record_is_ours_only_with_our_relay_and_nothing_else() {
        let key = SecretKey::from_bytes(&[7; 32]);
        let (home, other) = (
            url("https://home.relay.example/"),
            url("https://other.example/"),
        );
        let direct = "192.0.2.1:7777".parse().unwrap();
        let ours = EndpointAddr {
            id: key.public(),
            addrs: [
                TransportAddr::Relay(home.clone()),
                TransportAddr::Ip(direct),
            ]
            .into(),
        };
        let record = |info: EndpointInfo| signed(&key, info);
        let base = || EndpointInfo::new(key.public());
        assert!(publishes(
            &ours,
            &record(base().with_relay_url(home.clone()))
        ));
        assert!(!publishes(&ours, &record(base().with_relay_url(other))));
        // Direct addresses that this endpoint does not publish.
        let extra = base().with_relay_url(home).with_ip_addrs(vec![direct]);
        assert!(!publishes(&ours, &record(extra)));
        assert!(!publishes(&ours, &record(base())));
    }
}
