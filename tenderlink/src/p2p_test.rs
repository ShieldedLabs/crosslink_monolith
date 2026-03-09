
use std::net::Ipv6Addr;

use rand::RngCore;
use rand_chacha::ChaCha20Rng;
use rand::SeedableRng;
use rand::seq::SliceRandom;

use std::io;

const PRINT_PEER_LIST :bool=0!= (0);
const PRINT_HELLO     :bool=0!= (0);


const MAX_MTU: usize = 15972;
const MIN_MTU: usize =  1232;

const PACKET_TYPE_HELLO:     u8 = 0;
const PACKET_TYPE_HELLO_ACK: u8 = 1;
const PACKET_TYPE_PEER_LIST: u8 = 2;
const PACKET_TYPE_CHAT:      u8 = 3;

use std::sync::mpsc;
use std::thread;
use std::io::BufRead;

use crossterm::{event::{poll, read, Event, KeyCode, KeyEventKind}, terminal};
use std::io::{stdout, Write};

struct RawModePanicSafe;
impl RawModePanicSafe { fn new() -> Self { crossterm::terminal::enable_raw_mode().unwrap(); RawModePanicSafe } }
impl Drop for RawModePanicSafe { fn drop(&mut self) { crossterm::terminal::disable_raw_mode().unwrap(); } }

pub fn clear_line() { print!("\x1b[1K\r"); }
pub fn redraw(buf: &str) { clear_line(); print!("> {}", buf); stdout().flush().unwrap(); }
pub fn tick(buf: &mut String) -> Option<String> {
    if !poll(std::time::Duration::ZERO).unwrap() { return None; }
    if let Event::Key(k) = read().unwrap() && k.kind == KeyEventKind::Press {
        match k.code {
            KeyCode::Char('C') |
            KeyCode::Char('c') => { if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) { std::process::exit(0); } }
            KeyCode::Char(c)   => { buf.push(c); redraw(buf); }
            KeyCode::Backspace => { buf.pop();   redraw(buf); }
            KeyCode::Enter     => { let s = buf.clone(); buf.clear(); redraw(""); return Some(s); }
            _ => {}
        }
    }
    None
}

macro_rules! println_redraw {
    ($buf:expr, $($arg:tt)*) => {{
        clear_line();
        println!($($arg)*);
        redraw(&$buf);
    }}
}


trait SliceWrite          { fn write_to(&self, buf: &mut [u8]) -> usize; }
impl  SliceWrite for u128 { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..16].copy_from_slice(&u128::to_le_bytes(*self)); 16 } }
impl  SliceWrite for u64  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 8].copy_from_slice(& u64::to_le_bytes(*self));  8 } }
impl  SliceWrite for i64  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 8].copy_from_slice(& i64::to_le_bytes(*self));  8 } }
impl  SliceWrite for u32  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 4].copy_from_slice(& u32::to_le_bytes(*self));  4 } }
impl  SliceWrite for u16  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 2].copy_from_slice(& u16::to_le_bytes(*self));  2 } }
impl  SliceWrite for u8   { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0] = *self;                                         1 } }
impl  SliceWrite for [u8] { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..self.len()].copy_from_slice(self);      self.len() } }

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
    pub node_id: u128,
    pub send_time: u64,
    pub recv_time: u64,
    pub address: IpAddress,
}

