//! LAN discovery over UDP broadcast.
//!
//! A client broadcasts a `Query`; every host on the subnet answers with an
//! `Announce` straight back to the sender. That is the whole protocol. mDNS
//! would be the "proper" answer but drags in a much larger dependency and a
//! second daemon's worth of behaviour for something that has to work on exactly
//! one subnet.
//!
//! Broadcast has to go out per-interface: a machine on both WiFi and Ethernet
//! has two different broadcast addresses, and 255.255.255.255 is dropped by
//! plenty of stacks.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::protocol::{Beacon, DeviceId, DISCOVERY_PORT, PROTOCOL_VERSION};

/// A host that answered a query.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Found {
    pub id: String,
    pub name: String,
    pub os: String,
    pub address: String,
    pub port: u16,
    pub needs_code: bool,
}

/// Binds the discovery socket. Hosts keep one of these for their whole life;
/// clients open one per scan.
async fn bind(port: u16) -> std::io::Result<UdpSocket> {
    let socket = std::net::UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket)
}

/// Every IPv4 broadcast address this machine can reach, plus the global one as
/// a fallback for interfaces that report no broadcast address.
fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut targets = Vec::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            if iface.is_loopback() {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                if let Some(broadcast) = v4.broadcast {
                    if !targets.contains(&broadcast) {
                        targets.push(broadcast);
                    }
                }
            }
        }
    }
    if targets.is_empty() {
        targets.push(Ipv4Addr::BROADCAST);
    }
    targets
}

/// Runs the host's responder until the returned handle is dropped.
///
/// Answering queries rather than announcing on a timer means an idle host is
/// silent, and a client that just opened the app still gets an answer within a
/// round trip.
pub fn serve_announcements(
    id: DeviceId,
    name: String,
    os: String,
    port: u16,
    accepting: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(socket) = bind(DISCOVERY_PORT).await else {
            return;
        };
        let mut buf = [0u8; 512];
        loop {
            let Ok((len, from)) = socket.recv_from(&mut buf).await else {
                // A transient ICMP error should not kill discovery for good.
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            };
            let Ok(Beacon::Query { protocol }) = postcard::from_bytes::<Beacon>(&buf[..len]) else {
                continue;
            };
            if protocol != PROTOCOL_VERSION {
                continue;
            }

            let reply = Beacon::Announce {
                protocol: PROTOCOL_VERSION,
                id,
                name: name.clone(),
                os: os.clone(),
                port,
                needs_code: accepting.load(std::sync::atomic::Ordering::Relaxed),
            };
            if let Ok(bytes) = postcard::to_stdvec(&reply) {
                let _ = socket.send_to(&bytes, from).await;
            }
        }
    })
}

/// Broadcasts a query and collects answers for `window`.
///
/// Hosts are keyed by device id, so a machine reachable on two interfaces shows
/// up once rather than twice.
pub async fn scan(window: Duration) -> Vec<Found> {
    // Port 0: the reply comes back to whatever ephemeral port we get, which
    // keeps a client from colliding with a host on the same machine.
    let Ok(socket) = bind(0).await else {
        return Vec::new();
    };

    let query = Beacon::Query {
        protocol: PROTOCOL_VERSION,
    };
    let Ok(bytes) = postcard::to_stdvec(&query) else {
        return Vec::new();
    };
    for target in broadcast_targets() {
        let _ = socket
            .send_to(&bytes, SocketAddrV4::new(target, DISCOVERY_PORT))
            .await;
    }

    let mut found: Vec<Found> = Vec::new();
    let mut buf = [0u8; 512];
    let deadline = tokio::time::Instant::now() + window;

    while let Ok(Ok((len, from))) =
        tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await
    {
        let Ok(Beacon::Announce {
            protocol,
            id,
            name,
            os,
            port,
            needs_code,
        }) = postcard::from_bytes::<Beacon>(&buf[..len])
        else {
            continue;
        };
        if protocol != PROTOCOL_VERSION {
            continue;
        }

        let hex = crate::protocol::id_to_hex(&id);
        if found.iter().any(|f| f.id == hex) {
            continue;
        }
        found.push(Found {
            id: hex,
            name,
            os,
            address: match from.ip() {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => v6.to_string(),
            },
            port,
            needs_code,
        });
    }

    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}
