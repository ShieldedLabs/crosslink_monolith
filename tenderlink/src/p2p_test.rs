
use std::net::Ipv6Addr;
use static_assertions::const_assert;

use rand::RngCore;
use rand_chacha::ChaCha20Rng;
use rand::SeedableRng;
use rand::seq::SliceRandom;

use std::io;

const PRINT_UNKNOWN_CHAR             :bool=0!=                (0);
const PRINT_RECEIVES                 :bool=0!=                (0);
const PRINT_SENDS                    :bool=0!=                (0);
const PRINT_PEER_LIST                :bool=0!=                (0);


const MAX_MTU: usize = 15972;
const MIN_MTU: usize =  1232;

const PACKET_TYPE_PEER_LIST: u8 = 3;
const PACKET_TYPE_CHAT:      u8 = 4;
const PACKET_TYPE_COUNT:     u8 = 5;

const PACKET_TYPE_NAMES: [&str; PACKET_TYPE_COUNT as usize] = {
    let mut names = ["<INVALID>"; PACKET_TYPE_COUNT as usize];
    names[PACKET_TYPE_PEER_LIST   as usize] = "PACKET_TYPE_PEER_LIST";
    names[PACKET_TYPE_CHAT        as usize] = "PACKET_TYPE_CHAT";
    const_assert!(PACKET_TYPE_COUNT == 5); // keep names array updated when adding other tags
    names
};

use std::sync::mpsc;
use std::thread;
use std::io::BufRead;

use crossterm::{event::{poll, read, Event, KeyCode, KeyEventKind}, terminal};
use std::io::{stdout, Write};

use crate::bandwidth_test::*;
use crate::STP_ADDRESS_SERIALIZED_SIZE;

use std::collections::HashMap;

struct RawModePanicSafe;
impl RawModePanicSafe { fn new() -> Self { crossterm::terminal::enable_raw_mode().unwrap(); RawModePanicSafe } }
impl Drop for RawModePanicSafe { fn drop(&mut self) { crossterm::terminal::disable_raw_mode().unwrap(); } }

macro_rules! println_redraw {
    ($buf:expr, $name:expr, $($arg:tt)*) => {{
        clear_line();
        println!($($arg)*);
        redraw(&$buf, &$name);
    }}
}

pub fn clear_line() { print!("\x1b[1K\r"); }
pub fn redraw(buf: &str, name: &str) { clear_line(); print!("{}> {}", name, buf); stdout().flush().unwrap(); }
pub fn tick(buf: &mut String, name: &str) -> Option<String> {
    if !poll(std::time::Duration::ZERO).unwrap() { return None; }
    if let Event::Key(k) = read().unwrap() && k.kind == KeyEventKind::Press {
        match k.code {
            (KeyCode::Char('C') |
             KeyCode::Char('c')) if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => { crossterm::terminal::disable_raw_mode(); std::process::exit(0); }
            KeyCode::Char(c)   => { buf.push(c); redraw(buf, name); }
            KeyCode::Backspace => { buf.pop();   redraw(buf, name); }
            KeyCode::Enter     => { let s = buf.clone(); buf.clear(); redraw("", name); return Some(s); }
            ch => if PRINT_UNKNOWN_CHAR { println_redraw!(buf, name, "** hit unexpected char: {ch:?}"); }
        }
    }
    None
}

use crate::SliceWrite;
use crate::SliceRead;


#[derive(Debug, Default, Clone, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct Peer {
    pub send_time: u64,
    pub address: STPAddress,
}

pub fn send_to(socket: SockHandle, packets_to_send: &mut Vec<(ConnectionKey, Vec<u8>, Option<u32>)>, chat_buf: &String, name: &str, connection_key: &ConnectionKey, buf: &[u8]) {
    if PRINT_SENDS {
        println_redraw!(chat_buf, name, "Sent {} to: {:?}.", PACKET_TYPE_NAMES[buf[0] as usize], connection_key);
    }
    packets_to_send.push((*connection_key, Vec::from(buf), None));
}

