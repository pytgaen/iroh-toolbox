//! Network traffic of a session: bytes on the wire per route (direct or
//! relay), packet loss and route changes, for the chat's `/stats` and the
//! `--stats` / `--stats-json` options.
//!
//! Every connection a command admits or opens for its data is tracked here
//! from then on; pairing and refused attempts are not. Counters come from
//! QUIC: they include encryption, acknowledgements and retransmissions.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

use iroh::{
    TransportAddr,
    endpoint::{Closed, Connection, ConnectionStats, PathEvent, PathId, PathStats},
};
use n0_future::StreamExt;
use serde_json::{Value, json};

/// How a path reaches the peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// UDP between the two machines.
    Direct,
    /// Through a relay server, over WebSocket/TCP.
    Relay,
    /// A transport this tool does not configure.
    Other,
}

impl Route {
    fn of(addr: &TransportAddr) -> Self {
        match addr {
            TransportAddr::Ip(_) => Self::Direct,
            TransportAddr::Relay(_) => Self::Relay,
            _ => Self::Other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
            Self::Other => "other",
        }
    }
}

/// Bytes on the wire in each direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub sent: u64,
    pub received: u64,
}

impl Usage {
    fn total(self) -> u64 {
        self.sent + self.received
    }

    fn add(&mut self, other: Usage) {
        self.sent += other.sent;
        self.received += other.received;
    }
}

/// The selected path changed from one route to another, `at` after its
/// connection was tracked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
    pub at: Duration,
    pub from: Route,
    pub to: Route,
}

/// The path a live connection uses now.
#[derive(Clone, Debug, PartialEq)]
pub struct Current {
    pub route: Route,
    /// The peer's address on that path: IP and port, or the relay URL.
    pub remote: String,
    pub rtt: Duration,
}

/// Route changes kept for a report; later ones are only counted.
const CHANGES_KEPT: usize = 32;

/// Traffic of one connection, or the sum of several.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Traffic {
    /// From the first tracked connection to now, or to the last close.
    pub duration: Duration,
    pub connections: u64,
    pub open: u64,
    pub total: Usage,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub direct: Usage,
    pub relay: Usage,
    pub other: Usage,
    /// Relay servers that carried data.
    pub relays: BTreeSet<String>,
    pub current: Option<Current>,
    pub changes: Vec<Change>,
    /// Route changes beyond [`CHANGES_KEPT`].
    pub more_changes: u64,
}

impl Traffic {
    fn add_path(&mut self, addr: &TransportAddr, stats: &PathStats) {
        let usage = Usage {
            sent: stats.udp_tx.bytes,
            received: stats.udp_rx.bytes,
        };
        match addr {
            TransportAddr::Ip(_) => self.direct.add(usage),
            TransportAddr::Relay(url) => {
                self.relay.add(usage);
                if usage.total() > 0 {
                    self.relays.insert(url.to_string());
                }
            }
            _ => self.other.add(usage),
        }
    }

    fn add_totals(&mut self, stats: &ConnectionStats) {
        self.total.add(Usage {
            sent: stats.udp_tx.bytes,
            received: stats.udp_rx.bytes,
        });
        self.sent_packets += stats.sent_packets;
        self.lost_packets += stats.lost_packets;
    }

    /// Adds another connection's traffic; the duration is set by the caller.
    fn add(&mut self, other: &Traffic) {
        self.connections += other.connections;
        self.open += other.open;
        self.total.add(other.total);
        self.sent_packets += other.sent_packets;
        self.lost_packets += other.lost_packets;
        self.direct.add(other.direct);
        self.relay.add(other.relay);
        self.other.add(other.other);
        self.relays.extend(other.relays.iter().cloned());
        if other.current.is_some() {
            self.current = other.current.clone();
        }
        for change in &other.changes {
            if self.changes.len() < CHANGES_KEPT {
                self.changes.push(*change);
            } else {
                self.more_changes += 1;
            }
        }
        self.more_changes += other.more_changes;
    }

