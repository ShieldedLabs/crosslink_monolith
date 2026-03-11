
use std::net::Ipv6Addr;
use static_assertions::const_assert;

use rand::RngCore;
use rand_chacha::ChaCha20Rng;
use rand::SeedableRng;
use rand::seq::SliceRandom;

use std::io;

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

use std::collections::HashMap;

struct RawModePanicSafe;
impl RawModePanicSafe { fn new() -> Self { crossterm::terminal::enable_raw_mode().unwrap(); RawModePanicSafe } }
impl Drop for RawModePanicSafe { fn drop(&mut self) { crossterm::terminal::disable_raw_mode().unwrap(); } }

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
            _ => {}
        }
    }
    None
}

macro_rules! println_redraw {
    ($buf:expr, $name:expr, $($arg:tt)*) => {{
        clear_line();
        println!($($arg)*);
        redraw(&$buf, &$name);
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


#[derive(Debug, Default, Clone, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct Peer {
    pub send_time: u64,
    pub address: STPAddress,
}

pub fn send_to(socket: SockHandle, packets_to_send: &mut Vec<(ConnectionKey, Vec<u8>)>, chat_buf: &String, name: &str, connection_key: &ConnectionKey, buf: &[u8]) {
    if PRINT_SENDS {
        println_redraw!(chat_buf, name, "Sent {} to: {:?}.", PACKET_TYPE_NAMES[buf[0] as usize], connection_key);
    }
    packets_to_send.push((*connection_key, Vec::from(buf)));
}

pub fn is_connected(connection: &ConnectionTrackingData) -> bool {
    if let ConnectionState::Connected { .. } = connection.connection_state {
        true
    } else {
        false
    }
}

pub fn p2p(port: u16, peer_addresses: Vec<STPAddress>) {
    socket_setup();
    monotonic_clock_setup();

    let socket = setup_and_bind_udp_socket(port);

    let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(monotonic_clock_ns());

    let mut connections_map = HashMap::<ConnectionKey, ConnectionTrackingData>::new();

    let my_connect_keypair = if peer_addresses.len() == 0 { // I am The Seeder
        IdentityKeyPair {
            magic1:  CONNECT_MAGIC1_PLAIN_TEXT,
            private: vec![0xAAu8; 32],
            public:  vec![0xAAu8; 32],
        }
    } else { // I'm not The Seeder
        new_keypair_from_connect_magic1(CONNECT_MAGIC1_PLAIN_TEXT).unwrap()
    };
    let my_listen_keypair_plaintext = my_connect_keypair.clone();
    let my_listen_keypair_encrypted = my_connect_keypair.clone();

    let my_listen_keypairs = vec![my_listen_keypair_plaintext, my_listen_keypair_encrypted];

    for address in &peer_addresses {
        if address.magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
            connect_to_endpoint(socket, &mut connections_map, &my_listen_keypairs[0], address);
        } else {
            panic!("Dev: Plaintext only for now!!!"); // @Debug @Temporary @Remove
            if address.magic1 == CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s {
                connect_to_endpoint(socket, &mut connections_map, &my_listen_keypairs[1], address);
            }
        }
    }

    println!("Choose a name (max 64 bytes): ");
    let mut name = String::new();
    io::stdin().read_line(&mut name).unwrap();
    name = name.trim().to_string(); // strip trailing newline
    name.truncate(64);

    let _raw = RawModePanicSafe::new();

    let mut chat_buf = String::default();

    let mut packet_memory_encrypted = new_packet_memory(); // Incoming Encrypted / Outgoing Encrypted
    let mut packet_memory_recv = new_packet_memory(); // Incoming Decrypted
    let mut packet_memory_send = new_packet_memory(); // Outgoing Decrypted

    let mut peers = HashMap::<ConnectionKey, Peer>::new();

    println_redraw!(chat_buf, name, "");

    loop {
        let now = monotonic_clock_ns();

        let mut packets_to_send = Vec::new();

        let mut peer_addresses_list: Vec<STPAddress> = connections_map.iter().filter(|(connection_key, connection)| is_connected(*connection)).map(|(connection_key, connection)| connection.address()).collect();
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
                let key = &address.key[..32];

                o += address.ip.octets().write_to(&mut buf[o..]);
                o += address.port       .write_to(&mut buf[o..]);
                o += address.magic1     .write_to(&mut buf[o..]);
                o += key                .write_to(&mut buf[o..]);
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
                if is_connected(connection) {
                    send_to(socket, &mut packets_to_send, &chat_buf, &name, &connection_key, &buf[..o]);
                }
            }

            println_redraw!(chat_buf, name, "{}: {}", name, line);
        }

        let mut packets_received_this_tick = Vec::new();

        service_connections(&mut connections_map, &mut packets_received_this_tick, &packets_to_send, &mut packet_memory_encrypted, &mut packet_memory_recv, &mut packet_memory_send, socket, &my_listen_keypairs);

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
        for (address, data) in &packets_received_this_tick {
            let n = data.len();

            if n <= 0 {
                continue;
            }

            let buf = &data[..n];

            if PRINT_RECEIVES {
                println_redraw!(chat_buf, name, "Got {} from: {:?}.", PACKET_TYPE_NAMES[buf[0] as usize], address);
            }

            if buf[0] == PACKET_TYPE_PEER_LIST {
                let buf = &buf[1..];

                let chunks = buf.chunks_exact(58);

                clear_line();
                if PRINT_PEER_LIST { print!("Got PACKET_TYPE_PEER_LIST, with {} peer addresses: ", chunks.len()); }

                let mut new_peers = Vec::new();

                let mut comma = false;
                for chunk in chunks {
                    let ip     = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&chunk[ 0..16]).unwrap());
                    let port   =       u16::from_le_bytes(<[u8;  2]>::try_from(&chunk[16..18]).unwrap());
                    let magic1 =       u64::from_le_bytes(<[u8;  8]>::try_from(&chunk[18..26]).unwrap());
                    let key    =                                     Vec::from(&chunk[26..58]);

                    let is_myself = (magic1 == my_connect_keypair.magic1) && (key == my_connect_keypair.public);

                    let address = STPAddress { ip, port, magic1, key };

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

                    new_peers.push(address.clone());

                    // Discovered new peer! Connect.

                    if magic1 != CONNECT_MAGIC1_PLAIN_TEXT {
                        panic!("Dev: Plaintext only for now!!!"); // @Debug @Temporary @Remove
                    }
                    connect_to_endpoint(socket, &mut connections_map, &my_connect_keypair, &address);
                }

                if PRINT_PEER_LIST { println!(""); redraw(&chat_buf, &name); }

                for new_peer in new_peers {
                    println_redraw!(chat_buf, name, "Discovered new peer: {:?}", new_peer);
                }
            } else if buf[0] == PACKET_TYPE_CHAT {
                let buf = &buf[1..];

                if buf.len() < 64 { continue; }

                let peer_name = &buf[..64];

                let buf = &buf[64..];

                println_redraw!(chat_buf, name, "{}: {}",
                                std::str::from_utf8(peer_name).unwrap_or("?").trim_end_matches('\0'),
                                std::str::from_utf8(buf).unwrap_or("?").trim_end_matches('\0'));
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

use crate::native_sockets::*;