pub fn p2p(port: u16, peer_addresses: Vec<IpAddress>) {
    socket_setup();
    monotonic_clock_setup();

    let socket = setup_and_bind_udp_socket(port);

    let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(monotonic_clock_ns());

    let node_id: u128 = ((rng.next_u64() as u128) << 64) | (rng.next_u64() as u128);

    let mut peers: Vec<Peer> = peer_addresses.iter().map(|address| Peer { address: *address, recv_time: monotonic_clock_ns(), ..Peer::default() }).collect();

    println!("Choose a name (max 64 bytes): ");
    let mut name = String::new();
    io::stdin().read_line(&mut name).unwrap();
    name.truncate(64);
    name = name.trim().to_string(); // strip trailing newline

    let _raw = RawModePanicSafe::new();

    let mut chat_buf = String::default();

    loop {
        let now = monotonic_clock_ns();

        // Don't connect to myself.
        peers.retain(|p| p.node_id != node_id);

        let mut peer_addresses_list: Vec<(u128, IpAddress)> = peers.iter().filter(|p| p.state == PeerState::Connected).map(|p| (p.node_id, p.address)).collect();
        peer_addresses_list.shuffle(&mut rng);
        peer_addresses_list.truncate(32); // 1088 bytes of peers

        for peer in &mut peers {
            if (now - peer.send_time) > 250_000_000 {

                if peer.state == PeerState::Punching {
                    // Send HELLO punch hole with peers.
                    let (mut buf, mut o) = ([0u8; 2048], 0);
                    o += PACKET_TYPE_HELLO.write_to(&mut buf[o..]);
                    o += node_id          .write_to(&mut buf[o..]);

                    if PRINT_HELLO { println_redraw!(chat_buf, "Sending HELLO to: {:?}.", peer.address); }

                    peer.send_time = udp_send_with_congestion_and_dscp(socket, peer.address.0, peer.address.1, &buf[..o], Dscp::BestEffort).unwrap_or(peer.send_time);
                } else if peer.state == PeerState::Connected {
                    // Send PEER_LIST to all connected peers.
                    let (mut buf, mut o) = ([0u8; 2048], 0);
                    o += PACKET_TYPE_PEER_LIST.write_to(&mut buf[o..]);

                    for (node_id, address) in &peer_addresses_list {
                        o += node_id           .write_to(&mut buf[o..]);
                        o += address.0.octets().write_to(&mut buf[o..]);
                        o += address.1         .write_to(&mut buf[o..]);
                    }

                    // println_redraw!(chat_buf, "Sending PEER_LIST to: {:?}. It contains {} addresses.", peer.address, peer_addresses_list.len());

                    peer.send_time = udp_send_with_congestion_and_dscp(socket, peer.address.0, peer.address.1, &buf[..o], Dscp::BestEffort).unwrap_or(peer.send_time);
                }
            }
        }

        if let Some(mut line) = tick(&mut chat_buf) {
            line.truncate(1024);

            // Send CHAT to all connected peers.
            let (mut buf, mut o) = ([0u8; 2048], 0);
            o += PACKET_TYPE_CHAT.write_to(&mut buf[o..]);
            name.as_bytes().write_to(&mut buf[o..]);
            o += 64;
            o += line.as_bytes() .write_to(&mut buf[o..]);

            for peer in &mut peers {
                if peer.state == PeerState::Connected {
                    peer.send_time = udp_send_with_congestion_and_dscp(socket, peer.address.0, peer.address.1, &buf[..o], Dscp::BestEffort).unwrap_or(peer.send_time);
                }
            }

            println_redraw!(chat_buf, "{} ({}): {}", name, node_id >> 120, line);
        }

        // Timeout peers.
        for peer in &mut peers {
            let timeout = (now - peer.recv_time) >= 5_000_000_000;
            if timeout && peer.state == PeerState::Connected {
                println_redraw!(chat_buf, "Disconnected from: {:?}.", peer.address);

                peer.state = PeerState::Punching;
            }
        }
        peers.retain(|peer| {
            let connected = peer.state == PeerState::Connected;
            let is_a_seeder = peer_addresses.iter().any(|address| peer.address == *address);

            let should_keep = connected || is_a_seeder;

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
                        // println_redraw!(chat_buf, "{:?}", e);
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

            // Reply to all HELLOs with a HELLO_ACK.
            if buf[0] == PACKET_TYPE_HELLO {
                let buf = &buf[1..];

                if buf.len() < 16 { continue; }

                let peer_node_id = u128::from_le_bytes(<[u8; 16]>::try_from(&buf[..16]).unwrap());

                let buf = &buf[16..];

                if peer_node_id == node_id {
                    continue;
                }

                if PRINT_HELLO { println_redraw!(chat_buf, "Sending HELLO_ACK to: {:?}.", peer.address); }

                let (mut buf, mut o) = ([0u8; 2048], 0);
                o += PACKET_TYPE_HELLO_ACK.write_to(&mut buf[o..]);
                o += node_id              .write_to(&mut buf[o..]);
                o += peer_node_id         .write_to(&mut buf[o..]);

                peer.recv_time = recv_time;
                peer.send_time = udp_send_with_congestion_and_dscp(socket, peer.address.0, peer.address.1, &buf[..o], Dscp::BestEffort).unwrap_or(peer.send_time);
            } else if buf[0] == PACKET_TYPE_HELLO_ACK {
                let buf = &buf[1..];

                if buf.len() < 16 { continue; }

                let peer_node_id     = u128::from_le_bytes(<[u8; 16]>::try_from(&buf[..16]).unwrap());

                let buf = &buf[16..];

                if buf.len() < 16 { continue; }

                let mirrored_node_id = u128::from_le_bytes(<[u8; 16]>::try_from(&buf[..16]).unwrap());

                let buf = &buf[16..];

                if peer_node_id == node_id {
                    continue;
                }
                if mirrored_node_id != node_id {
                    continue;
                }

                peer.recv_time = recv_time;

                peer.state = PeerState::Connected;
                peer.node_id = peer_node_id;

                println_redraw!(chat_buf, "Connected to: {:?}. Now connected to {} peers.", address, peers.iter().filter(|peer| peer.state == PeerState::Connected).enumerate().count());
            } else if peer.state == PeerState::Connected {
                if buf[0] == PACKET_TYPE_PEER_LIST {
                    let buf = &buf[1..];

                    let chunks = buf.chunks_exact(34);

                    clear_line();
                    if PRINT_PEER_LIST { print!("Got PACKET_TYPE_PEER_LIST, with {} peer addresses:", chunks.len()); }

                    peer.recv_time = recv_time;

                    for chunk in chunks {
                        let id   =      u128::from_le_bytes(<[u8; 16]>::try_from(&chunk[ 0..16]).unwrap());
                        let ip   = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&chunk[16..32]).unwrap());
                        let port =       u16::from_le_bytes(<[u8;  2]>::try_from(&chunk[32..34]).unwrap());

                        let address = IpAddress(ip, port);

                        if PRINT_PEER_LIST { print!("{:?}, ", (id >> 120, address)); }

                        if id == node_id {
                            continue;
                        }

                        // @Note: Allows for the same node on different addresses in case a new path will be discovered.
                        if peers.iter().find(|p| p.address == address).is_none() {
                            peers.push(Peer { node_id: id, address, recv_time, ..Peer::default() });
                        }
                    }

                    if PRINT_PEER_LIST { println!(""); redraw(&chat_buf); }
                } else if buf[0] == PACKET_TYPE_CHAT {
                    let buf = &buf[1..];

                    if buf.len() < 64 { continue; }

                    let name = &buf[..64];

                    let buf = &buf[64..];

                    println_redraw!(chat_buf, "{} ({}): {}",
                                    std::str::from_utf8(name).unwrap_or("?").trim_end_matches('\0'),
                                    peer.node_id >> 120,
                                    std::str::from_utf8(buf).unwrap_or("?").trim_end_matches('\0'));
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

use crate::native_sockets::*;