    /// The share of the bytes that went through `route`, in percent.
    fn share(&self, usage: Usage) -> f64 {
        let routed = self.direct.total() + self.relay.total() + self.other.total();
        if routed == 0 {
            0.0
        } else {
            usage.total() as f64 * 100.0 / routed as f64
        }
    }

    fn loss(&self) -> f64 {
        if self.sent_packets == 0 {
            0.0
        } else {
            self.lost_packets as f64 * 100.0 / self.sent_packets as f64
        }
    }

    /// `Stats: …` followed by detail lines (`route: …`, `direct: …`), for a
    /// chat or a block on stderr.
    pub fn lines(&self) -> (String, Vec<String>) {
        if self.connections == 0 {
            return (
                "Stats: no connection was established".to_owned(),
                Vec::new(),
            );
        }
        let header = format!(
            "Stats: sent {}, received {} on the wire in {}",
            crate::progress::size(self.total.sent),
            crate::progress::size(self.total.received),
            crate::progress::duration(self.duration)
        );
        let mut lines = Vec::new();
        if let Some(current) = &self.current {
            lines.push(format!("route: {}", describe(current)));
        }
        for (route, usage) in [
            (Route::Direct, self.direct),
            (Route::Relay, self.relay),
            (Route::Other, self.other),
        ] {
            if usage.total() == 0 {
                continue;
            }
            let mut line = format!(
                "{}: sent {}, received {} ({:.1} %)",
                route.name(),
                crate::progress::size(usage.sent),
                crate::progress::size(usage.received),
                self.share(usage)
            );
            if route == Route::Relay && !self.relays.is_empty() {
                let relays: Vec<_> = self.relays.iter().map(String::as_str).collect();
                line.push_str(&format!(" via {}", relays.join(", ")));
            }
            lines.push(line);
        }
        lines.push(format!(
            "loss: {} of {} packets ({:.2} %)",
            self.lost_packets,
            self.sent_packets,
            self.loss()
        ));
        if !self.changes.is_empty() {
            let mut changes: Vec<String> = self.changes.iter().map(change_text).collect();
            if self.more_changes > 0 {
                changes.push(format!("{} more", self.more_changes));
            }
            lines.push(format!("route changes: {}", changes.join("; ")));
        }
        if self.connections > 1 {
            lines.push(format!(
                "connections: {}, {} open",
                self.connections, self.open
            ));
        }
        (header, lines)
    }

    /// One line for a connection that closed on a server.
    pub fn line(&self, peer: &str) -> String {
        let mut routes = Vec::new();
        for (route, usage) in [
            (Route::Direct, self.direct),
            (Route::Relay, self.relay),
            (Route::Other, self.other),
        ] {
            if usage.total() > 0 {
                routes.push(format!("{} {:.1} %", route.name(), self.share(usage)));
            }
        }
        let mut line = format!(
            "Stats: connection from {peer} closed after {}: sent {}, received {}",
            crate::progress::duration(self.duration),
            crate::progress::size(self.total.sent),
            crate::progress::size(self.total.received),
        );
        if !routes.is_empty() {
            line.push_str(&format!(", {}", routes.join(", ")));
        }
        line.push_str(&format!(", {} lost packets", self.lost_packets));
        line
    }

    pub fn json(&self) -> Value {
        let usage =
            |usage: Usage| json!({"sent_bytes": usage.sent, "received_bytes": usage.received});
        let mut relay = usage(self.relay);
        relay["relay_urls"] = json!(self.relays);
        json!({
            "schema": 1,
            "duration_ms": millis(self.duration),
            "connections": self.connections,
            "open": self.open,
            "sent_bytes": self.total.sent,
            "received_bytes": self.total.received,
            "sent_packets": self.sent_packets,
            "lost_packets": self.lost_packets,
            "route": self.current.as_ref().map(|c| c.route.name()),
            "remote_addr": self.current.as_ref().map(|c| c.remote.clone()),
            "rtt_ms": self.current.as_ref().map(|c| c.rtt.as_secs_f64() * 1000.0),
            "routes": {
                "direct": usage(self.direct),
                "relay": relay,
                "other": usage(self.other),
            },
            "route_changes": self.changes.iter().map(|c| json!({
                "at_ms": millis(c.at), "from": c.from.name(), "to": c.to.name(),
            })).collect::<Vec<_>>(),
            "more_route_changes": self.more_changes,
        })
    }
}

