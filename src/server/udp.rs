use std::{
    collections::HashMap,
    future::pending,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use fast_socks5::{
    new_udp_header, parse_udp_request,
    server::{states, Socks5ServerProtocol},
    util::target_addr::TargetAddr,
    ReplyError,
};
use tokio::{
    io::AsyncReadExt,
    net::{lookup_host, TcpStream, UdpSocket},
    time::{timeout, timeout_at, Instant},
};
use tracing::{debug, info};

use super::{Session, Traffic};
use crate::config::{canonical_ip, Config};

// Bounded state per association; expired destinations are removed before adding new ones.
const MAX_DESTINATIONS: usize = 1024;
const MAX_DATAGRAM: usize = 65_535;

pub(super) async fn run(
    proto: Socks5ServerProtocol<TcpStream, states::CommandRead>,
    client_hint: &TargetAddr,
    peer: SocketAddr,
    local: SocketAddr,
    config: &Config,
    session: &mut Session,
) -> Result<()> {
    session.stage = "udp_setup";
    let local_ip = canonical_ip(local.ip());
    let public_ip = canonical_ip(config.udp.public_ip.unwrap_or(local_ip));
    if public_ip.is_ipv4() != local_ip.is_ipv4() {
        proto
            .reply_error(&ReplyError::AddressTypeNotSupported)
            .await?;
        bail!("udp.public_ip must use the same address family as the client's server address");
    }
    let relay = match bind_udp(SocketAddr::new(local_ip, 0)).await {
        Ok(socket) => socket,
        Err(error) => {
            proto.reply_error(&ReplyError::GeneralFailure).await?;
            return Err(error).context("binding UDP relay");
        }
    };
    let relay_address = relay.local_addr()?;
    let advertised = SocketAddr::new(public_ip, relay_address.port());
    let client_port = match client_hint {
        TargetAddr::Ip(address) => address.port(),
        TargetAddr::Domain(_, port) => *port,
    };
    let mut association = Association {
        relay,
        outbound_v4: None,
        outbound_v6: None,
        peer_ip: canonical_ip(peer.ip()),
        // The request address may be private behind NAT; the actual TCP peer IP is authoritative.
        client: (client_port != 0).then(|| SocketAddr::new(canonical_ip(peer.ip()), client_port)),
        destinations: HashMap::new(),
        idle_timeout: Duration::from_secs(config.udp.idle_timeout_secs),
        resolve_timeout: Duration::from_secs(config.connect_timeout_secs),
    };
    let mut control = proto.reply_success(advertised).await?;
    info!(event = "udp_associated", relay = %relay_address, advertised = %advertised,
        requested_client_port = client_port, "UDP association established");
    session.stage = "udp_relay";
    let mut byte = [0];
    // Control lifetime also bounds an in-flight DNS lookup or UDP send.
    tokio::select! {
        biased;
        result = control.read(&mut byte) => {
            match result.context("reading UDP control connection")? {
                0 => Ok(()),
                _ => bail!("unexpected data on UDP control connection"),
            }
        }
        result = association.relay(&mut session.traffic) => result,
    }
}

struct Association {
    relay: UdpSocket,
    outbound_v4: Option<UdpSocket>,
    outbound_v6: Option<UdpSocket>,
    peer_ip: IpAddr,
    client: Option<SocketAddr>,
    destinations: HashMap<SocketAddr, Instant>,
    idle_timeout: Duration,
    resolve_timeout: Duration,
}

enum Packet {
    Client(usize, SocketAddr),
    Remote(usize, SocketAddr),
}

impl Association {
    async fn relay(&mut self, traffic: &mut Traffic) -> Result<()> {
        let mut client_buffer = vec![0; MAX_DATAGRAM];
        let mut remote_v4_buffer = vec![0; MAX_DATAGRAM];
        let mut remote_v6_buffer = vec![0; MAX_DATAGRAM];
        let mut deadline = Instant::now() + self.idle_timeout;
        loop {
            let active = timeout_at(
                deadline,
                self.step(
                    traffic,
                    &mut client_buffer,
                    &mut remote_v4_buffer,
                    &mut remote_v6_buffer,
                ),
            )
            .await
            .context("UDP association idle timed out")??;
            // Invalid packets cannot keep an association alive.
            if active {
                deadline = Instant::now() + self.idle_timeout;
            }
        }
    }

    async fn step(
        &mut self,
        traffic: &mut Traffic,
        client_buffer: &mut [u8],
        remote_v4_buffer: &mut [u8],
        remote_v6_buffer: &mut [u8],
    ) -> Result<bool> {
        let (packet, remote_v6) = tokio::select! {
            result = self.relay.recv_from(client_buffer) => {
                let (size, source) = result.context("receiving UDP client packet")?;
                (Packet::Client(size, source), false)
            }
            result = receive(self.outbound_v4.as_ref(), remote_v4_buffer) => {
                match result {
                    Ok((size, source)) => (Packet::Remote(size, source), false),
                    Err(error) => { debug!(event = "udp_receive_error", %error); return Ok(false); }
                }
            }
            result = receive(self.outbound_v6.as_ref(), remote_v6_buffer) => {
                match result {
                    Ok((size, source)) => (Packet::Remote(size, source), true),
                    Err(error) => { debug!(event = "udp_receive_error", %error); return Ok(false); }
                }
            }
        };
        match packet {
            Packet::Client(size, source) => {
                self.handle_client(source, &client_buffer[..size], traffic)
                    .await
            }
            Packet::Remote(size, source) => {
                let data = if remote_v6 {
                    &remote_v6_buffer[..size]
                } else {
                    &remote_v4_buffer[..size]
                };
                self.handle_remote(source, data, traffic).await
            }
        }
    }

    async fn handle_client(
        &mut self,
        source: SocketAddr,
        packet: &[u8],
        traffic: &mut Traffic,
    ) -> Result<bool> {
        let source = canonical_address(source);
        if source.ip() != self.peer_ip || self.client.is_some_and(|client| client != source) {
            debug!(event = "udp_packet_dropped", reason = "client_source", %source);
            return Ok(false);
        }
        let (fragment, destination, payload) = match parse_udp_request(packet).await {
            Ok(parsed) => parsed,
            Err(error) => {
                debug!(event = "udp_packet_dropped", reason = "invalid_header", %error);
                return Ok(false);
            }
        };
        if fragment != 0 {
            debug!(
                event = "udp_packet_dropped",
                reason = "fragmentation_unsupported"
            );
            return Ok(false);
        }
        // Pin only after a valid, unfragmented packet; malformed traffic cannot claim the port.
        if self.client.is_none() {
            self.client = Some(source);
            info!(event = "udp_client_bound", client_udp = %source, "UDP client endpoint bound");
        }
        let addresses = match self.resolve(&destination).await {
            Ok(addresses) => addresses,
            Err(error) => {
                debug!(event = "udp_packet_dropped", reason = "dns", destination = %destination, error = %error);
                return Ok(false);
            }
        };
        self.destinations
            .retain(|_, sent| sent.elapsed() < self.idle_timeout);
        for address in addresses {
            let is_new = !self.destinations.contains_key(&address);
            if is_new && self.destinations.len() >= MAX_DESTINATIONS {
                debug!(event = "udp_packet_dropped", reason = "destination_limit");
                continue;
            }
            match self.send(address, payload).await {
                Ok(bytes) => {
                    self.destinations.insert(address, Instant::now());
                    traffic.up += bytes as u64;
                    traffic.packets_up += 1;
                    if is_new {
                        info!(event = "udp_destination", destination = %destination, upstream = %address, "UDP destination used");
                    }
                    debug!(event = "udp_packet_sent", upstream = %address, bytes);
                    return Ok(true);
                }
                Err(error) => {
                    debug!(event = "udp_packet_dropped", reason = "send", upstream = %address, %error)
                }
            }
        }
        Ok(false)
    }

    async fn resolve(&self, destination: &TargetAddr) -> Result<Vec<SocketAddr>> {
        match destination {
            TargetAddr::Ip(address) => Ok(vec![canonical_address(*address)]),
            TargetAddr::Domain(host, port) => Ok(timeout(
                self.resolve_timeout,
                lookup_host((host.as_str(), *port)),
            )
            .await
            .context("UDP DNS lookup timed out")??
            .map(canonical_address)
            .collect()),
        }
    }

    async fn send(&mut self, address: SocketAddr, payload: &[u8]) -> io::Result<usize> {
        let (socket, bind_ip) = match address {
            SocketAddr::V4(_) => (&mut self.outbound_v4, IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            SocketAddr::V6(_) => (&mut self.outbound_v6, IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
        };
        if socket.is_none() {
            *socket = Some(bind_udp(SocketAddr::new(bind_ip, 0)).await?);
        }
        socket.as_ref().unwrap().send_to(payload, address).await
    }

    async fn handle_remote(
        &mut self,
        source: SocketAddr,
        payload: &[u8],
        traffic: &mut Traffic,
    ) -> Result<bool> {
        let source = canonical_address(source);
        let Some(client) = self.client else {
            return Ok(false);
        };
        if !self
            .destinations
            .get(&source)
            .is_some_and(|sent| sent.elapsed() < self.idle_timeout)
        {
            debug!(event = "udp_packet_dropped", reason = "unrequested_source", %source);
            return Ok(false);
        }
        let mut packet = new_udp_header(source)?;
        packet.extend_from_slice(payload);
        match self.relay.send_to(&packet, client).await {
            Ok(_) => {
                traffic.down += payload.len() as u64;
                traffic.packets_down += 1;
                debug!(event = "udp_packet_received", upstream = %source, bytes = payload.len());
                Ok(true)
            }
            Err(error) => {
                debug!(event = "udp_packet_dropped", reason = "reply", %error);
                Ok(false)
            }
        }
    }
}

fn canonical_address(address: SocketAddr) -> SocketAddr {
    // Preserve scope IDs for native IPv6 destinations.
    match canonical_ip(address.ip()) {
        IpAddr::V4(ip) => SocketAddr::new(IpAddr::V4(ip), address.port()),
        IpAddr::V6(_) => address,
    }
}

async fn receive(socket: Option<&UdpSocket>, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    match socket {
        Some(socket) => socket.recv_from(buffer).await,
        None => pending().await,
    }
}

async fn bind_udp(address: SocketAddr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(address).await?;
    // In particular, macOS defaults may reject even 32 KiB datagrams with EMSGSIZE.
    let options = socket2::SockRef::from(&socket);
    options.set_send_buffer_size(MAX_DATAGRAM * 2)?;
    options.set_recv_buffer_size(MAX_DATAGRAM * 4)?;
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_packet_from_another_ip_cannot_claim_or_use_the_association() {
        let mut association = Association {
            relay: bind_udp("127.0.0.1:0".parse().unwrap()).await.unwrap(),
            outbound_v4: None,
            outbound_v6: None,
            peer_ip: "127.0.0.1".parse().unwrap(),
            client: None,
            destinations: HashMap::new(),
            idle_timeout: Duration::from_secs(60),
            resolve_timeout: Duration::from_secs(1),
        };
        let target = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut packet = new_udp_header(target.local_addr().unwrap()).unwrap();
        packet.extend_from_slice(b"source-check");
        let mut traffic = Traffic::default();
        let intruder = "127.0.0.2:12345".parse().unwrap();
        assert!(!association
            .handle_client(intruder, &packet, &mut traffic)
            .await
            .unwrap());
        assert!(association.client.is_none());
        assert!(association.destinations.is_empty());
        // Exercise the actual forwarding path, including canonical IPv4-mapped sources.
        let authorized = "[::ffff:127.0.0.1]:12345".parse().unwrap();
        assert!(association
            .handle_client(authorized, &packet, &mut traffic)
            .await
            .unwrap());
        assert_eq!(
            association.client.unwrap(),
            "127.0.0.1:12345".parse().unwrap()
        );
        let mut payload = [0; 100];
        let (size, _) = timeout(Duration::from_secs(1), target.recv_from(&mut payload))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&payload[..size], b"source-check");
        assert!(!association
            .handle_client(intruder, &packet, &mut traffic)
            .await
            .unwrap());
        assert_eq!(traffic.packets_up, 1);
        assert!(
            timeout(Duration::from_millis(100), target.recv_from(&mut payload))
                .await
                .is_err()
        );
    }
}