pub fn p2p(port: u16, crypto: u64, keypair: Option<IdentityKeyPair>, peer_addresses: Vec<STPAddress>, use_ipv4: bool, use_ipv6: bool) {
    socket_setup();
    monotonic_clock_setup();

    let socket = setup_and_bind_udp_socket(port).unwrap();

    let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(monotonic_clock_ns());

    let mut connections_map = HashMap::<ConnectionKey, ConnectionTrackingData>::new();

    let my_keypair_encrypted = keypair.unwrap_or(new_keypair_from_connect_magic1(crypto).unwrap());
    let my_keypair_plaintext = IdentityKeyPair { magic1: CONNECT_MAGIC1_PLAIN_TEXT, ..my_keypair_encrypted.clone() };

    let my_keypairs = vec![my_keypair_encrypted.clone(), my_keypair_plaintext.clone()];

    let name = b64(&my_keypair_encrypted.public);
    assert!(name.len() <= 64);
    // println!("Choose a name (max 64 bytes): ");
    // let mut name = String::new();
    // io::stdin().read_line(&mut name).unwrap();
    // name = name.trim().to_string(); // strip trailing newline
    // name.truncate(64);

    let _raw = RawModePanicSafe::new();

    let mut chat_buf = String::default();

    let mut packet_memory_encrypted = new_packet_memory(); // Incoming Encrypted / Outgoing Encrypted
    let mut packet_memory_recv = new_packet_memory(); // Incoming Decrypted
    let mut packet_memory_send = new_packet_memory(); // Outgoing Decrypted

    let mut peers = HashMap::<ConnectionKey, Peer>::new();

    for address in &peer_addresses {
        if !use_ipv4 && address.is_ipv4() {
            continue;
        }
        if !use_ipv6 && address.is_ipv6() {
            continue;
        }
        //if let Err(s) = connect_to(&mut connections_map, &my_keypairs, address) {
        //    println_redraw!(chat_buf, name, "{}", s);
        //}
    }

    println_redraw!(chat_buf, name, "");

    loop {
        let now = monotonic_clock_ns();

        let mut packets_to_send = Vec::new();

        let mut peer_addresses_list: Vec<STPAddress> = connections_map.iter().filter(|(connection_key, connection)| connection.is_connected()).map(|(connection_key, connection)| connection.address()).collect();
        peer_addresses_list.shuffle(&mut rng);
        peer_addresses_list.truncate(20); // 1160 bytes of peers

        // Send PEER_LIST to all connected peers.
        for (connection_key, peer) in &mut peers {
            let Some(ref mut connection) = get_connected_mut(&mut connections_map, connection_key)
            else { continue; };

            let address = connection.address();
            if (now - peer.send_time) <= 250_000_000 {
                continue;
            }

            peer.send_time = now;

            let (mut buf, mut o) = ([0u8; 2048], 0);
            o += PACKET_TYPE_PEER_LIST.write_to(&mut buf[o..]);

            for address in &peer_addresses_list {
                o += address.write_to(&mut buf[o..]);
            }

            // if PRINT_PEER_LIST { println_redraw!(chat_buf, name, "Sending PEER_LIST to: {:?}. It contains {} addresses.", address, peer_addresses_list.len()); }

            send_to(socket, &mut packets_to_send, &chat_buf, &name, &connection_key, &buf[..o]);
        }

        if let Some(mut line) = tick(&mut chat_buf, &name) {
            line.truncate(1024);

            // Send CHAT to all connected peers.
            let (mut buf, mut o) = ([0u8; 2048], 0);
            o += PACKET_TYPE_CHAT.write_to(&mut buf[o..]);
            name.as_bytes()      .write_to(&mut buf[o..]);
            o += 64;
            o += line.as_bytes() .write_to(&mut buf[o..]);

            for (connection_key, connection) in &mut connections_map {
                if connection.is_connected() {
                    send_to(socket, &mut packets_to_send, &chat_buf, &name, &connection_key, &buf[..o]);
                }
            }

            println_redraw!(chat_buf, name, "{}: {}", name, line);
        }

        let mut packets_received_this_tick: Vec<(ConnectionKey, Vec<u8>, Option<u32>)> = Vec::new();

        // @Todo @Incomplete
        // service_connections(&mut connections_map, &mut packets_received_this_tick, &packets_to_send, &mut packet_memory_encrypted, &mut packet_memory_recv, &mut packet_memory_send, socket, &my_keypairs);

        // Remove peers connecting through a disabled protocol. This is a @Dev feature for testing ipv4/ipv6 issues.
        connections_map.retain(|connection_key, _| (use_ipv4 || !connection_key.is_ipv4()) && (use_ipv6 || !connection_key.is_ipv6()));

        // Remove peers that have been disconnected.
        peers.retain(|connection_key, peer| {
            if connections_map.get(connection_key).is_some() {
                true
            } else {
                println_redraw!(chat_buf, name, "Disconnected from {:?}.", peer.address);
                false
            }
        });

        // Add peers that have connected.
        for (connection_key, connection) in &connections_map {
            if peers.get(connection_key).is_some() {
                continue; // Already exists in the peer map.
            }

            let address = connection.address();

            if connection.is_connected() {
                println_redraw!(chat_buf, name, "Connected to {:?}.", address.clone());
                // New connection; insert peer.
                peers.insert(*connection_key, Peer {
                    send_time: now,
                    address,
                    ..Default::default()
                });
            }
        }

        // Receive
        for (connection_key, data, _) in &packets_received_this_tick {
            let n = data.len();

            if n <= 0 {
                continue;
            }

            let Some(connection) = get_connected(&connections_map, connection_key)
            else {
                continue;
            };

            let address = connection.address();

            let buf = &data[..n];

            if PRINT_RECEIVES {
                println_redraw!(chat_buf, name, "Got {} from: {:?}.", PACKET_TYPE_NAMES[buf[0] as usize], address);
            }

            if buf[0] == PACKET_TYPE_PEER_LIST {
                let buf = &buf[1..];

                let chunks = buf.chunks_exact(STP_ADDRESS_SERIALIZED_SIZE);

                if PRINT_PEER_LIST { clear_line(); print!("Got PACKET_TYPE_PEER_LIST, with {} peer addresses: ", chunks.len()); }

                let mut new_peers: Vec<(STPAddress, Result<(), String>)> = Vec::new();

                let mut comma = false;
                for chunk in chunks {
                    let Some(address) = STPAddress::read_from(&mut &chunk[..])
                    else {
                        continue
                    };

                    let mut is_myself = false;
                    for keypair in &my_keypairs {
                        if (address.magic1 == keypair.magic1) && (address.key == keypair.public) {
                            is_myself = true;
                            break;
                        }
                    }

                    if PRINT_PEER_LIST {
                        if comma { print!(", "); } else { comma = true; }
                        print!("({}, {}, {})", address.ip, address.port, address.key[0]);
                    }

                    // @Note: Allows for anything about the addresses to differ, in case a new path will be discovered.
                    let already_exists = connections_map.iter().find(|(connection_key, connection)| connection.address() == address).is_some();

                    let skip = is_myself || already_exists;

                    if skip {
                        if is_myself {
                            if PRINT_PEER_LIST { print!(" (me)"); }
                        }
                        continue;
                    }

                    if PRINT_PEER_LIST { print!(" (new!)"); }

                    // Discovered new peer! Connect.
                    if !use_ipv4 && address.is_ipv4() {
                        continue;
                    }
                    if !use_ipv6 && address.is_ipv6() {
                        continue;
                    }
                    //new_peers.push((address.clone(), connect_to(&mut connections_map, &my_keypairs, &address)));
                }

                if PRINT_PEER_LIST { println!(""); redraw(&chat_buf, &name); }

                // for (new_peer, connect_result) in new_peers {
                //     println_redraw!(chat_buf, name, "Discovered new peer: {:?}", new_peer);
                //     if let Err(s) = connect_result {
                //         println_redraw!(chat_buf, name, "{}", s);
                //     }
                // }
            } else if buf[0] == PACKET_TYPE_CHAT {
                let buf = &buf[1..];

                if buf.len() < 64 { continue; }

                let peer_name = &buf[..64];

                let buf = &buf[64..];

                println_redraw!(chat_buf, name, "{:?}: {}",
                                address,
                                // std::str::from_utf8(peer_name).unwrap_or("?").trim_end_matches('\0'),
                                std::str::from_utf8(buf).unwrap_or("?").trim_end_matches('\0'));
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

use crate::native_sockets::*;