/// `192.0.2.7:54012` or the relay URL, without iroh's `ip:` prefix.
fn address(addr: &TransportAddr) -> String {
    match addr {
        TransportAddr::Ip(addr) => addr.to_string(),
        TransportAddr::Relay(url) => url.to_string(),
        other => other.to_string(),
    }
}

fn millis(value: Duration) -> u64 {
    value.as_millis().try_into().unwrap_or(u64::MAX)
}

fn describe(current: &Current) -> String {
    let rtt = current.rtt.as_secs_f64() * 1000.0;
    match current.route {
        Route::Direct => format!("direct (UDP {}), rtt {rtt:.1} ms", current.remote),
        Route::Relay => format!("relay ({}), rtt {rtt:.1} ms", current.remote),
        Route::Other => format!("other ({}), rtt {rtt:.1} ms", current.remote),
    }
}

fn change_text(change: &Change) -> String {
    format!(
        "{} → {} after {}",
        change.from.name(),
        change.to.name(),
        crate::progress::duration(change.at)
    )
}

/// The line a chat shows when its connection changes route.
pub fn change_notice(change: &Change) -> String {
    let how = match change.to {
        Route::Direct => "direct (UDP)",
        Route::Relay => "through a relay",
        Route::Other => "through another transport",
    };
    format!(
        "Route: now {how}, {} after connecting",
        crate::progress::duration(change.at)
    )
}

/// The path `conn` uses now.
pub fn current(conn: &Connection) -> Option<Current> {
    let paths = conn.paths();
    let path = paths.iter().find(|path| path.is_selected())?;
    Some(Current {
        route: Route::of(path.remote_addr()),
        remote: address(path.remote_addr()),
        rtt: path.rtt(),
    })
}

/// The route a connection uses, and its changes.
#[derive(Default)]
struct Routes {
    selected: Option<Route>,
    changes: Vec<Change>,
}

impl Routes {
    fn select(&mut self, at: Duration, route: Route) {
        if let Some(from) = self.selected
            && from != route
        {
            self.changes.push(Change {
                at,
                from,
                to: route,
            });
        }
        self.selected = Some(route);
    }
}

/// What one tracked connection has carried so far.
struct Record {
    id: usize,
    conn: iroh::endpoint::WeakConnectionHandle,
    peer: String,
    opened: Instant,
    closed: Option<Instant>,
    /// Addresses of every path seen, open or closed.
    addrs: Vec<(PathId, TransportAddr)>,
    /// Final statistics of the paths that closed.
    ended: Vec<(PathId, PathStats)>,
    /// The path in use, for the route of a closed connection.
    selected: Option<PathId>,
    routes: Routes,
    /// Connection totals at close.
    totals: Option<ConnectionStats>,
    /// Admitted for the command's data: see [`track`].
    counted: bool,
}

impl Record {
    fn remember(&mut self, id: PathId, addr: &TransportAddr) {
        match self.addrs.iter_mut().find(|(known, _)| *known == id) {
            Some(entry) => entry.1 = addr.clone(),
            None => self.addrs.push((id, addr.clone())),
        }
    }

    fn end(&mut self, id: PathId, stats: PathStats) {
        match self.ended.iter_mut().find(|(known, _)| *known == id) {
            Some(entry) => entry.1 = stats,
            None => self.ended.push((id, stats)),
        }
    }

    fn apply(&mut self, event: PathEvent) {
        match event {
            PathEvent::Opened {
                id, remote_addr, ..
            } => self.remember(id, &remote_addr),
            PathEvent::Closed {
                id,
                remote_addr,
                last_stats,
                ..
            } => {
                self.remember(id, &remote_addr);
                self.end(id, *last_stats);
            }
            PathEvent::Selected {
                id, remote_addr, ..
            } => {
                self.remember(id, &remote_addr);
                self.selected = Some(id);
                self.routes
                    .select(self.opened.elapsed(), Route::of(&remote_addr));
            }
            _ => {}
        }
    }

