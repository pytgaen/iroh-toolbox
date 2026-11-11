use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use iroh::endpoint::Connection;
use tokio::task::JoinSet;

use crate::{
    config::Settings,
    network::{self, PING_ALPN},
    slot::{self, Protocol, Serve},
    target::{self, Destination},
};

async fn echo(conn: Connection, timeout: Duration) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(timeout, conn.accept_bi()).await??;
    tokio::io::copy(&mut recv, &mut send).await?;
    send.finish()?;
    send.stopped().await?;
    Ok(())
}

pub async fn serve(settings: &Settings, max_connections: u64, serve: Serve<'_>) -> Result<()> {
    let (ep, server) =
        slot::bind_server(settings, vec![PING_ALPN.to_vec()], Protocol::Ping, serve).await?;
    slot::announce(
        settings,
        &ep,
        server.as_deref(),
        "ping connect",
        " --count 3",
    )?;
    let mut tasks = JoinSet::new();
    // Handshakes of strangers and open sessions have separate budgets.
    let (pending, active) = (network::Pending::default(), network::Counter::default());
    loop {
        tokio::select! {
            incoming = ep.accept() => {
                let incoming = incoming.context("endpoint closed")?;
                // A full budget retries unvalidated sources and evicts the oldest
                // handshake that has not sent its hello.
                let Some((incoming, mut handshaking)) = pending.screen(incoming) else {
                    continue;
                };
                let timeout = settings.timeout;
                let server = server.clone();
                let active = active.clone();
                tasks.spawn(async move {
                    let Some(conn) = slot::accept(server.as_deref(), incoming, "ping", &mut handshaking).await else {
                        return Ok(());
                    };
                    drop(handshaking);
                    let full = network::at_capacity(active.get(), max_connections);
                    let Some(_serving) = (!full).then(|| active.try_take(max_connections)).flatten() else {
                        conn.close(network::BUSY.into(), b"the server is at its connection limit");
                        return Ok(());
                    };
                    echo(conn, timeout).await
                });
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if !matches!(result, Ok(Ok(()))) { note!("ping client disconnected or failed"); }
            }
        }
    }
}

pub async fn connect(
    settings: &Settings,
    target: &str,
    addrs: &[String],
    count: u64,
    interval: f64,
    as_json: bool,
) -> Result<()> {
    let destination = Destination::parse(target, addrs).await?;
    let ep = target::bind_client(settings, &destination.target).await?;
    let result = async {
        let conn = target::connect(
            &ep,
            &destination,
            PING_ALPN,
            Protocol::Ping,
            "ping connect",
            settings,
        )
        .await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        for seq in 1..=count {
            let payload = seq.to_be_bytes();
            let started = Instant::now();
            let mut reply = [0; 8];
            tokio::time::timeout(settings.timeout, async {
                send.write_all(&payload).await?;
                recv.read_exact(&mut reply).await?;
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context("ping timed out")??;
            ensure!(reply == payload, "invalid ping reply");
            let mut info = network::path_info(&conn);
            info["seq"] = seq.into();
            info["rtt_ms"] = (started.elapsed().as_secs_f64() * 1000.0).into();
            if as_json {
                out!("{info}");
            } else {
                out!(
                    "seq={seq} rtt={:.3}ms QUIC route={} carrier={} remote={}",
                    info["rtt_ms"].as_f64().unwrap(),
                    info["route"].as_str().unwrap(),
                    info["carrier"].as_str().unwrap(),
                    info["remote_addr"].as_str().unwrap_or("unknown")
                );
            }
            if seq < count {
                tokio::time::sleep(Duration::from_secs_f64(interval)).await;
            }
        }
        send.finish()?;
        tokio::time::timeout(settings.timeout, recv.read_to_end(4096)).await??;
        Ok(())
    }
    .await;
    ep.close().await;
    result
}
