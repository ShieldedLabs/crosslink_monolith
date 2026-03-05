
use std::net::Ipv6Addr;

use rand_chacha::ChaCha20Rng;
use rand::SeedableRng;


const MAX_MTU: usize = 15972;
const MIN_MTU: usize =  1232;

const PACKET_TYPE_HELLO:     u8 = 0;
const PACKET_TYPE_HELLO_ACK: u8 = 1;
const PACKET_TYPE_PEER_LIST: u8 = 2;

trait SliceWrite          { fn write_to(&self, buf: &mut [u8]) -> usize; }
impl  SliceWrite for u64  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..8].copy_from_slice(&u64::to_le_bytes(*self)); 8 } }
impl  SliceWrite for i64  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..8].copy_from_slice(&i64::to_le_bytes(*self)); 8 } }
impl  SliceWrite for u32  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..4].copy_from_slice(&u32::to_le_bytes(*self)); 4 } }
impl  SliceWrite for u16  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..2].copy_from_slice(&u16::to_le_bytes(*self)); 2 } }
impl  SliceWrite for u8   { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0] = *self;                                      1 } }
impl  SliceWrite for [u8] { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..self.len()].copy_from_slice(self);   self.len() } }

#[derive(Debug, Copy, Clone, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct IpAddress(pub Ipv6Addr, pub u16);
impl Default for IpAddress { fn default() -> Self { Self(Ipv6Addr::UNSPECIFIED, 0) } }

#[derive(Debug, Default, Copy, Clone, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub enum PeerState {
    #[default] Punching,
    Connected
}

#[derive(Debug, Default, Copy, Clone, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct Peer {
    pub state: PeerState,
    pub send_time: u64,
    pub recv_time: u64,
    pub address: IpAddress,
}

pub fn p2p(port: u16, peer_addresses: Vec<IpAddress>) {
    socket_setup();
    monotonic_clock_setup();

    let socket = setup_and_bind_udp_socket(port);

    let rng = rand_chacha::ChaCha20Rng::seed_from_u64(monotonic_clock_ns());

    let mut peers: Vec<Peer> = peer_addresses.iter().map(|address| Peer { address: *address, recv_time: monotonic_clock_ns(), ..Peer::default() }).collect();

    loop {
        let now = monotonic_clock_ns();

        let peer_addresses_list: Vec<IpAddress> = (&peers[0..peers.len().min(64)]).iter().filter(|p| p.state == PeerState::Connected).map(|p| p.address).collect(); // 1153 byte packet (1 byte header + 64 * 18 byte addresses)

        for peer in &mut peers {
            if (now - peer.send_time) > 500_000_000 {

                // Send HELLO to all peers.
                let (mut buf, mut o) = ([0u8; 2048], 0);
                o += PACKET_TYPE_HELLO.write_to(&mut buf[o..]);

                // println!("Sending HELLO to: {:?}.", peer.address);

                peer.send_time = udp_send_with_congestion_and_dscp(socket, peer.address.0, peer.address.1, &buf[..o], Dscp::BestEffort).unwrap_or(peer.send_time);

                // Send PEER_LIST to all connected peers.
                if peer.state == PeerState::Connected {
                    let (mut buf, mut o) = ([0u8; 2048], 0);
                    o += PACKET_TYPE_PEER_LIST.write_to(&mut buf[o..]);

                    for address in &peer_addresses_list {
                        o += address.0.octets().write_to(&mut buf[o..]);
                        o += address.1         .write_to(&mut buf[o..]);
                    }

                    println!("Sending PEER_LIST to: {:?}. It contains {} addresses.", peer.address, peer_addresses_list.len());

                    peer.send_time = udp_send_with_congestion_and_dscp(socket, peer.address.0, peer.address.1, &buf[..o], Dscp::BestEffort).unwrap_or(peer.send_time);
                }
            }
        }

        // Timeout peers.
        peers.retain(|p| {
            let timeout = (now - p.recv_time) >= 5_000_000_000;

            let is_a_seeder = peer_addresses.iter().any(|address| p.address == *address);

            let should_keep = !timeout || is_a_seeder;

            if !should_keep {
                println!("Disconnected from: {:?}.", p.address);
            }

            should_keep
        });

        // Receive
        loop {
            let mut buf = [0u8; MAX_MTU];
            let (address, recv_time, n) = {
                match udp_recv_with_congestion_and_dscp(socket, &mut buf) {
                    Ok((n, src_ip6, src_port, _congested, _ecn_enabled, _dscp, recv_time)) => {
                        (IpAddress(src_ip6, src_port), recv_time, n)
                    },
                    Err(ref e) if e.kind() == tokio::io::ErrorKind::WouldBlock => { break; },
                    Err(e) => {
                        // println!("{:?}", e);
                        continue;
                    },
                }
            };

            if n <= 0 {
                continue;
            }

            let buf = &buf[..n];

            let peer = {
                if let Some(mut peer) = peers.iter_mut().find(|p| p.address == address) {
                    peer
                } else {
                    peers.push(Peer { address, ..Peer::default() });
                    &mut peers.last_mut().unwrap()
                }
            };

            peer.recv_time = recv_time;

            // Reply to all HELLOs with a HELLO_ACK.
            if buf[0] == PACKET_TYPE_HELLO {
                let (mut buf, mut o) = ([0u8; 2048], 0);
                o += PACKET_TYPE_HELLO_ACK.write_to(&mut buf[o..]);

                if let Ok(when) = udp_send_with_congestion_and_dscp(socket, address.0, address.1, &buf[..o], Dscp::BestEffort) {
                    peer.send_time = when;
                }
                peer.send_time = udp_send_with_congestion_and_dscp(socket, peer.address.0, peer.address.1, &buf[..o], Dscp::BestEffort).unwrap_or(peer.send_time);
            } else if buf[0] == PACKET_TYPE_HELLO_ACK {

                if peer.state == PeerState::Punching {
                    peer.state = PeerState::Connected;
                    println!("Connected to: {:?}.", peer.address);
                }

            } else if peer.state == PeerState::Connected {
                if buf[0] == PACKET_TYPE_PEER_LIST {
                    let buf = &buf[1..];

                    println!("Received {} peer addresses.", buf.chunks_exact(18).len());

                    for chunk in buf.chunks_exact(18) {
                        let ip = std::net::Ipv6Addr::from(<[u8;16]>::try_from(&chunk[0..16]).unwrap());
                        let port = u16::from_le_bytes(<[u8;2]>::try_from(&chunk[16..18]).unwrap());

                        let address = IpAddress(ip, port);

                        // println!("{:?}", address);

                        if peers.iter().find(|p| p.address == address).is_none() {
                            peers.push(Peer { address, recv_time, ..Peer::default() });
                        }
                    }
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

use crate::native_sockets::*;