    fn close(&mut self, closed: Option<Closed>) {
        self.closed = Some(Instant::now());
        if let Some(closed) = closed {
            self.totals = Some(closed.stats);
            for (id, stats) in closed.path_stats {
                self.end(id, stats);
            }
        }
    }

    fn traffic(&self) -> Traffic {
        let mut traffic = Traffic {
            connections: 1,
            duration: self
                .closed
                .unwrap_or_else(Instant::now)
                .saturating_duration_since(self.opened),
            changes: self.routes.changes.clone(),
            ..Traffic::default()
        };
        let live = match self.closed {
            None => self.conn.upgrade(),
            Some(_) => None,
        };
        let mut counted = Vec::new();
        if let Some(conn) = &live {
            traffic.open = 1;
            traffic.add_totals(&conn.stats());
            let paths = conn.paths();
            for path in paths.iter() {
                counted.push(path.id());
                traffic.add_path(path.remote_addr(), &path.stats());
            }
            traffic.current = current(conn);
        }
        for (id, stats) in &self.ended {
            if counted.contains(id) {
                continue;
            }
            if let Some((_, addr)) = self.addrs.iter().find(|(known, _)| known == id) {
                traffic.add_path(addr, stats);
            }
        }
        if live.is_none() {
            // The last route a closed connection used.
            traffic.current = self.selected.and_then(|id| {
                let (_, addr) = self.addrs.iter().find(|(known, _)| *known == id)?;
                let (_, stats) = self.ended.iter().find(|(known, _)| *known == id)?;
                Some(Current {
                    route: Route::of(addr),
                    remote: address(addr),
                    rtt: stats.rtt,
                })
            });
            match &self.totals {
                Some(totals) => traffic.add_totals(totals),
                // Dropped before its close was seen: the paths are all there is.
                None => {
                    traffic.total = Usage {
                        sent: traffic.direct.sent + traffic.relay.sent + traffic.other.sent,
                        received: traffic.direct.received
                            + traffic.relay.received
                            + traffic.other.received,
                    };
                }
            }
        }
        traffic
    }
}

/// How the command reports its traffic, from `--stats` or `--stats-json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

#[derive(Clone, Debug)]
struct Reporting {
    format: Format,
    command: String,
    /// A line per closed connection: servers and tunnels.
    per_connection: bool,
}

#[derive(Default)]
struct Registry {
    records: Vec<Arc<Mutex<Record>>>,
    /// Connections that closed, summed.
    done: Traffic,
    first: Option<Instant>,
    last_close: Option<Instant>,
}

fn registry() -> MutexGuard<'static, Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

static REPORTING: OnceLock<Reporting> = OnceLock::new();

/// Reports the traffic at the end of the command, and of each closed
/// connection with `per_connection`.
pub fn configure(format: Format, command: impl Into<String>, per_connection: bool) {
    let _ = REPORTING.set(Reporting {
        format,
        command: command.into(),
        per_connection,
    });
}

fn lock(record: &Mutex<Record>) -> MutexGuard<'_, Record> {
    record.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts `conn`'s traffic, from its start if it was watched, until it
/// closes: a connection admitted for the command's data. Tracking a
/// connection twice changes nothing.
pub fn track(conn: &Connection) {
    watch(conn);
    let id = conn.stable_id();
    let mut registry = registry();
    let opened = registry.records.iter().find_map(|record| {
        let mut record = lock(record);
        (record.id == id).then(|| {
            record.counted = true;
            record.opened
        })
    });
    if let Some(opened) = opened {
        registry.first = Some(registry.first.map_or(opened, |first| first.min(opened)));
    }
}

