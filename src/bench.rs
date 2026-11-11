use std::{
    future::Future,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde_json::{Value, json};

use crate::{
    cli::Direction,
    config::Settings,
    network::{self, BENCH_ALPN},
    progress,
};

/// Bench steps follow the shared levels: `step` by default, `phase` from -v,
/// raw per-second lines from -vv.
struct Progress {
    enabled: bool,
    phase: &'static str,
    info: bool,
    bytes: u64,
    total: u64,
    started: Instant,
    observed: Vec<Value>,
}

impl Progress {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            phase: "starting",
            info: false,
            bytes: 0,
            total: 0,
            started: Instant::now(),
            observed: vec![],
        }
    }
    fn step(&mut self, step: &'static str, total: u64) {
        self.start(step, total, true);
    }
    fn phase(&mut self, phase: &'static str, total: u64) {
        self.start(phase, total, false);
    }
    fn start(&mut self, phase: &'static str, total: u64, info: bool) {
        // Keep the finished transfer line on screen.
        progress::keep_status();
        self.phase = phase;
        self.info = info;
        self.total = total;
        self.bytes = 0;
        self.started = Instant::now();
        self.observed.clear();
        self.render(None, true);
    }
    async fn wait<T>(&self, conn: Option<&Connection>, future: impl Future<Output = T>) -> T {
        tokio::pin!(future);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                result = &mut future => return result,
                _ = tick.tick() => self.report(conn),
            }
        }
    }
    fn observe(&mut self, conn: &Connection) {
        let info = network::path_info(conn);
        let route = json!({"route":info["route"],"carrier":info["carrier"],"remote_addr":info["remote_addr"]});
        if self.observed.last() != Some(&route) {
            self.observed.push(route);
        }
    }
    fn report(&self, conn: Option<&Connection>) {
        self.render(conn, false);
    }
    fn render(&self, conn: Option<&Connection>, new_phase: bool) {
        if !self.enabled {
            return;
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        let route = conn
            .map(network::path_info)
            .and_then(|v| v["route"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into());
        let percent = self.bytes as f64 / self.total.max(1) as f64 * 100.0;
        let mbps = self.bytes as f64 * 8.0 / elapsed.max(1e-9) / 1e6;
        if progress::level() >= 2 {
            if self.total == 0 {
                note!("bench: {} elapsed={elapsed:.1}s", self.phase);
            } else {
                note!(
                    "bench: {} {:.1}/{:.1} MiB ({percent:.0}%) {mbps:.2} Mbit/s elapsed={elapsed:.1}s route={route}",
                    self.phase,
                    self.bytes as f64 / 1048576.0,
                    self.total as f64 / 1048576.0,
                );
            }
            return;
        }
        if !self.info && progress::level() == 0 {
            return;
        }
        if self.total == 0 {
            if new_phase {
                note!("{}", self.phase);
            }
        } else if progress::live() {
            progress::draw_status(&format!(
                "{} {percent:.0}% ({}/{}) · {mbps:.1} Mbit/s · {route}",
                self.phase,
                progress::mib(self.bytes),
                progress::mib(self.total),
            ));
        } else if new_phase {
            note!("{} ({})", self.phase, progress::mib(self.total));
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        progress::keep_status();
    }
}

async fn write_bytes(
    send: &mut SendStream,
    size: u64,
    progress: &mut Progress,
    conn: &Connection,
) -> Result<()> {
    let block = vec![b'x'; 262144];
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    while progress.bytes < size {
        let count = (size - progress.bytes).min(block.len() as u64) as usize;
        let write = send.write_all(&block[..count]);
        tokio::pin!(write);
        loop {
            tokio::select! {
                result = &mut write => { result?; break; },
                _ = tick.tick() => progress.report(Some(conn)),
            }
        }
        progress.bytes += count as u64;
        progress.observe(conn);
    }
    progress.report(Some(conn));
    Ok(())
}

async fn read_bytes(
    recv: &mut RecvStream,
    size: u64,
    progress: &mut Progress,
    conn: &Connection,
) -> Result<()> {
    let mut block = vec![0; 262144];
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    while progress.bytes < size {
        let count = (size - progress.bytes).min(block.len() as u64) as usize;
        let read = recv.read(&mut block[..count]);
        tokio::pin!(read);
        let count = loop {
            tokio::select! {
                result = &mut read => break result?.context("truncated benchmark stream")?,
                _ = tick.tick() => progress.report(Some(conn)),
            }
        };
        progress.bytes += count as u64;
        progress.observe(conn);
    }
    progress.report(Some(conn));
    Ok(())
}

async fn expect(recv: &mut RecvStream, value: u8) -> Result<()> {
    let mut byte = [0];
    recv.read_exact(&mut byte).await?;
    ensure!(byte[0] == value, "invalid benchmark acknowledgement");
    Ok(())
}

pub async fn serve(
    settings: &Settings,
    max_bytes: u64,
    transfer_timeout: f64,
    no_progress: bool,
    serve: crate::slot::Serve<'_>,
) -> Result<()> {
    let mut progress = Progress::new(!no_progress);
    progress.phase("registering endpoint", 0);
    let (ep, server) = progress
        .wait(
            None,
            crate::slot::bind_server(
                settings,
                vec![BENCH_ALPN.to_vec()],
                crate::slot::Protocol::Bench,
                serve,
            ),
        )
        .await?;
    crate::slot::announce(settings, &ep, server.as_deref(), "bench connect", "")?;
    let result = async {
        progress.step("waiting for peer", 0);
        let busy_server = server.clone();
        let conn = progress
            .wait(
                None,
                network::accept_first(&ep, network::HANDSHAKE_DEADLINE, "bench", move |conn| {
                    let server = server.clone();
                    async move {
                        crate::slot::authenticate(
                            server.as_deref(),
                            &conn,
                            network::HANDSHAKE_DEADLINE,
                        )
                        .await?;
                        Ok(conn)
                    }
                }),
            )
            .await?;
        // A benchmark serves one client: later ones learn why instead of timing out.
        let _busy = network::refuse_while_busy(&ep, busy_server);
        progress.phase("negotiating benchmark", 0);
        tokio::time::timeout(Duration::from_secs_f64(transfer_timeout), async {
            let (mut send, mut recv) = progress.wait(Some(&conn), conn.accept_bi()).await?;
            let mut header = [0; 9];
            progress
                .wait(Some(&conn), recv.read_exact(&mut header))
                .await?;
            let size = u64::from_be_bytes(header[..8].try_into().unwrap());
            let code = header[8];
            ensure!(
                size > 0 && size <= max_bytes && (1..=3).contains(&code),
                "invalid benchmark request or size exceeds --max-bytes"
            );
            send.write_all(b"R").await?;
            if code == 1 || code == 3 {
                progress.step("receiving upload", size);
                read_bytes(&mut recv, size, &mut progress, &conn).await?;
                send.write_all(b"A").await?;
            }
            if code == 2 || code == 3 {
                progress.wait(Some(&conn), expect(&mut recv, b'G')).await?;
                progress.step("sending download", size);
                write_bytes(&mut send, size, &mut progress, &conn).await?;
                progress.wait(Some(&conn), expect(&mut recv, b'D')).await?;
            }
            ensure!(
                progress
                    .wait(Some(&conn), recv.read_to_end(1))
                    .await?
                    .is_empty(),
                "unexpected benchmark data"
            );
            send.finish()?;
            send.stopped().await?;
            note!("benchmark complete");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("benchmark transfer timed out")?
    }
    .await;
    ep.close().await;
    result
}

pub struct ConnectOptions<'a> {
    pub target: &'a str,
    pub addrs: &'a [String],
    pub size: u64,
    pub direction: Direction,
    pub transfer_timeout: f64,
    pub json: bool,
    pub no_progress: bool,
}

pub async fn connect(settings: &Settings, options: ConnectOptions<'_>) -> Result<()> {
    let mut progress = Progress::new(!options.no_progress);
    progress.phase("registering endpoint", 0);
    let destination = crate::target::Destination::parse(options.target, options.addrs).await?;
    let ep = progress
        .wait(
            None,
            crate::target::bind_client(settings, &destination.target),
        )
        .await?;
    let result = async {
        progress.phase("connecting to peer", 0);
        let conn = progress
            .wait(
                None,
                crate::target::connect(
                    &ep,
                    &destination,
                    BENCH_ALPN,
                    crate::slot::Protocol::Bench,
                    "bench connect",
                    settings,
                ),
            )
            .await?;
        progress.phase("negotiating benchmark", 0);
        tokio::time::timeout(Duration::from_secs_f64(options.transfer_timeout), async {
            let (mut send, mut recv) = conn.open_bi().await?;
            let code = match options.direction {
                Direction::Upload => 1,
                Direction::Download => 2,
                Direction::Both => 3,
            };
            send.write_all(&options.size.to_be_bytes()).await?;
            send.write_all(&[code]).await?;
            progress.wait(Some(&conn), expect(&mut recv, b'R')).await?;
            let mut results = vec![];
            if code == 1 || code == 3 {
                progress.step("upload", options.size);
                let start = Instant::now();
                write_bytes(&mut send, options.size, &mut progress, &conn).await?;
                progress.wait(Some(&conn), expect(&mut recv, b'A')).await?;
                results.push(record("upload", options.size, start, &conn, &progress));
            }
            if code == 2 || code == 3 {
                progress.step("download", options.size);
                let start = Instant::now();
                send.write_all(b"G").await?;
                read_bytes(&mut recv, options.size, &mut progress, &conn).await?;
                results.push(record("download", options.size, start, &conn, &progress));
                send.write_all(b"D").await?;
            }
            send.finish()?;
            ensure!(
                progress
                    .wait(Some(&conn), recv.read_to_end(1))
                    .await?
                    .is_empty(),
                "unexpected benchmark response"
            );
            for result in results {
                if options.json {
                    out!("{result}");
                } else {
                    out!(
                        "{}: {:.2} Mbit/s bytes={} duration={:.3}s route={} carrier={}",
                        result["direction"].as_str().unwrap(),
                        result["mbps"].as_f64().unwrap(),
                        options.size,
                        result["seconds"].as_f64().unwrap(),
                        result["route"].as_str().unwrap(),
                        result["carrier"].as_str().unwrap()
                    );
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("benchmark transfer timed out")?
        .map_err(|error| network::explain_busy(&conn, error))
    }
    .await;
    ep.close().await;
    result
}

fn record(
    direction: &str,
    size: u64,
    started: Instant,
    conn: &Connection,
    progress: &Progress,
) -> Value {
    let seconds = started.elapsed().as_secs_f64();
    let mut result = network::path_info(conn);
    result["direction"] = direction.into();
    result["bytes"] = size.into();
    result["seconds"] = seconds.into();
    result["mbps"] = (size as f64 * 8.0 / seconds.max(1e-9) / 1e6).into();
    result["observed_routes"] = json!(progress.observed);
    result
}
