use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

pub const DISCOVERY_PORT: u16 = 9475;
const DISCOVERY_MAGIC: &str = "INORI_DISCOVER_V1";
const DISCOVERY_GROUP: SocketAddr = SocketAddr::V4(std::net::SocketAddrV4::new(
    Ipv4Addr::BROADCAST,
    DISCOVERY_PORT,
));

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryPacket {
    magic: String,
    nonce: String,
    fingerprint: String,
    port: u16,
}

fn valid_nonce(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse_response(payload: &[u8], nonce: &str, fingerprint: &str) -> Option<u16> {
    let response: DiscoveryPacket = serde_json::from_slice(payload).ok()?;
    (response.magic == DISCOVERY_MAGIC
        && response.nonce == nonce
        && response.fingerprint == fingerprint
        && response.port != 0)
        .then_some(response.port)
}

/// Ask servers on the local IPv4 broadcast domain for a LAN address.
/// A response is useful only when its certificate fingerprint matches the paired server.
pub async fn discover_local(fingerprint: &str) -> Result<Option<SocketAddr>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let request = format!("{DISCOVERY_MAGIC} {nonce}");
    socket.send_to(request.as_bytes(), DISCOVERY_GROUP).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(650);
    let mut packet = [0_u8; 512];
    loop {
        let received = tokio::time::timeout_at(deadline, socket.recv_from(&mut packet)).await;
        let Ok(Ok((size, source))) = received else {
            return Ok(None);
        };
        if !matches!(source.ip(), IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast())
        {
            continue;
        }
        if let Some(port) = parse_response(&packet[..size], &nonce, fingerprint) {
            return Ok(Some(SocketAddr::new(source.ip(), port)));
        }
    }
}

/// Answer short-lived, nonce-bound discovery requests without exposing credentials.
pub async fn serve_discovery(
    lan_port: u16,
    fingerprint: String,
    shutdown: CancellationToken,
) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT))
        .await
        .with_context(|| format!("LAN discovery UDP port {DISCOVERY_PORT} is unavailable"))?;
    let mut recent = HashMap::<IpAddr, Instant>::new();
    let mut buffer = [0_u8; 256];
    loop {
        let (size, source) = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            received = socket.recv_from(&mut buffer) => received?,
        };
        let Ok(request) = std::str::from_utf8(&buffer[..size]) else {
            continue;
        };
        let Some(nonce) = request.strip_prefix(&format!("{DISCOVERY_MAGIC} ")) else {
            continue;
        };
        if !valid_nonce(nonce) || source.ip().is_unspecified() || source.ip().is_multicast() {
            continue;
        }
        let now = Instant::now();
        if recent
            .get(&source.ip())
            .is_some_and(|previous| now.duration_since(*previous) < Duration::from_millis(400))
        {
            continue;
        }
        if recent.len() >= 256 {
            recent.retain(|_, previous| now.duration_since(*previous) < Duration::from_secs(30));
            if recent.len() >= 256 && !recent.contains_key(&source.ip()) {
                continue;
            }
        }
        recent.insert(source.ip(), now);
        let response = serde_json::to_vec(&DiscoveryPacket {
            magic: DISCOVERY_MAGIC.into(),
            nonce: nonce.into(),
            fingerprint: fingerprint.clone(),
            port: lan_port,
        })?;
        socket.send_to(&response, source).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_response_must_match_nonce_and_paired_certificate() {
        let response = serde_json::to_vec(&DiscoveryPacket {
            magic: DISCOVERY_MAGIC.into(),
            nonce: "0123456789abcdef0123456789abcdef".into(),
            fingerprint: "paired-cert".into(),
            port: 9473,
        })
        .unwrap();
        assert_eq!(
            parse_response(&response, "0123456789abcdef0123456789abcdef", "paired-cert"),
            Some(9473)
        );
        assert_eq!(
            parse_response(&response, "ffffffffffffffffffffffffffffffff", "paired-cert"),
            None
        );
        assert_eq!(
            parse_response(&response, "0123456789abcdef0123456789abcdef", "other-cert"),
            None
        );
    }
}