/// Observes `conn` from its start, before the command decides to admit it:
/// paths that close during the handshake still count once it is admitted
/// with [`track`]. A connection never admitted is forgotten.
pub fn watch(conn: &Connection) {
    let id = conn.stable_id();
    let mut tracked = registry();
    if tracked.records.iter().any(|record| lock(record).id == id) {
        return;
    }
    // Subscribed before the snapshot: a path opened in between is seen twice, never missed.
    let mut events = conn.path_events();
    let weak = conn.weak_handle();
    let closed = weak.closed();
    let mut record = Record {
        id,
        conn: weak,
        peer: conn.remote_id().fmt_short().to_string(),
        opened: Instant::now(),
        closed: None,
        addrs: Vec::new(),
        ended: Vec::new(),
        selected: None,
        routes: Routes::default(),
        totals: None,
        counted: false,
    };
    {
        let paths = conn.paths();
        for path in paths.iter() {
            record.remember(path.id(), path.remote_addr());
            if path.is_selected() {
                record.selected = Some(path.id());
                record.routes.selected = Some(Route::of(path.remote_addr()));
            }
        }
    }
    let record = Arc::new(Mutex::new(record));
    tracked.records.push(record.clone());
    drop(tracked);
    tokio::spawn(async move {
        tokio::pin!(closed);
        let closed = loop {
            tokio::select! {
                closed = &mut closed => break closed,
                event = events.next() => match event {
                    Some(event) => lock(&record).apply(event),
                    None => break closed.await,
                },
            }
        };
        // Path closures sent with the connection's close.
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(50), events.next()).await
        {
            lock(&record).apply(event);
        }
        let (traffic, peer, counted) = {
            let mut record = lock(&record);
            record.close(closed);
            (record.traffic(), record.peer.clone(), record.counted)
        };
        {
            let mut registry = registry();
            registry
                .records
                .retain(|other| !Arc::ptr_eq(other, &record));
            if !counted {
                return;
            }
            registry.done.add(&traffic);
            registry.last_close = Some(Instant::now());
        }
        if let Some(reporting) = REPORTING.get().filter(|r| r.per_connection) {
            match reporting.format {
                Format::Text => note!("{}", traffic.line(&peer)),
                Format::Json => {
                    let mut value = traffic.json();
                    value["scope"] = "connection".into();
                    value["command"] = reporting.command.clone().into();
                    value["peer"] = peer.into();
                    note!("{value}");
                }
            }
        }
    });
}

/// The traffic of every tracked connection so far.
pub fn snapshot() -> Traffic {
    let registry = registry();
    let mut traffic = registry.done.clone();
    // Live connections last: the newest one's route is the current one.
    for record in &registry.records {
        let record = lock(record);
        if record.counted {
            traffic.add(&record.traffic());
        }
    }
    traffic.duration = match registry.first {
        None => Duration::ZERO,
        Some(first) if traffic.open > 0 => first.elapsed(),
        Some(first) => registry
            .last_close
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(first),
    };
    traffic
}

/// Route changes of `conn` since it was tracked.
pub fn changes(conn: &Connection) -> Vec<Change> {
    let id = conn.stable_id();
    registry()
        .records
        .iter()
        .map(|record| lock(record))
        .find(|record| record.id == id)
        .map(|record| record.routes.changes.clone())
        .unwrap_or_default()
}

/// Whether the command reports its traffic.
pub fn wanted() -> bool {
    REPORTING.get().is_some()
}

/// Waits, at most `limit`, for closed connections to report their final
/// counters.
pub async fn settle(limit: Duration) {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        let pending = registry().records.iter().any(|record| {
            let record = lock(record);
            record.counted
                && record.closed.is_none()
                && record
                    .conn
                    .upgrade()
                    .is_none_or(|conn| conn.close_reason().is_some())
        });
        if !pending {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Prints the command's traffic, as asked by `--stats` or `--stats-json`.
pub fn report(traffic: &Traffic) {
    let Some(reporting) = REPORTING.get() else {
        return;
    };
    match reporting.format {
        Format::Text => {
            let (header, lines) = traffic.lines();
            note!("{header}");
            for line in lines {
                note!("  {line}");
            }
        }
        Format::Json => {
            let mut value = traffic.json();
            value["scope"] = "session".into();
            value["command"] = reporting.command.clone().into();
            note!("{value}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Traffic {
        Traffic {
            duration: Duration::from_secs(41),
            connections: 1,
            open: 1,
            total: Usage {
                sent: 4 << 20,
                received: 3 << 10,
            },
            sent_packets: 4000,
            lost_packets: 12,
            direct: Usage {
                sent: (4 << 20) - 2048,
                received: 2048,
            },
            relay: Usage {
                sent: 2048,
                received: 1024,
            },
            relays: ["https://relay.example/".to_owned()].into(),
            current: Some(Current {
                route: Route::Direct,
                remote: "192.0.2.7:54012".to_owned(),
                rtt: Duration::from_micros(3100),
            }),
            changes: vec![Change {
                at: Duration::from_millis(1800),
                from: Route::Relay,
                to: Route::Direct,
            }],
            ..Traffic::default()
        }
    }

    #[test]
    fn lines_name_routes_loss_and_changes() {
        let (header, lines) = sample().lines();
        assert_eq!(
            header,
            "Stats: sent 4.0 MiB, received 3.0 KiB on the wire in 41 s"
        );
        assert_eq!(
            lines,
            [
                "route: direct (UDP 192.0.2.7:54012), rtt 3.1 ms",
                "direct: sent 4.0 MiB, received 2.0 KiB (99.9 %)",
                "relay: sent 2.0 KiB, received 1.0 KiB (0.1 %) via https://relay.example/",
                "loss: 12 of 4000 packets (0.30 %)",
                "route changes: relay → direct after 1.8 s",
            ]
        );
    }

    #[test]
    fn no_connection_says_so() {
        let (header, lines) = Traffic::default().lines();
        assert_eq!(header, "Stats: no connection was established");
        assert!(lines.is_empty());
    }

    #[test]
    fn json_keeps_every_counter() {
        let value = sample().json();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["duration_ms"], 41000);
        assert_eq!(value["sent_bytes"], 4 << 20);
        assert_eq!(value["route"], "direct");
        assert_eq!(value["remote_addr"], "192.0.2.7:54012");
        assert_eq!(value["routes"]["relay"]["received_bytes"], 1024);
        assert_eq!(
            value["routes"]["relay"]["relay_urls"],
            json!(["https://relay.example/"])
        );
        assert_eq!(
            value["route_changes"],
            json!([{"at_ms": 1800, "from": "relay", "to": "direct"}])
        );
    }

    #[test]
    fn sums_keep_the_newest_route_and_cap_changes() {
        let mut sum = Traffic::default();
        let mut closed = sample();
        closed.open = 0;
        closed.current = None;
        closed.changes = vec![closed.changes[0]; CHANGES_KEPT];
        sum.add(&closed);
        sum.add(&sample());
        assert_eq!(sum.connections, 2);
        assert_eq!(sum.open, 1);
        assert_eq!(sum.total.sent, 2 * (4 << 20));
        assert_eq!(sum.current, sample().current);
        assert_eq!(sum.changes.len(), CHANGES_KEPT);
        assert_eq!(sum.more_changes, 1);
        let (_, lines) = sum.lines();
        assert!(lines.contains(&"connections: 2, 1 open".to_owned()));
        assert!(lines.iter().any(|line| line.ends_with("; 1 more")));
    }

    #[test]
    fn a_closed_connection_line_names_its_peer() {
        let mut traffic = sample();
        traffic.current = None;
        assert_eq!(
            traffic.line("3f2a1b9c0d"),
            "Stats: connection from 3f2a1b9c0d closed after 41 s: sent 4.0 MiB, received 3.0 KiB, direct 99.9 %, relay 0.1 %, 12 lost packets"
        );
    }

    #[test]
    fn route_changes_follow_selection() {
        let mut routes = Routes {
            selected: Some(Route::Relay),
            changes: Vec::new(),
        };
        let at = Duration::from_millis(1800);
        routes.select(at, Route::Relay);
        routes.select(at, Route::Direct);
        routes.select(at, Route::Direct);
        assert_eq!(
            routes.changes,
            [Change {
                at,
                from: Route::Relay,
                to: Route::Direct
            }]
        );
        assert_eq!(
            change_notice(&routes.changes[0]),
            "Route: now direct (UDP), 1.8 s after connecting"
        );
    }
}
