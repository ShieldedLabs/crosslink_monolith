#![allow(unsafe_code)]
use std::collections::{HashMap, VecDeque};
use rand::SeedableRng;
use static_assertions::const_assert;


#[test]
pub fn ack_test() {
    use rand::Rng;

    println!("Begin the ack test!");

    let kp1 = new_keypair_from_connect_magic1(CONNECT_MAGIC1_PLAIN_TEXT).unwrap();
    let kp1_pub = kp1.public.clone();
    let kp2 = new_keypair_from_connect_magic1(CONNECT_MAGIC1_PLAIN_TEXT).unwrap();

    socket_setup();
    monotonic_clock_setup();

    let network_thread_handle1 = new_network_thread(vec![kp1.clone()], 58493, None, (1_000_000, 256 * 1024 * 1024, 256 * 1024 * 1024));
    let network_thread_handle2 = new_network_thread(vec![kp2.clone()], 23843, None, (1_000_000, 256 * 1024 * 1024, 256 * 1024 * 1024));

    let initiate_connections = vec![ STPAddress { ip: Ipv6Addr::LOCALHOST, port: 58493, magic1: CONNECT_MAGIC1_PLAIN_TEXT, key: kp1_pub, } ];
    let ret = service_connections(&network_thread_handle2, NetworkThreadPush { initiate_connections, send_unreliable: vec![], ..Default::default() });
    let mut wanted_connections = ret.current_connections;

    // Build 50 messages with random sizes between 100 bytes and 4 MiB.
    // Layout: [id: u32 LE][len: u32 LE][blake3 hash: 32 bytes][random payload]
    const NUM_MESSAGES: u32 = 50;
    const HEADER_SIZE: usize = 4 + 4 + 32; // id + len + hash
    let mut rng = rand::rng();
    let mut messages: Vec<Vec<u8>> = Vec::new();

    for id in 0..NUM_MESSAGES {
        let payload_size: usize = rng.random_range(100..=(4 * 1024 * 1024));
        let total_size = HEADER_SIZE + payload_size;
        let mut buf = vec![0u8; total_size];

        // Fill payload with random data
        rng.fill(&mut buf[HEADER_SIZE..]);

        // Write header
        let hash = blake3::hash(&buf[HEADER_SIZE..]);
        buf[0..4].copy_from_slice(&id.to_le_bytes());
        buf[4..8].copy_from_slice(&(total_size as u32).to_le_bytes());
        buf[8..40].copy_from_slice(hash.as_bytes());

        println!("Message {id}: {total_size} bytes");
        messages.push(buf);
    }

    // Send all 50 messages once we have a connection
    let mut sent = false;

    loop {
        let mut send_unreliable = Vec::new();
        if !sent && !wanted_connections.is_empty() {
            for msg in &messages {
                send_unreliable.push((wanted_connections[0].0.to_connection_key(), msg.clone()));
            }
            sent = true;
            println!("Sent {NUM_MESSAGES} messages, waiting 10s for receive...");
        } else if !sent {
            let ret = service_connections(&network_thread_handle2, NetworkThreadPush { wanted_connections, ..Default::default() });
            wanted_connections = ret.current_connections;
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        } else {
            break;
        }
        let ret = service_connections(&network_thread_handle2, NetworkThreadPush { wanted_connections, send_unreliable, ..Default::default() });
        wanted_connections = ret.current_connections;
    }

    // Receive on the other side for 10 seconds
    let mut received_ids: HashMap<u32, u32> = HashMap::new(); // id -> count
    let mut receive_order: Vec<u32> = Vec::new();
    let mut corrupted: u32 = 0;
    let mut malformed: u32 = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

    while std::time::Instant::now() < deadline {
        let ret = service_connections(&network_thread_handle1, NetworkThreadPush::default());
        for (_conn_key, data) in &ret.received_unreliable_messages {
            if data.len() < HEADER_SIZE {
                malformed += 1;
                continue;
            }
            let id = u32::from_le_bytes(data[0..4].try_into().unwrap());
            let declared_len = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
            let declared_hash: &[u8; 32] = data[8..40].try_into().unwrap();

            if declared_len != data.len() {
                malformed += 1;
                continue;
            }

            let actual_hash = blake3::hash(&data[HEADER_SIZE..]);
            if actual_hash.as_bytes() != declared_hash {
                corrupted += 1;
                continue;
            }

            receive_order.push(id);
            *received_ids.entry(id).or_insert(0) += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let unique_received = received_ids.len() as u32;
    let duplicates: u32 = received_ids.values().filter(|&&c| c > 1).map(|c| c - 1).sum();
    let missing = NUM_MESSAGES - unique_received;

    println!("\n=== Results ===");
    println!("Sent:       {NUM_MESSAGES}");
    println!("Received:   {unique_received}");
    println!("Missing:    {missing}");
    println!("Duplicates: {duplicates}");
    println!("Corrupted:  {corrupted}");
    println!("Malformed:  {malformed}");
    println!("Order:      {:?}", receive_order);
}

pub fn do_the_test_program3(port: u16, my_keypair: IdentityKeyPair, beam_to: Option<STPAddress>, max_pps: Option<u64>, send_buffer_params: (u32, u32, u32)) {
    socket_setup();
    monotonic_clock_setup();

    let network_thread_handle2 = new_network_thread(vec![my_keypair], port, max_pps, send_buffer_params);
    
    if let Some(other) = beam_to {
        let initiate_connections = vec![other];

        let ret = service_connections(&network_thread_handle2, NetworkThreadPush { initiate_connections, ..Default::default() });
        let mut wanted_connections = ret.current_connections;
    
        let mut i = 0;
    
        loop {
            let mut send_unreliable = Vec::new();
            if wanted_connections.len() > 0 {
                for _ in 0..50000 {
                    send_unreliable.push((wanted_connections[0].0.to_connection_key(), Vec::from(b"Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...Test data...")));
                }
                // the sends will be truncated anyway
            }
            let ret = service_connections(&network_thread_handle2, NetworkThreadPush { wanted_connections, send_unreliable, ..Default::default() });
            wanted_connections = ret.current_connections;
            std::thread::sleep(std::time::Duration::from_millis(3000));
        }
    }
    else {
        let mut wanted_connections = Vec::new();
        loop {
            let ret = service_connections(&network_thread_handle2, NetworkThreadPush { wanted_connections, ..Default::default() });
            wanted_connections = ret.current_connections;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

// LSB of this 48 bit value must be 1 for this to be recognized as an incoming connect handshake.
pub const CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s: u64 = 0x7193_c304_f8d5;
pub const CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b: u64 = 0xbe53_b364_1ce1;
pub const CONNECT_MAGIC1_PLAIN_TEXT: u64 = 0x5bb2_2856_ae53;
pub fn crypto_string_from_connect_magic1(magic: u64) -> Option<&'static str> {
    match magic {
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s => Some("Noise_IK_25519_ChaChaPoly_BLAKE2s"),
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b => Some("Noise_IK_25519_ChaChaPoly_BLAKE2b"),
        CONNECT_MAGIC1_PLAIN_TEXT => Some("plaintext"),
        _ => None,
    }
}

pub const MAGIC2_BLOCK_SIZE: usize = 1 + 32 * 8; // 257 bytes: 1 byte count + 32 × 8-byte magic2 values. Always send full block for constant-size.
pub const MAGIC2_APP_CROSSLINK: u64 = 0x9b21ac6a28ae93c0;
const SERVER_SUPPORTED_MAGIC2: &[u64] = &[MAGIC2_APP_CROSSLINK];

pub fn build_magic2_client_block() -> [u8; MAGIC2_BLOCK_SIZE] {
    let mut block = [0u8; MAGIC2_BLOCK_SIZE];
    block[0] = 1;
    store_u64(&mut block[1..9], MAGIC2_APP_CROSSLINK);
    block
}

/// Server picks the first client-offered magic2 that the server supports. Returns None if no match.
/// Also validates that all slots beyond `count` are zeroed.
pub fn negotiate_magic2(client_block: &[u8]) -> Option<u64> {
    if client_block.len() != MAGIC2_BLOCK_SIZE { return None; }
    let count = client_block[0] as usize;
    if count == 0 || count > 32 { return None; }
    // Validate that padding beyond the count is all zeros.
    for i in count..32 {
        let offset = 1 + i * 8;
        if load_u64(&client_block[offset..offset + 8]) != 0 { return None; }
    }
    for i in 0..count {
        let offset = 1 + i * 8;
        let magic2 = load_u64(&client_block[offset..offset + 8]);
        if SERVER_SUPPORTED_MAGIC2.contains(&magic2) {
            return Some(magic2);
        }
    }
    None
}

/// Client validates server's chosen magic2 was in the list we sent.
pub fn validate_magic2(magic2: u64) -> bool {
    let block = build_magic2_client_block();
    let count = block[0] as usize;
    for i in 0..count {
        let offset = 1 + i * 8;
        if load_u64(&block[offset..offset + 8]) == magic2 { return true; }
    }
    false
}

pub const fn total_packet_payload_overhead_from_connect_magic1_inside_udp_payload(magic: u64) -> Option<usize> {
    match magic {
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s => Some(6+16),
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b => Some(6+16),
        CONNECT_MAGIC1_PLAIN_TEXT => Some(6+0),
        _ => None,
    }
}

pub const MIN_SECRET_KEY_SIZE: usize = 32;
pub const MIN_PUBLIC_KEY_SIZE: usize = 32;

// Quantum-resilient key sizes
pub const MAX_SECRET_KEY_SIZE: usize = 8000;
pub const MAX_PUBLIC_KEY_SIZE: usize = 8000;

pub fn new_keypair_from_connect_magic1(magic1: u64) -> Option<IdentityKeyPair> {
    if magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
        let mut ret = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s)?;
        ret.magic1 = CONNECT_MAGIC1_PLAIN_TEXT;
        return Some(ret);
    }
    if let Some(crypto_string) = crypto_string_from_connect_magic1(magic1) {
        let kp = snow::Builder::new(crypto_string.parse().ok()?).generate_keypair().ok()?;
        Some(IdentityKeyPair { magic1, private: kp.private, public: kp.public })
    } else { None }
}

pub fn new_keypair_from_connect_magic1_with_seed(magic1: u64, seed: [u8; 32]) -> Option<IdentityKeyPair> {
    if magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
        let mut ret = new_keypair_from_connect_magic1_with_seed(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s, seed)?;
        ret.magic1 = CONNECT_MAGIC1_PLAIN_TEXT;
        return Some(ret);
    }
    if let Some(crypto_string) = crypto_string_from_connect_magic1(magic1) {
        use rand::SeedableRng;
        let mut rng = rand_chacha::ChaCha20Rng::from_seed(seed);
        let kp = snow::Builder::with_resolver(crypto_string.parse().ok()?, Box::new(SnowRngResolver { rng: RustIsBadRngWrapper(rng) })).generate_keypair().ok()?;
        Some(IdentityKeyPair { magic1, private: kp.private, public: kp.public })
    } else { None }
}

pub fn b64(s: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
}

pub fn fmt_magic1_pubkey_b64(magic1: u64, key: &Vec<u8>) -> String {
    let mut tmp = Vec::with_capacity(8 + key.len());
    tmp.extend_from_slice(&magic1.to_le_bytes()[..6]);
    tmp.extend_from_slice(&key);
    b64(&tmp)
}

#[derive(Clone, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct STPAddress {
    pub ip: Ipv6Addr,
    pub port: u16,
    pub magic1: u64,
    pub key: Vec<u8>,
}
impl STPAddress {
    pub fn is_ipv4(&self) -> bool { self.ip.to_ipv4_mapped().is_some() }
    pub fn is_ipv6(&self) -> bool { self.ip.to_ipv4_mapped().is_none() }
    pub fn from(ip: Ipv6Addr, port: u16, kp: &IdentityKeyPair) -> Self { Self { ip, port, magic1: kp.magic1, key: kp.public.clone() } }
    pub fn connection_key(&self) -> ConnectionKey { ConnectionKey::from(self) }

    // Parse [ip]:port:magic1:key
    pub fn parse(addr: &str) -> Option<STPAddress> {
        use base64::Engine;
        let     addr           = addr.strip_prefix('[')?;
        let     (ip_str, rest) = addr.split_once("]:")?;
        let     ip: Ipv6Addr   = ip_str.parse().ok()?;
        let mut parts          = rest.splitn(3, ':');
        let     port: u16      = parts.next()?.parse().ok()?;
        let     b64_magic1     = parts.next()?; if b64_magic1.contains('=') { return None; }
        let     b64_key        = parts.next()?; if b64_key   .contains('=') { return None; }
        let     magic1_bytes   = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64_magic1).ok()?; if magic1_bytes.len() != 6 { return None; }
        let     key            = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64_key).ok()?;    if key.len() < MIN_PUBLIC_KEY_SIZE || key.len() > MAX_PUBLIC_KEY_SIZE { return None; }
        let mut buf            = [0u8; 8]; buf[..6].copy_from_slice(&magic1_bytes);
        let     magic1         = u64::from_le_bytes(buf); if magic1 & 1 == 0 { return None; }
        Some(STPAddress { ip, port, magic1, key })
    }
    
    pub fn to_connection_key(&self) -> ConnectionKey { self.into() }
}
impl Default for STPAddress {
    fn default() -> Self {
        Self {
            ip:     Ipv6Addr::UNSPECIFIED,
            port:   0,
            magic1: 0,
            key:    Vec::default(),
        }
    }
}
impl std::fmt::Debug for STPAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "[{:?}]:{}:{}:{}", self.ip, self.port, b64(&self.magic1.to_le_bytes()[..6]), b64(&self.key))
    }
}


pub fn fmt_byte_str(f: &mut std::fmt::Formatter<'_>, bytes: &[u8]) -> std::fmt::Result {
    let n = usize::min(bytes.len(), f.precision().unwrap_or(bytes.len()));
    for i in 0..n { write!(f, "{:02x}", bytes[i])?; }
    Ok(())
}

pub fn fmt_byte_str_rev(f: &mut std::fmt::Formatter<'_>, bytes: &[u8]) -> std::fmt::Result {
    let n = usize::min(bytes.len(), f.precision().unwrap_or(bytes.len()));
    for i in 0..n { write!(f, "{:02x}", bytes[n-(i+1)])?; }
    Ok(())
}

pub fn fmt_prefixed_byte_str(f: &mut std::fmt::Formatter<'_>, pre: &str, bytes: &[u8]) -> std::fmt::Result {
    write!(f, "{}", pre)?;
    fmt_byte_str(f, bytes)
}

pub fn fmt_prefixed_byte_str_rev(f: &mut std::fmt::Formatter<'_>, pre: &str, bytes: &[u8]) -> std::fmt::Result {
    write!(f, "{}", pre)?;
    fmt_byte_str_rev(f, bytes)
}

impl std::fmt::Debug for IdentityKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        format!("IdentityKeyPair {{ magic1: {}, private: \"", self.magic1);
        fmt_byte_str(f, &self.private)?;
        fmt_prefixed_byte_str(f, "\", public: \"",                &self.public)?;
        write!(f, "\" }}")
    }
}

// TODO: rename private key to secret key
#[derive(/* Copy, */ Clone, PartialEq, Eq, Hash)]
pub struct IdentityKeyPair {
    pub magic1: u64,
    pub private: Vec<u8>, // [u8; MAX_SECRET_KEY_SIZE],
    pub public:  Vec<u8>, // [u8; MAX_PUBLIC_KEY_SIZE],
}
// const_assert!(size_of::<IdentityKeyPair>().is_power_of_two());
impl std::fmt::Display for IdentityKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "magic1: {} sk: REDACTED pk: {}", b64(&self.magic1.to_le_bytes()[..6]), b64(&self.public))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionKey {
    pub ip: Ipv6Addr,
    pub port: u16,
    pub key_15_bits: u16, // LSB is just always 1.
}
impl ConnectionKey {
    pub fn is_ipv4(&self) -> bool { self.ip.to_ipv4_mapped().is_some() }
    pub fn is_ipv6(&self) -> bool { self.ip.to_ipv4_mapped().is_none() }
}
impl From<&STPAddress> for ConnectionKey {
    fn from(a: &STPAddress) -> Self { Self { ip: a.ip, port: a.port, key_15_bits: load_u16(&a.key[0..2]) << 1 } }
}
impl Default for ConnectionKey {
    fn default() -> Self { Self { ip: Ipv6Addr::UNSPECIFIED, port: 0, key_15_bits: 0 } }
}

impl SliceWrite for ConnectionKey {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.ip.octets().write_to(&mut buf[o..]);
        o += self.port       .write_to(&mut buf[o..]);
        o += self.key_15_bits.write_to(&mut buf[o..]);
        o
    }
}
impl SliceRead for ConnectionKey {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self {
            ip:          Ipv6Addr::from(<[u8; 16]>::read_from(buf)?),
            port:        SliceRead::read_from(buf)?,
            key_15_bits: SliceRead::read_from(buf)?,
        })
    }
}

#[derive(Debug)]
pub struct UnreliableSendBuffer {
    // A time based capacity constrained by byte counts.
    // The queue should for example probably have a minimum byte size twice that of your largest message.
    pub target_queue_size_us: u32,
    pub min_queue_size_bytes: u32,
    pub max_queue_size_bytes: u32,
    
    pub queue_bytes_filled: usize,
    pub message_queue: VecDeque<Vec<u8>>,
    pub partial_send_slot: Option<(Vec<u8>, u32)>,
    pub send_package_id: u16,
    pub current_bytes_per_second: u64,
}

impl UnreliableSendBuffer {
    pub fn new(target_queue_size_us: u32, min_queue_size_bytes: u32, max_queue_size_bytes: u32) -> Self {
        Self {
            target_queue_size_us,
            min_queue_size_bytes,
            max_queue_size_bytes,
            queue_bytes_filled: 0,
            message_queue: VecDeque::new(),
            partial_send_slot: None,
            send_package_id: 0,
            current_bytes_per_second: 0,
        }
    }

    #[inline]
    pub fn current_byte_capacity(&self) -> u32 {
        let dynamic_target = self.target_queue_size_us as u64 * self.current_bytes_per_second / 1000_000;
        dynamic_target.max(self.min_queue_size_bytes as u64).min(self.max_queue_size_bytes as u64) as u32
    }
    
    pub fn submit_messages(&mut self, mut new_messages: Vec<Vec<u8>>) {
        use rand::seq::SliceRandom;
        new_messages.shuffle(&mut rand::thread_rng());
        let mut new_bytes = 0;
        for m in &new_messages { new_bytes += m.len(); }
        self.message_queue.extend(new_messages);
        self.queue_bytes_filled += new_bytes;
        self.cut_to_size();
    }
    
    pub fn cut_to_size(&mut self) {
        while self.queue_bytes_filled > self.current_byte_capacity() as usize {
            let Some(m) = self.message_queue.pop_front() else { break; };
            self.queue_bytes_filled -= m.len();
        }
    }
    
    pub fn has_unsent_data(&self) -> bool {
        if !self.message_queue.is_empty() { return true; }
        if let Some((msg, start_byte)) = &self.partial_send_slot {
            if (*start_byte as usize) < msg.len() { return true; }
        }
        false
    }

    pub fn try_get_fragment(&mut self, frag_size: usize) -> Option<(bool, u16, &[u8], u32)> {
        if let Some((msg, start_byte)) = &self.partial_send_slot {
            if *start_byte as usize >= msg.len() {
                let msg_len = msg.len();
                self.queue_bytes_filled -= msg_len;
                self.partial_send_slot = None;
                self.send_package_id = self.send_package_id.wrapping_add(1);
            }
        }
        if self.partial_send_slot.is_none() {
            if let Some(front_msg) = self.message_queue.pop_front() {
                self.partial_send_slot = Some((front_msg, 0));
            }
            else { return None; }
        }
        let (msg, start_byte) = self.partial_send_slot.as_mut().unwrap();
        let start = *start_byte as usize;
        let ret_size = (msg.len() - start).min(frag_size);
        // Both frag_size and msg.len() may be zero.

        *start_byte += ret_size as u32;
        Some((*start_byte as usize == msg.len(), self.send_package_id, &msg[start..start + ret_size], start as u32))
    }
}

fn fill_packet_payload_with_unreliable_fragments(payload: &mut [u8], unreliable_send_buffer: &mut UnreliableSendBuffer) -> bool {
    let mut send = false;
    let mut cursor = 0;
    while cursor + 8 <= payload.len() && let Some((is_fin, package_id, frag_data, frag_offset)) = unreliable_send_buffer.try_get_fragment(payload.len() - 8 - cursor) {
        send = true;
        let header = 0 | ((is_fin as u64) << 1) | ((package_id as u64) << 2) | ((frag_data.len() as u64 & 0x3fff) << 18) | ((frag_offset as u64) << 32);
        store_u64(&mut payload[cursor..cursor+8], header);
        frag_data.write_to(&mut payload[cursor+8..]);
        cursor += 8+frag_data.len();
    }
    if send == false { return false; }
    while cursor < payload.len() {
        payload[cursor] = 0xff;
        cursor += 1;
    }
    return true;
}

#[derive(Debug)]
pub struct ConnectionTrackingData {
    pub creation_time_ns: u64,
    pub my_ip: Ipv6Addr, // TODO: use an ipv6 type that supports Default!
    pub my_transport_identity_keypair: IdentityKeyPair,
    pub two_byte_send_prefix: u16,
    pub other_ip: Ipv6Addr, // TODO: use an ipv6 type that supports Default!
    pub other_port: u16,
    pub other_transport_identity: Vec<u8>,
    pub connection_state: ConnectionState,
    pub jumbo_reassembly: JumboReassembly,
    pub handshake_hash: [u8; 64],
    // pub reliable_streams: ReliableStreams,
    
    pub unreliable_send_buffer: UnreliableSendBuffer,
    pub unreliable_reassembly: Vec<ReassemblySlot>,
    
    pub nym_sock: Option<NymSockHandle>,
}
impl ConnectionTrackingData {
    pub fn address(&self) -> STPAddress {
        STPAddress {
            ip:     self.other_ip,
            port:   self.other_port,
            magic1: self.my_transport_identity_keypair.magic1,
            key:    self.other_transport_identity.clone()
        }
    }
    pub fn is_connected(&self) -> bool { self.connection_state.is_connected() }
}


#[derive(Debug, Clone)]
pub struct ConnectionCipherTriplet {
    pub old: snow::StatelessTransportState,
    pub current: snow::StatelessTransportState,
    pub new: snow::StatelessTransportState,
}
impl ConnectionCipherTriplet {
    pub fn new_from_old_init_only(mut old: snow::StatelessTransportState) -> Self {
        old.rekey_outgoing(); // target the other side's current
        let mut current = old.clone();
        current.rekey_incoming();
        let mut new = current.clone();
        new.rekey_incoming();
        Self { old, current, new }
    }
    pub fn ratchet_forward_incoming(&mut self) {
        self.old.rekey_incoming();
        self.current.rekey_incoming();
        self.new.rekey_incoming();
    }
    pub fn advance_outgoing(&mut self) {
        self.old.rekey_outgoing(); // This is a bit redundant since they are all the same.
        self.current.rekey_outgoing();
        self.new.rekey_outgoing();
    }
}

#[derive(Debug)]
pub enum ConnectionState {
    SendingClientHelloPlaintext {
        last_sent_time_ns: u64,
        hello_packet_payload: Vec<u8>,
    },
    SendingClientHello {
        magic1: u64,

        last_sent_time_ns: u64,
        hello_packet_payload: Vec<u8>,
        handshake: snow::HandshakeState,
    },
    SendingServerHelloPlaintext {
        magic2: u64,
        last_sent_time_ns: u64,
        hello_packet_payload: Vec<u8>,
    },
    SendingServerHello {
        cipher: ConnectionCipherTriplet,
        magic1: u64,
        magic2: u64,

        last_sent_time_ns: u64,
        hello_packet_payload: Vec<u8>,
    },
    Connected(ConnectionStateConnected),
}

#[derive(Debug)]
pub struct ConnectionStateConnected {
    pub cipher: Option<ConnectionCipherTriplet>,
    pub magic1: u64,
    pub magic2: u64,

    pub send_sequence_number: u64,
    pub jumbogram_index: u32,
    pub last_sent_data_packet: u64,
    pub last_sent_could_have_sent_data_packet: u64,
    pub last_ack_received_time: u64,
    pub last_status_print_time: u64,
    pub packet_since_last_print: u64,
    pub packet_lost_since_last_print: u64,
    pub max_in_flight_since_last_print: u64,

    pub ack_field: AckField,
    pub ack_timer: u64,

    // Note(Sam): HARD ASSUMPTION that no link ever has a greater RTT than 4.295 seconds.
    pub send_time_band: [u64; 1024], // 1024 buckets of 4.194 ms giving 4.295 seconds of tracking.
    pub send_time_band_head_index: u64, // monotonic time ns / 4_194_304 aka 0x400000
    
    pub RTT_sample_sums: [u32; 16],   // sum of RTT measurements per ack, in 0.1 ms units
    pub RTT_sample_counts: [u16; 16], // count of RTT measurements per ack
    pub RTT_sample_cursor: u64,
    pub RTT_mean: u16,

    pub ack_arrival_times: [u64; 16], // timestamp_ns of each ack arrival, ring buffer
    pub ack_pace_estimate: u16,       // median inter-ack gap in 0.1 ms units, clamped to RTT_mean*2
    
    pub packets_waiting_ack_field: [u64; 2048],
    // head is the send_sequence_number
    pub packets_waiting_ack_tail: u64,
    
    pub current_tu: u64,
    pub last_sent_tu_probe_time_ns: u64,
    pub tu_probe_sequence_number: u64,
    pub tu_probe_size: u64,
    pub tu_probe_size_advance: u64,
    pub tu_probe_failed_count: u64,
    
    pub packets_in_flight: u64,
    pub send_pacer_acc_ns: u64,
    pub congestion_event_time_ns: u64,
    pub congestion_event_rate_upps: u64,
    pub loss_window_start_ns: u64,
    pub loss_count_in_window: u64,
    pub is_app_limited: bool,
    pub app_limit_time_offset: u64,
    pub ecn_congestion_received: bool,
    pub ecn_oldest_send_time: u64,
}
pub fn new_connection_state_connected(cipher: Option<ConnectionCipherTriplet>, magic1: u64, magic2: u64, timestamp_ns: u64) -> ConnectionState {
    ConnectionState::Connected(ConnectionStateConnected {
        cipher,
        magic1,
        magic2,
        send_sequence_number: 0,
        jumbogram_index: 0,
        last_sent_data_packet: 0,
        last_sent_could_have_sent_data_packet: 0,
        last_ack_received_time: timestamp_ns,
        last_status_print_time: timestamp_ns,
        packet_since_last_print: 0,
        packet_lost_since_last_print: 0,
        max_in_flight_since_last_print: 0,
        ack_field: Default::default(),
        ack_timer: u64::MAX,
        send_time_band: [0; 1024],
        send_time_band_head_index: 0,
        RTT_sample_sums: [0; 16],
        RTT_sample_counts: [0; 16],
        RTT_sample_cursor: 0,
        RTT_mean: u16::MAX,
        ack_arrival_times: [0; 16],
        ack_pace_estimate: 0,
        packets_waiting_ack_field: [0; 2048],
        packets_waiting_ack_tail: 0,
        
        current_tu: ASSUMED_UDP_PAYLOAD_SIZE_WITH_GUARANTEED_DELIVERY as u64,
        last_sent_tu_probe_time_ns: 0,
        tu_probe_sequence_number: u64::MAX,
        tu_probe_size: 0,
        tu_probe_size_advance: 0,
        tu_probe_failed_count: 0,
        
        packets_in_flight: 0,
        send_pacer_acc_ns: 0,
        congestion_event_time_ns: 0,
        congestion_event_rate_upps: 0,
        loss_window_start_ns: 0,
        loss_count_in_window: 0,
        is_app_limited: false,
        app_limit_time_offset: 0,
        ecn_congestion_received: false,
        ecn_oldest_send_time: 0,
    })
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool { if let ConnectionState::Connected(_) = self { true } else { false } }
}

pub fn get_send_time_for_sequence_number(sequence_number: u64, send_time_band: &[u64; 1024], send_time_band_head_index: u64) -> u64 {
    if sequence_number >= send_time_band[(send_time_band_head_index % 1024) as usize] { return u64::MAX; }
    
    let mut cursor = send_time_band_head_index;
    while cursor > send_time_band_head_index.saturating_sub(1023) {
        if send_time_band[((cursor-1) % 1024) as usize] <= sequence_number && sequence_number < send_time_band[(cursor % 1024) as usize] {
            return cursor * 0x400000;
        }
        cursor -= 1;
    }
    0 // failed lookup
}

pub fn increment_sequence_number_and_account(send_time_ns: u64, send_sequence_number: &mut u64, send_time_band: &mut [u64; 1024], send_time_band_head_index: &mut u64, packets_waiting_ack_field: &mut [u64; 2048]) {
    {
        let bit_index = *send_sequence_number % (2048*64);
        packets_waiting_ack_field[(bit_index / 64) as usize] |= 1u64 << (bit_index % 64);
    }

    let new_head_index = send_time_ns / 0x400000;
    if new_head_index >= *send_time_band_head_index + 1023 {
        let current = *send_sequence_number;
        for i in 0..1024 { send_time_band[i] = current; }
        
        send_time_band[(new_head_index % 1024) as usize] = current + 1;
        *send_time_band_head_index = new_head_index;
        *send_sequence_number = current + 1;
        return;
    }
    if new_head_index > *send_time_band_head_index {
        let current = *send_sequence_number;
        // this loop does not run for the single increment case. Only jumps over slots.
        for i in *send_time_band_head_index+1..new_head_index { send_time_band[(i % 1024) as usize] = current; }

        send_time_band[(new_head_index % 1024) as usize] = current + 1;
        *send_time_band_head_index = new_head_index;
        *send_sequence_number = current + 1;
        return;
    }
    // equal
    *send_sequence_number += 1;
    send_time_band[(new_head_index % 1024) as usize] = *send_sequence_number;
}

/* Because rust is bad, packed is the only way for us to remove any alignment requirements and assumptions. */
#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct AckField {
    pub field_base: u32,
    pub field: [u64; 64], // Each entry is 1 bit. 4096 entries total.
}

impl Default for AckField { fn default() -> Self { Self { field_base: 0, field: [0u64; 64], } } }

impl AckField {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                self as *const AckField as *const u8,
                core::mem::size_of::<AckField>(),
            )
        }
    }
}

pub fn get_connected<'a>(m: &'a HashMap::<ConnectionKey, ConnectionTrackingData>, k: &ConnectionKey) -> Option<&'a ConnectionTrackingData> {
    let connection = m.get(k)?;
    if !connection.is_connected() { return None; }
    Some(connection)
}
pub fn get_connected_mut<'a>(m: &'a mut HashMap::<ConnectionKey, ConnectionTrackingData>, k: &ConnectionKey) -> Option<&'a mut ConnectionTrackingData> {
    let connection = m.get_mut(k)?;
    if !connection.is_connected() { return None; }
    Some(connection)
}

pub fn allocate_jumbogram_id(connections_map: &mut HashMap<ConnectionKey, ConnectionTrackingData>, key: &ConnectionKey) -> Option<u32> {
    let conn = connections_map.get_mut(key)?;
    if let ConnectionState::Connected(state) = &mut conn.connection_state {
        let id = state.jumbogram_index;
        state.jumbogram_index = state.jumbogram_index.wrapping_add(1) & (MAX_JUMBOGRAM_IDS - 1);
        Some(id)
    } else {
        None
    }
}

pub fn connection_state_string(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::SendingClientHelloPlaintext { .. } => { return "SendingClientHelloPlaintext"; },
        ConnectionState::SendingClientHello          { .. } => { return "SendingClientHello"; },
        ConnectionState::SendingServerHelloPlaintext { .. } => { return "SendingServerHelloPlaintext"; },
        ConnectionState::SendingServerHello          { .. } => { return "SendingServerHello"; },
        ConnectionState::Connected(_)                        => { return "Connected"; },
    };
    return "<INVALID>";
}

pub fn connect_to_endpoint(
    connections_map: &mut HashMap::<ConnectionKey, ConnectionTrackingData>,
    my_connect_keypair: &IdentityKeyPair,
    endpoint: &STPAddress,
    nym_sock: Option<NymSockHandle>,
    send_buffer_params: (u32, u32, u32),
) {
    assert!(my_connect_keypair.magic1 == endpoint.magic1);

    if my_connect_keypair.magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
        let magic2_block = build_magic2_client_block();
        let mut hello_packet_payload = vec![0u8; 6 + 32 + MAGIC2_BLOCK_SIZE];
        store_u48(&mut hello_packet_payload[0..6], my_connect_keypair.magic1);
        assert_eq!(my_connect_keypair.public.len(), 32);
        hello_packet_payload[6..6+32].copy_from_slice(&my_connect_keypair.public[..]);
        hello_packet_payload[6+32..6+32+MAGIC2_BLOCK_SIZE].copy_from_slice(&magic2_block);

        let my_ip = if endpoint.is_ipv4() { Ipv6Addr::UNSPECIFIED } else { Ipv6Addr::UNSPECIFIED };
        connections_map.insert(
            ConnectionKey::from(endpoint),
            ConnectionTrackingData {
                creation_time_ns: monotonic_clock_ns(),
                my_ip,
                my_transport_identity_keypair: my_connect_keypair.clone(),
                two_byte_send_prefix: load_u16(&my_connect_keypair.public[0..2]) << 1,
                other_ip: endpoint.ip,
                other_port: endpoint.port,
                other_transport_identity: endpoint.key.clone(),
                connection_state: ConnectionState::SendingClientHelloPlaintext { last_sent_time_ns: 0, hello_packet_payload },
                jumbo_reassembly: Default::default(),
                handshake_hash: [0u8; 64],
                unreliable_send_buffer: UnreliableSendBuffer::new(send_buffer_params.0, send_buffer_params.1, send_buffer_params.2),
                unreliable_reassembly: ReassemblySlot::new_ring(1024),
                nym_sock,
            },
        );
    }
    else {
        let magic2_block = build_magic2_client_block();
        let mut hello_packet_payload = vec![0u8; 1024];
        store_u48(&mut hello_packet_payload[0..6], my_connect_keypair.magic1);
        let mut handshake = snow::Builder::new(crypto_string_from_connect_magic1(my_connect_keypair.magic1).unwrap().parse().unwrap())
            .prologue(&hello_packet_payload[0..6]).unwrap()
            .local_private_key(&my_connect_keypair.private[..]).unwrap()
            .remote_public_key(&endpoint.key[..]).unwrap()
            .build_initiator().unwrap();

        let handshake_size = handshake.write_message(&magic2_block, &mut hello_packet_payload[6..]).unwrap();
        hello_packet_payload.truncate(6+handshake_size);
        hello_packet_payload.shrink_to_fit();

        let my_ip = if endpoint.is_ipv4() { Ipv6Addr::UNSPECIFIED } else { Ipv6Addr::UNSPECIFIED };
        connections_map.insert(
            ConnectionKey::from(endpoint),
            ConnectionTrackingData {
                creation_time_ns: monotonic_clock_ns(),
                my_ip,
                my_transport_identity_keypair: my_connect_keypair.clone(),
                two_byte_send_prefix: load_u16(&my_connect_keypair.public[0..2]) << 1,
                other_ip: endpoint.ip,
                other_port: endpoint.port,
                other_transport_identity: endpoint.key.clone(),
                connection_state: ConnectionState::SendingClientHello { magic1: my_connect_keypair.magic1, last_sent_time_ns: 0, hello_packet_payload, handshake },
                jumbo_reassembly: Default::default(),
                handshake_hash: [0u8; 64],
                unreliable_send_buffer: UnreliableSendBuffer::new(send_buffer_params.0, send_buffer_params.1, send_buffer_params.2),
                unreliable_reassembly: ReassemblySlot::new_ring(1024),
                nym_sock,
            },
        );
    }
}

const        VERBOSE                :bool=0!=                (1);
const OVERLY_VERBOSE                :bool=0!=                (0);

macro_rules! pod { ($($item:item)*) => { $(#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)] $item)* }; }

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReassemblySlot {
    pub buf: Vec<u8>,
    pub total_len: Option<u32>,
    pub received: Vec<(u32, u32)>, // sorted non-overlapping non-adjacent ranges: [start, end)
}

impl ReassemblySlot {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            total_len: None,
            received: Vec::new(),
        }
    }
    
    pub fn new_ring(size: usize) -> Vec<ReassemblySlot> {
        let mut vec = Vec::with_capacity(size);
        for _ in 0..size { vec.push(Self::new()); }
        vec
    }

    /// returns (success, is_complete)
    pub fn insert(&mut self, offset: usize, data: &[u8], is_fin: bool) -> (bool, bool) {

        let start = offset as u32;
        let end = (offset + data.len()) as u32;

        // If this is the final fragment, derive total_len from offset + data.len()
        if is_fin {
            if let Some(existing) = self.total_len {
                if existing != end { return (false, false); }
            } else {
                if let Some(&(_, last_end)) = self.received.last() {
                    if last_end > end { return (false, false); }
                }
                self.total_len = Some(end);
                self.buf.resize(end as usize, 0);
            }
        }

        // Empty fragment — no data to insert, bounds check offset then check completion
        if data.is_empty() {
            if let Some(tl) = self.total_len {
                if start > tl { return (false, false); }
            }
            let complete = self.total_len.is_some_and(|tl| self.received.len() == 1 && self.received[0] == (0, tl));
            return (true, complete);
        }

        // Bounds check against total_len if known
        if let Some(tl) = self.total_len {
            if start >= tl || end > tl { return (false, false); }
        }

        // Grow buf if needed (total_len not yet known)
        if end as usize > self.buf.len() {
            self.buf.resize(end as usize, 0);
        }

        // Find all ranges that overlap or are adjacent to [start, end)
        let first = self.received.partition_point(|&(_, e)| e < start);
        let last = self.received.partition_point(|&(s, _)| s <= end);

        // Verify overlapping bytes match
        for &(rs, re) in &self.received[first..last] {
            let ov_start = start.max(rs) as usize;
            let ov_end = end.min(re) as usize;
            if ov_start < ov_end {
                let data_off = ov_start - offset;
                if self.buf[ov_start..ov_end] != data[data_off..data_off + (ov_end - ov_start)] {
                    return (false, false);
                }
            }
        }

        // Write new data (overlapping parts verified equal)
        self.buf[offset..offset + data.len()].copy_from_slice(data);

        // Merge: union of [start, end) with all overlapping/adjacent ranges
        let merged_start = if first < last { self.received[first].0.min(start) } else { start };
        let merged_end = if first < last { self.received[last - 1].1.max(end) } else { end };
        self.received.splice(first..last, std::iter::once((merged_start, merged_end)));

        let complete = self.total_len.is_some_and(|tl| self.received.len() == 1 && self.received[0] == (0, tl));
        (true, complete)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JumboReassembly {
    slots: HashMap<u32, ReassemblySlot>,
}

pub const MAX_REASSEMBLY_SLOTS: usize = 128; // @Todo: convert max slots into max bytes instead -- which is what we really want anyway!
pub const MAX_JUMBOGRAM_LEN: usize = 1 << 23; // 8 MB, matches the 23-bit field
pub const MAX_JUMBOGRAM_IDS: u32   = 1 << 18; // matches 18-bit field

pub const ACK_BUFFER_TIME_NS: u64 = 50_000_000;

#[derive(Default)]
pub struct NetworkThreadPush {
    pub initiate_connections: Vec<STPAddress>,
    pub wanted_connections: Vec<(STPAddress, [u8; 64])>,
    pub send_unreliable: Vec<(ConnectionKey, Vec<u8>)>,
}

#[derive(Default)]
pub struct NetworkThreadPull {
    pub current_connections: Vec<(STPAddress, [u8; 64])>,
    pub received_unreliable_messages: Vec<(ConnectionKey, Vec<u8>)>,
}

struct NetworkThreadInner {
    state: std::sync::atomic::AtomicUsize, // 0 empty, 1 full
    push: std::cell::UnsafeCell<NetworkThreadPush>,
    pull: std::cell::UnsafeCell<NetworkThreadPull>,
}

#[allow(unsafe_code)]
unsafe impl std::marker::Sync for NetworkThreadInner {}

// Note(Sam): Using this handle from two threads at once is UB. Only one thread may call service_connections at any given time.
pub struct NetworkThreadHandle {
    inner: std::sync::Arc<NetworkThreadInner>,
    thread: std::thread::JoinHandle<()>,
}

pub fn new_network_thread(my_keypairs: Vec<IdentityKeyPair>, my_port: u16, max_pps: Option<u64>, send_buffer_params: (u32, u32, u32)) -> NetworkThreadHandle {

    // STP setup
    socket_setup();
    monotonic_clock_setup();

    let socket = setup_and_bind_udp_socket(my_port).expect("Failed to bind socket, try again.");
    
    // Nym setup
    let nym_handle = nym_setup();

    let inner = std::sync::Arc::new(NetworkThreadInner {
        state: std::sync::atomic::AtomicUsize::new(0),
        push: std::cell::UnsafeCell::new(NetworkThreadPush::default()),
        pull: std::cell::UnsafeCell::new(NetworkThreadPull::default()),
    });

    let thread_inner = inner.clone();

    let thread = std::thread::spawn(move || {

        let mut packet_memory_encrypted = new_packet_memory(); // Incoming Encrypted / Outgoing Encrypted
        let mut packet_memory_recv      = new_packet_memory(); // Incoming Decrypted
        let mut packet_memory_send      = new_packet_memory(); // Outgoing Decrypted

        let mut received_unreliable_messages: Vec<(ConnectionKey, Vec<u8>)> = Vec::new();

        let mut connections_map = HashMap::<ConnectionKey, ConnectionTrackingData>::new();
        let mut server_nym_sockets: Vec<NymSockHandle> = Vec::new();

        loop {
            if thread_inner.state.load(std::sync::atomic::Ordering::Acquire) == 1 {
                let mut req = NetworkThreadPush::default();

                #[allow(unsafe_code)]
                unsafe {
                    std::mem::swap(&mut *thread_inner.push.get(), &mut req);
                }

                let current_time_now_ns = monotonic_clock_ns();
                connections_map.retain(|key, value| {
                    let stp_address = value.address();
                    if value.creation_time_ns + 30_000_000_000 < current_time_now_ns && req.wanted_connections.iter().position(|(x, _)| x == &stp_address).is_none() && value.is_connected() {
                        println!("############################# KILLING CONNECTION {:?}", value);
                        return false;
                    }
                    true
                });
                for w in &req.initiate_connections {
                    if my_keypairs.iter().any(|kp| kp.public == w.key) {
                        continue;
                    }
                    let key = ConnectionKey {
                        ip: w.ip,
                        port: w.port,
                        key_15_bits: load_u16(&w.key[0..2]) << 1
                    };
                    if connections_map.contains_key(&key) {
                        continue;
                    }
                    for keypair in &my_keypairs {
                        if w.magic1 == keypair.magic1 {
                            //connect_to_endpoint(&mut connections_map, keypair, w, Some(new_nym_sock(&nym_handle)));
                            connect_to_endpoint(&mut connections_map, keypair, w, None, send_buffer_params);
                            break;
                        }
                    }
                }
                
                // attempt to push unreliable messages to queue, batched per connection
                req.send_unreliable.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                let mut i = 0;
                while i < req.send_unreliable.len() {
                    let key = req.send_unreliable[i].0;
                    let mut j = i;
                    while j < req.send_unreliable.len() && req.send_unreliable[j].0 == key { j += 1; }
                    let mut batch = Vec::with_capacity(j - i);
                    while i < j {
                        batch.push(std::mem::take(&mut req.send_unreliable[i].1));
                        i += 1;
                    }
                    if let Some(tracking_data) = connections_map.get_mut(&key) {
                        tracking_data.unreliable_send_buffer.submit_messages(batch);
                    }
                }

                let mut resp = NetworkThreadPull::default();
                resp.current_connections = connections_map.iter().filter_map(|(key, value)| {
                    if value.is_connected() {
                        Some((value.address().clone(), value.handshake_hash))
                    } else {
                        None
                    }
                }).collect();
                std::mem::swap(&mut resp.received_unreliable_messages, &mut received_unreliable_messages);

                #[allow(unsafe_code)]
                unsafe {
                    std::mem::swap(&mut *thread_inner.pull.get(), &mut resp);
                }

                thread_inner.state.store(0, std::sync::atomic::Ordering::Release);
            } else {
                
                /*  Note(Sam): Scheduling is very important for fairness and good performance under load
                    without strange bugs. Here is the strategy we will use even though it may seem CPU
                    expensive. Try to receive, then loop all connections and try to send for each of them.
                    We go to sleep if everything failed. Because we loop over all connections the calculation
                    for if we can send or not must be very cheap.
                */
                
                let mut should_sleep = true;
                
//////// BEGIN RECEIVE ////////////////////////////////////////////////////////////////////////
                // Gather and deduplicate all nym sockets (server + per-connection)
                let mut nym_recv_sockets: Vec<NymSockHandle> = Vec::new();
                for s in &server_nym_sockets {
                    nym_recv_sockets.push(s.clone());
                }
                for (_, ctd) in &connections_map {
                    if let Some(ns) = &ctd.nym_sock {
                        if !nym_recv_sockets.iter().any(|existing| std::sync::Arc::ptr_eq(&existing.shared, &ns.shared)) {
                            nym_recv_sockets.push(ns.clone());
                        }
                    }
                }

                for recv_source_i in 0..1 + nym_recv_sockets.len() {
                    let (buf_len, other_ip_addr, other_port, ecn_marked, ecn_enabled, service_class, timestamp_ns) = if recv_source_i == 0 {
                        match udp_recv_with_congestion_and_dscp(socket, &mut packet_memory_encrypted[..]) {
                            Ok(r) => r,
                            Err(_) => continue,
                        }
                    } else {
                        match nym_udp_recv_with_congestion_and_dscp(&nym_recv_sockets[recv_source_i - 1], &mut packet_memory_encrypted[..]) {
                            Ok(r) => r,
                            Err(_) => continue,
                        }
                    };
                    if buf_len < 6 { continue; }
                    
                    should_sleep = false;
                    let first_six_bytes = load_u48(&packet_memory_encrypted[0..6]);
                    
                    
                    if first_six_bytes & 1 != 0 { // Client Hello
                        let magic1 = first_six_bytes;
                        if magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
                            for key_i in 0..my_keypairs.len() {
                                let my_kp = &my_keypairs[key_i];
                                if my_kp.magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
                                    if buf_len >= 6 + 32 + MAGIC2_BLOCK_SIZE {
                                        let client_key = &packet_memory_encrypted[6..6+32];
                                        let magic2_block = &packet_memory_encrypted[6+32..6+32+MAGIC2_BLOCK_SIZE];
                                        let Some(chosen_magic2) = negotiate_magic2(magic2_block) else {
                                            // if OVERLY_VERBOSE { println!("Did NOT respond to connection: magic2 negotiation failed."); }
                                            break;
                                        };
        
                                        let connection_key = ConnectionKey { ip: other_ip_addr, port: other_port, key_15_bits: load_u16(&client_key[0..2]) << 1 };
                                        if let Some(existing_connection) = connections_map.get_mut(&connection_key) {
                                            if &existing_connection.my_transport_identity_keypair == my_kp && existing_connection.other_transport_identity == client_key {
                                                if let ConnectionState::SendingClientHelloPlaintext { last_sent_time_ns, hello_packet_payload } = &mut existing_connection.connection_state {
                                                    if *client_key < *my_kp.public {
                                                        store_u48(&mut packet_memory_send[0..6], 0xffff_ffff_0000 | (load_u16(&my_kp.public[0..2]) << 1) as u64);
                                                        packet_memory_send[6..6+32].copy_from_slice(&my_kp.public[..]);
                                                        store_u64(&mut packet_memory_send[6+32..6+32+8], chosen_magic2);
                                                        let hello_packet_payload = Vec::from(&packet_memory_send[0..6+32+8]);
        
                                                        existing_connection.connection_state = ConnectionState::SendingServerHelloPlaintext { magic2: chosen_magic2, last_sent_time_ns: 0, hello_packet_payload };
                                                        if OVERLY_VERBOSE { println!("Transitioned connection {} to SendingServerHelloPlaintext.", connection_key.key_15_bits); }
                                                    } else {
                                                        if OVERLY_VERBOSE { println!("Did NOT respond to connection {}: Lost the lexicographic compare.", connection_key.key_15_bits); }
                                                    }
                                                } else {
                                                    let state_str = connection_state_string(&existing_connection.connection_state);
                                                    if OVERLY_VERBOSE { println!("Did NOT respond to connection {}: We were not in SendingClientHelloPlaintext state. We are in {} state.", connection_key.key_15_bits, state_str); }
                                                }
                                            } else {
                                                if OVERLY_VERBOSE { println!("Did NOT respond to connection {}: Failed condition \"&existing_connection.my_transport_identity_keypair == my_kp && existing_connection.other_transport_identity == client_key\"", connection_key.key_15_bits); }
                                            }
                                        }
                                        else {
                                            let client_key = Vec::from(client_key);
                                            store_u48(&mut packet_memory_send[0..6], 0xffff_ffff_0000 | (load_u16(&my_kp.public[0..2]) << 1) as u64);
                                            packet_memory_send[6..6+32].copy_from_slice(&my_kp.public[..]);
                                            store_u64(&mut packet_memory_send[6+32..6+32+8], chosen_magic2);
                                            let hello_packet_payload = Vec::from(&packet_memory_send[0..6+32+8]);
        
                                            let my_ip = if other_ip_addr.to_ipv4_mapped().is_some() { Ipv6Addr::UNSPECIFIED } else { Ipv6Addr::UNSPECIFIED };
                                            connections_map.insert(
                                                connection_key,
                                                ConnectionTrackingData {
                                                    creation_time_ns: monotonic_clock_ns(),
                                                    my_ip,
                                                    my_transport_identity_keypair: my_kp.clone(),
                                                    two_byte_send_prefix: load_u16(&my_kp.public[0..2]) << 1,
                                                    other_ip: other_ip_addr,
                                                    other_port: other_port,
                                                    other_transport_identity: client_key,
                                                    connection_state: ConnectionState::SendingServerHelloPlaintext { magic2: chosen_magic2, last_sent_time_ns: 0, hello_packet_payload },
                                                    jumbo_reassembly: Default::default(),
                                                    handshake_hash: [0u8; 64],
                                                    unreliable_send_buffer: UnreliableSendBuffer::new(send_buffer_params.0, send_buffer_params.1, send_buffer_params.2),
                                                    unreliable_reassembly: ReassemblySlot::new_ring(1024),
                                                    nym_sock: None,
                                                },
                                            );
                                            if OVERLY_VERBOSE { println!("Transitioned connection {} to SendingServerHelloPlaintext.", connection_key.key_15_bits); }
                                        }
                                    } else {
                                        if OVERLY_VERBOSE { println!("Did NOT respond to connection: The packet was too small (expected {} bytes, got {}).", 6 + 32 + MAGIC2_BLOCK_SIZE, buf_len); }
                                    }
                                }
                            }
                        }
                        else if let Some(crypto_string) = crypto_string_from_connect_magic1(magic1) {
                            for key_i in 0..my_keypairs.len() {
                                let my_kp = &my_keypairs[key_i];
                                if my_kp.magic1 == magic1 {
                                    let mut new_handshake = snow::Builder::new(crypto_string.parse().unwrap())
                                        .prologue(&packet_memory_encrypted[0..6]).unwrap()
                                        .local_private_key(&my_kp.private).unwrap()
                                        .build_responder().unwrap();
                                    let read_message_maybe = new_handshake.read_message(&packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]);
                                    if let Ok(magic2_block_len) = read_message_maybe {
                                        if let Some(client_key) = new_handshake.get_remote_static() {
                                            let Some(chosen_magic2) = negotiate_magic2(&packet_memory_recv[..magic2_block_len]) else {
                                                if OVERLY_VERBOSE { println!("Did NOT respond to Client Hello from {:?}: magic2 negotiation failed.", (other_ip_addr, other_port)); }
                                                break;
                                            };
        
                                            let connection_key = ConnectionKey { ip: other_ip_addr, port: other_port, key_15_bits: load_u16(&client_key[0..2]) << 1 };
                                            if let Some(existing_connection) = connections_map.get_mut(&connection_key) {
                                                if &existing_connection.my_transport_identity_keypair == my_kp && existing_connection.other_transport_identity == client_key {
                                                    if let ConnectionState::SendingClientHello { magic1, last_sent_time_ns, hello_packet_payload, handshake } = &mut existing_connection.connection_state {
                                                        if *client_key < *my_kp.public {
                                                            let mut chosen_magic2_bytes = [0u8; 8];
                                                            store_u64(&mut chosen_magic2_bytes, chosen_magic2);
                                                            store_u48(&mut packet_memory_send[0..6], 0xffff_ffff_0000 | (load_u16(&my_kp.public[0..2]) << 1) as u64);
                                                            let handshake_size = new_handshake.write_message(&chosen_magic2_bytes, &mut packet_memory_send[6..]).unwrap();
                                                            let hello_packet_payload = Vec::from(&packet_memory_send[0..6+handshake_size]);
        
                                                            debug_assert!(new_handshake.is_handshake_finished());
                                                            existing_connection.handshake_hash = get_handshake_hash(&new_handshake);
                                                            let cipher = ConnectionCipherTriplet::new_from_old_init_only(new_handshake.into_stateless_transport_mode().expect("Cannot fail given assert above."));
        
                                                            existing_connection.connection_state = ConnectionState::SendingServerHello { cipher, magic1: *magic1, magic2: chosen_magic2, last_sent_time_ns: 0, hello_packet_payload };
                                                            if OVERLY_VERBOSE { println!("Transitioned connection {} to SendingServerHello.", connection_key.key_15_bits); }
                                                        } else {
                                                            if OVERLY_VERBOSE { println!("Did NOT transition {} to SendingServerHello: We \"lost\" the contended initiator comparison.", connection_key.key_15_bits); }
                                                        }
                                                    } else {
                                                        if OVERLY_VERBOSE { println!("Did NOT transition {} to SendingServerHello: We were not in the SendingClientHello state.", connection_key.key_15_bits); }
                                                    }
                                                } else {
                                                    if OVERLY_VERBOSE { println!("Did NOT respond to client hello from {}: @Todo explanation: Following expression was false: &existing_connection.my_transport_identity_keypair == my_kp && existing_connection.other_transport_identity == client_key", connection_key.key_15_bits); }
                                                }
                                            }
                                            else {
                                                let client_key = Vec::from(client_key);
                                                let mut chosen_magic2_bytes = [0u8; 8];
                                                store_u64(&mut chosen_magic2_bytes, chosen_magic2);
                                                store_u48(&mut packet_memory_send[0..6], 0xffff_ffff_0000 | (load_u16(&my_kp.public[0..2]) << 1) as u64);
                                                let handshake_size = new_handshake.write_message(&chosen_magic2_bytes, &mut packet_memory_send[6..]).unwrap();
                                                let hello_packet_payload = Vec::from(&packet_memory_send[0..6+handshake_size]);
        
                                                debug_assert!(new_handshake.is_handshake_finished());
                                                let handshake_hash = get_handshake_hash(&new_handshake);
                                                let cipher = ConnectionCipherTriplet::new_from_old_init_only(new_handshake.into_stateless_transport_mode().expect("Cannot fail given assert above."));
        
                                                let my_ip = if other_ip_addr.to_ipv4_mapped().is_some() { Ipv6Addr::UNSPECIFIED } else { Ipv6Addr::UNSPECIFIED };
                                                connections_map.insert(
                                                    connection_key,
                                                    ConnectionTrackingData {
                                                        creation_time_ns: monotonic_clock_ns(),
                                                        my_ip,
                                                        my_transport_identity_keypair: my_kp.clone(),
                                                        two_byte_send_prefix: load_u16(&my_kp.public[0..2]) << 1,
                                                        other_ip: other_ip_addr,
                                                        other_port: other_port,
                                                        other_transport_identity: client_key,
                                                        connection_state: ConnectionState::SendingServerHello { cipher, magic1, magic2: chosen_magic2, last_sent_time_ns: 0, hello_packet_payload },
                                                        jumbo_reassembly: Default::default(),
                                                        handshake_hash,
                                                        unreliable_send_buffer: UnreliableSendBuffer::new(send_buffer_params.0, send_buffer_params.1, send_buffer_params.2),
                                                        unreliable_reassembly: ReassemblySlot::new_ring(1024),
                                                        nym_sock: None,
                                                    },
                                                );
        
                                                if OVERLY_VERBOSE { println!("Transitioned connection {} to SendingServerHello.", connection_key.key_15_bits); }
                                            }
                                        } else {
                                            if OVERLY_VERBOSE { println!("Did NOT respond to Client Hello from {:?}: @Todo explanation: Following expression failed: let Some(client_key) = new_handshake.get_remote_static()", (other_ip_addr, other_port)); }
                                        }
                                    } else {
                                        if OVERLY_VERBOSE { println!("Did NOT respond to Client Hello from {:?}: new_handshake.read_message failed with error = {:?}", (other_ip_addr, other_port), read_message_maybe); }
                                    }
                                }
                            }
                        }
                    }
                    else { 'conn: { // Not client hello
        
                        let connection_key = ConnectionKey { ip: other_ip_addr, port: other_port, key_15_bits: first_six_bytes as u16 };
                        let Some(existing_connection) = connections_map.get_mut(&connection_key) else {
                            if OVERLY_VERBOSE {
                                println!("Packet arrived from non-connection. Drop!");
                            }
                            break 'conn;
                        };
    
                        match &mut existing_connection.connection_state {
                            ConnectionState::SendingClientHelloPlaintext { last_sent_time_ns, hello_packet_payload } => {
                                if first_six_bytes >> 16 == 0xffff_ffff && buf_len >= 6 + 32 + 8 {
                                    let server_key = &packet_memory_encrypted[6..6+32];
                                    if server_key == existing_connection.other_transport_identity {
                                        let server_magic2 = load_u64(&packet_memory_encrypted[6+32..6+32+8]);
                                        if !validate_magic2(server_magic2) {
                                            if OVERLY_VERBOSE { println!("Dropping from {connection_key:?}: server chose magic2 {server_magic2:#x} which we did not offer."); }
                                            break 'conn;
                                        }
                                        if VERBOSE { println!("Connected to new server {:?}", server_key); }
                                        existing_connection.connection_state = new_connection_state_connected(None, CONNECT_MAGIC1_PLAIN_TEXT, server_magic2, timestamp_ns);
                                    }
                                } else {
                                    if OVERLY_VERBOSE {
                                        println!("Dropping from {connection_key:?} because the plain text server hello was incorrectly sized.");
                                    }
                                }
                                break 'conn;
                            }
                            ConnectionState::SendingClientHello { magic1, last_sent_time_ns, hello_packet_payload, handshake } => {
                                if first_six_bytes >> 16 == 0xffff_ffff && buf_len >= 6 + 32 {
                                    if let Ok(payload_len) = handshake.read_message(&packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]) {
                                        debug_assert!(handshake.is_handshake_finished());
                                        existing_connection.handshake_hash = get_handshake_hash(handshake);
                                        if payload_len != 8 {
                                            if OVERLY_VERBOSE { println!("Dropping from {connection_key:?}: server hello payload was {payload_len} bytes, expected 8 (magic2)."); }
                                            break 'conn;
                                        }
                                        let server_magic2 = load_u64(&packet_memory_recv[0..8]);
                                        if !validate_magic2(server_magic2) {
                                            if OVERLY_VERBOSE { println!("Dropping from {connection_key:?}: server chose magic2 {server_magic2:#x} which we did not offer."); }
                                            break 'conn;
                                        }
                                        if VERBOSE { println!("Connected to new server {:?}", handshake.get_remote_static().unwrap()); }
    
                                        // Borrow Checker crazyness required here.
                                        let new_state = new_connection_state_connected(None, *magic1, server_magic2, timestamp_ns);
                                        let old_state = std::mem::replace(&mut existing_connection.connection_state, new_state);

                                        let ConnectionState::SendingClientHello { handshake, .. } = old_state else { panic!(); };
                                        let ConnectionState::Connected(ref mut state) = existing_connection.connection_state else { panic!(); };
                                        state.cipher = Some(ConnectionCipherTriplet::new_from_old_init_only(handshake.into_stateless_transport_mode().expect("Cannot fail given assert above.")));
                                    } else {
                                        if OVERLY_VERBOSE {
                                            println!("Dropping from {connection_key:?} because this failed: let Ok(payload_len) = handshake.read_message(&packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..])");
                                        }
                                    }
                                } else {
                                    if OVERLY_VERBOSE {
                                        println!("Dropping from {connection_key:?} because this was false: first_six_bytes >> 16 == 0xffff_ffff && buf_len >= 6 + 32");
                                    }
                                }
                                break 'conn;
                            }
                            ConnectionState::SendingServerHelloPlaintext { magic2, last_sent_time_ns, hello_packet_payload } => {
                                if VERBOSE { println!("Connected to new client {:?}", existing_connection.other_transport_identity); }
                                existing_connection.connection_state = new_connection_state_connected(None, CONNECT_MAGIC1_PLAIN_TEXT, *magic2, timestamp_ns);
                                // FALLTHROUGH TO CONNECTED
                            }
                            ConnectionState::SendingServerHello { cipher, magic1, magic2, last_sent_time_ns, hello_packet_payload } => {
                                let non_virtual_nonce = first_six_bytes >> 16; // Todo convert this to virtual.
    
                                let mut can_decrypt = false;
                                // Optimization done here. It can never be cipher.old so we skip checking.
                                can_decrypt |= cipher.current.read_message(non_virtual_nonce, &packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]).is_ok();
                                can_decrypt |= cipher.current.read_message(non_virtual_nonce, &packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]).is_ok();
                                if can_decrypt == false {
                                    break 'conn;
                                }
    
                                if VERBOSE { println!("Connected to new client {:?}", existing_connection.other_transport_identity); }
                                existing_connection.connection_state = new_connection_state_connected(Some(cipher.clone()), *magic1, *magic2, timestamp_ns);
                                // FALLTHROUGH TO CONNECTED
                            }
                            ConnectionState::Connected(_) => (),
                        }
//////// BEGIN ORDINARY PACKET HANDLING  ////////////////////////////////////////////////////////////////////
                        if let ConnectionState::Connected(state) = &mut existing_connection.connection_state {
                            let non_virtual_nonce = (first_six_bytes >> 16) as u32;
                            let nonce = non_virtual_nonce; // @Todo: convert this to virtual.
    
                            let payload;
                            if let Some(cipher) = &mut state.cipher {
                                if let Ok(payload_len) = cipher.current.read_message(nonce as u64, &packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]) {
                                    payload = &packet_memory_recv[0..payload_len];
                                }
                                else if let Ok(payload_len) = cipher.old.read_message(nonce as u64, &packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]) {
                                    payload = &packet_memory_recv[0..payload_len];
                                }
                                else if let Ok(payload_len) = cipher.new.read_message(nonce as u64, &packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]) {
                                    payload = &packet_memory_recv[0..payload_len];
                                    cipher.ratchet_forward_incoming();
                                }
                                else { break 'conn; }
                            }
                            else {
                                payload = &packet_memory_encrypted[6..buf_len];
                            }
                            
                            if payload.len() < 516 {
                                if OVERLY_VERBOSE { println!("Dropping from {connection_key:?}, payload < 516."); }
                                break 'conn;
                            }

                            if payload.len() == 516 { // This is an ack packet.
                                let ack = unsafe { &*(payload.as_ptr() as *const AckField) };

                                let mut RTT_acc = 0u64;
                                let mut RTT_count = 0u64;
                                
                                let mut oldest_newly_acked_send_time: u64 = u64::MAX;
                                let field_base = ack.field_base as u64;
                                for index in field_base..field_base+4096 {
                                    let bit_index = (index-field_base);
                                    let acked = 0 != (1u64 << (bit_index % 64)) & ack.field[bit_index as usize / 64];
                                    if acked && index >= state.packets_waiting_ack_tail && index < state.send_sequence_number {
                                        let bit_index = index % (2048*64);
                                        if state.packets_waiting_ack_field[(bit_index / 64) as usize] & (1u64 << (bit_index % 64)) != 0 {
                                            state.packets_waiting_ack_field[(bit_index / 64) as usize] &= !(1u64 << (bit_index % 64));
                                            state.last_ack_received_time = timestamp_ns;

                                            let send_time_of_latest_ns = get_send_time_for_sequence_number(index, &state.send_time_band, state.send_time_band_head_index);
                                            if send_time_of_latest_ns != 0 && send_time_of_latest_ns != u64::MAX {
                                                RTT_acc += ((timestamp_ns - send_time_of_latest_ns) / 100_000);
                                                RTT_count += 1;
                                                oldest_newly_acked_send_time = oldest_newly_acked_send_time.min(send_time_of_latest_ns);
                                            }
                                            
                                            if index == state.tu_probe_sequence_number {
                                                state.tu_probe_sequence_number = u64::MAX;
                                                state.last_sent_tu_probe_time_ns = 0;
                                                state.current_tu = state.tu_probe_size;
                                                state.tu_probe_size_advance *= 2;
                                                state.tu_probe_failed_count = 0;
                                            }
                                            else {
                                                assert!(state.packets_in_flight > 0, "packets_in_flight underflow");
                                                state.packets_in_flight -= 1;
                                            }
                                            
                                        }
                                    }
                                }

                                if ecn_marked && oldest_newly_acked_send_time != u64::MAX {
                                    state.ecn_congestion_received = true;
                                    state.ecn_oldest_send_time = oldest_newly_acked_send_time;
                                }

                                if RTT_count > 0 {
                                    let slot = (state.RTT_sample_cursor % 16) as usize;
                                    state.RTT_sample_sums[slot] = RTT_acc as u32;
                                    state.RTT_sample_counts[slot] = RTT_count as u16;
                                    state.ack_arrival_times[slot] = timestamp_ns;
                                    state.RTT_sample_cursor += 1;

                                    // Compute ack pace: mean to establish baseline, then max of non-outliers
                                    let n = (state.RTT_sample_cursor as usize).min(16);
                                    if n >= 2 {
                                        let clamp_ns = state.RTT_mean as u64 * 200_000; // 2 * RTT_mean in ns
                                        // Pass 1: clamped mean as baseline
                                        let mut gap_sum = 0u64;
                                        let mut gap_count = 0u64;
                                        for i in 1..n {
                                            let cur = state.ack_arrival_times[((state.RTT_sample_cursor - 1 - (i as u64 - 1)) % 16) as usize];
                                            let prev = state.ack_arrival_times[((state.RTT_sample_cursor - 1 - i as u64) % 16) as usize];
                                            if prev != 0 && cur > prev {
                                                gap_sum += (cur - prev).min(clamp_ns);
                                                gap_count += 1;
                                            }
                                        }
                                        if gap_count > 0 {
                                            let mean_ns = gap_sum / gap_count;
                                            // Pass 2: max of gaps within 2x the mean (filter idle gaps)
                                            let mut max_ns = mean_ns;
                                            for i in 1..n {
                                                let cur = state.ack_arrival_times[((state.RTT_sample_cursor - 1 - (i as u64 - 1)) % 16) as usize];
                                                let prev = state.ack_arrival_times[((state.RTT_sample_cursor - 1 - i as u64) % 16) as usize];
                                                if prev != 0 && cur > prev {
                                                    let gap = cur - prev;
                                                    if gap <= mean_ns * 2 {
                                                        max_ns = max_ns.max(gap);
                                                    }
                                                }
                                            }
                                            state.ack_pace_estimate = (max_ns / 100_000) as u16;
                                        }
                                    }
                                }
                                
                                break 'conn;
                            }
                            // This is not an ack packet.

                            if nonce < state.ack_field.field_base {
                                if OVERLY_VERBOSE { println!("Dropping from {connection_key:?}, nonce too old."); }
                                break 'conn;
                            }
                            if nonce > state.ack_field.field_base + u32::MAX / 2 {
                                if OVERLY_VERBOSE { println!("Dropping from {connection_key:?}, nonce too far in the future.."); }
                                break 'conn;
                            }
                            
                            let bit_index = nonce-state.ack_field.field_base;
                            if nonce < state.ack_field.field_base + 4096 && 0 != (1u64 << (bit_index % 64)) & state.ack_field.field[bit_index as usize / 64] {

                                if OVERLY_VERBOSE { println!("Dropping from {connection_key:?}, nonce already received."); }
                                break 'conn;
                            }

                            if nonce >= state.ack_field.field_base + 4096 {
                                { // send ack
                                    let mut o = 0;
                                    let virtual_nonce = state.send_sequence_number;
                                    o += existing_connection.two_byte_send_prefix.write_to(&mut packet_memory_send[o..]);
                                    o += (virtual_nonce as u32).write_to(&mut packet_memory_send[o..]);
                                    {
                                        let bit_index = state.send_sequence_number % (2048*64);
                                        state.packets_waiting_ack_field[(bit_index / 64) as usize] &= !(1u64 << (bit_index % 64));
                                        state.send_sequence_number += 1;
                                    }

                                    if let Some(cipher) = &mut state.cipher { o += cipher.current.write_message(virtual_nonce, state.ack_field.as_bytes(), &mut packet_memory_send[o..]).unwrap(); }
                                    else { o += state.ack_field.as_bytes().write_to(&mut packet_memory_send[o..]); }
                                    let ack_ecn = state.ecn_congestion_received;
                                    state.ecn_congestion_received = false;
                                    if let Some(ns) = &existing_connection.nym_sock { nym_udp_send_with_congestion_and_dscp(ns, existing_connection.other_ip, existing_connection.other_port, &packet_memory_send[..o], Dscp::Af21, ack_ecn); }
                                    else { udp_send_with_congestion_and_dscp(socket, existing_connection.other_ip, existing_connection.other_port, &packet_memory_send[..o], Dscp::Af21, ack_ecn); }
                                    state.ack_timer = u64::MAX;
                                }

                                if nonce >= state.ack_field.field_base + 4096 + 2048 {
                                    state.ack_field.field_base = nonce;
                                    state.ack_field.field = [0u64; 64];
                                }
                                else if nonce >= state.ack_field.field_base + 4096 {
                                    state.ack_field.field_base += 2048;
                                    let mut field = state.ack_field.field;
                                    let (bottom, top) = field.split_at_mut(32);
                                    bottom.copy_from_slice(top);
                                    top.fill(0);
                                    state.ack_field.field = field;
                                }
                            }

                            let bit_index = nonce-state.ack_field.field_base;
                            state.ack_field.field[bit_index as usize / 64] |= 1u64 << (bit_index % 64);
                            if ecn_marked { state.ecn_congestion_received = true; }
                            if state.ack_timer == u64::MAX { state.ack_timer = timestamp_ns + ACK_BUFFER_TIME_NS; }

                            // A legitimate data packet could never this state because to be all zeroes the
                            // first fragment would have to be package_id=0 and frag_len=0. If so that is
                            // either the only fragment and followed by u64::MAX terminator OR there would
                            // be a second fragment with a package_id != 0.
                            {
                                let mut all_null = true;
                                let mut i = 0;
                                while all_null && i < payload.len() {
                                    all_null &= payload[i] == 0;
                                    i += 1;
                                }
                                if all_null { 
                                    if OVERLY_VERBOSE { println!("Received keep alive from {connection_key:?}."); }
                                    break 'conn;
                                }
                            }

                            // if OVERLY_VERBOSE { println!("Got data from {:?}  data: {:?}", existing_connection.other_transport_identity, payload); }
                            
                            let mut remaining_payload = payload;
                            while remaining_payload.len() >= 8 {
                                let header = load_u64(&remaining_payload[0..8]);
                                if header == u64::MAX { break 'conn; }
                                
                                let is_reliable = header & 1 != 0;
                                let is_fin = header & 2 != 0;
                                let package_id = (header >> 2) & 0xffff;
                                let frag_len = (header >> 18) & 0x3fff;
                                let frag_offset = header >> 32;
                                
                                if (remaining_payload.len() as u64) < 8+frag_len {
                                    if OVERLY_VERBOSE { println!("Error, frag_len ({frag_len:?}) extends beyond end of packet received from {connection_key:?}. Disconnecting..."); }
                                    connections_map.remove(&connection_key);
                                    break 'conn;
                                }
                                let frag_data = &remaining_payload[8..(8+frag_len as usize)];
                                remaining_payload = &remaining_payload[(8+frag_len as usize)..];

                                if frag_offset + frag_len > u32::MAX as u64 {
                                    if OVERLY_VERBOSE { println!("Error, frag_offset + frag_len overflows u32 from {connection_key:?}. Disconnecting..."); }
                                    connections_map.remove(&connection_key);
                                    break 'conn;
                                }

                                //if OVERLY_VERBOSE { println!("Fragment from {:?}: R:{} F:{} ID:{} L:{} O:{}", connection_key, is_reliable, is_fin, package_id, frag_len, frag_offset); }
                                
                                let slot_idx = package_id as usize % existing_connection.unreliable_reassembly.len();
                                let (mut success, mut complete) = existing_connection.unreliable_reassembly[slot_idx].insert(frag_offset as usize, frag_data, is_fin);
                                if !success {
                                    existing_connection.unreliable_reassembly[slot_idx] = ReassemblySlot::new();
                                    (success, complete) = existing_connection.unreliable_reassembly[slot_idx].insert(frag_offset as usize, frag_data, is_fin);
                                    assert!(success);
                                }
                                if complete {
                                    let completed = std::mem::replace(&mut existing_connection.unreliable_reassembly[slot_idx], ReassemblySlot::new());
                                    received_unreliable_messages.push((connection_key, completed.buf));
                                }
                            }

//////// END ORDINARY PACKET HANDLING  ////////////////////////////////////////////////////////////////////
                        }
                    } } // <- break 'conn lands here
                }
//////// END RECEIVE ////////////////////////////////////////////////////////////////////////

//////// BEGIN SEND ////////////////////////////////////////////////////////////////////////
                let current_time_now_ns = monotonic_clock_ns();
                connections_map.retain(|connection_key, connection_tracking_data| {match &mut connection_tracking_data.connection_state {
                    ConnectionState::SendingClientHelloPlaintext { last_sent_time_ns, hello_packet_payload } => {
                        if *last_sent_time_ns + 2_500_000_000 < current_time_now_ns {
                            if let Some(ns) = &connection_tracking_data.nym_sock { nym_udp_send_with_congestion_and_dscp(ns, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21, false); }
                            else { udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21, false); }
                            if *last_sent_time_ns == 0 { // cheeky optimization
                                *last_sent_time_ns = current_time_now_ns - 2_500_000_000 + 10_000_000;
                            } else {
                                *last_sent_time_ns = current_time_now_ns;
                            }
                            return true;
                        }
                        if connection_tracking_data.creation_time_ns + 30_000_000_000 < current_time_now_ns {
                            return false;
                        }
                    }
                    ConnectionState::SendingClientHello { magic1, last_sent_time_ns, hello_packet_payload, handshake } => {
                        if *last_sent_time_ns + 2_500_000_000 < current_time_now_ns {
                            if let Some(ns) = &connection_tracking_data.nym_sock { nym_udp_send_with_congestion_and_dscp(ns, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21, false); }
                            else { udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21, false); }
                            if *last_sent_time_ns == 0 { // cheeky optimization
                                *last_sent_time_ns = current_time_now_ns - 2_500_000_000 + 10_000_000;
                            } else {
                                *last_sent_time_ns = current_time_now_ns;
                            }
                            return true;
                        }
                        if connection_tracking_data.creation_time_ns + 30_000_000_000 < current_time_now_ns {
                            return false;
                        }
                    }
                    ConnectionState::SendingServerHelloPlaintext { magic2: _, last_sent_time_ns, hello_packet_payload } => {
                        if *last_sent_time_ns + 2_500_000_000 < current_time_now_ns {
                            if let Some(ns) = &connection_tracking_data.nym_sock { nym_udp_send_with_congestion_and_dscp(ns, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21, false); }
                            else { udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21, false); }
                            if *last_sent_time_ns == 0 { // cheeky optimization
                                *last_sent_time_ns = current_time_now_ns - 2_500_000_000 + 10_000_000;
                            } else {
                                *last_sent_time_ns = current_time_now_ns;
                            }
                            return true;
                        }
                        if connection_tracking_data.creation_time_ns + 15_000_000_000 < current_time_now_ns {
                            return false;
                        }
                    }
                    ConnectionState::SendingServerHello { magic1, magic2: _, cipher, last_sent_time_ns, hello_packet_payload } => {
                        if *last_sent_time_ns + 2_500_000_000 < current_time_now_ns {
                            if let Some(ns) = &connection_tracking_data.nym_sock { nym_udp_send_with_congestion_and_dscp(ns, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21, false); }
                            else { udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21, false); }
                            if *last_sent_time_ns == 0 { // cheeky optimization
                                *last_sent_time_ns = current_time_now_ns - 2_500_000_000 + 10_000_000;
                            } else {
                                *last_sent_time_ns = current_time_now_ns;
                            }
                            return true;
                        }
                        if connection_tracking_data.creation_time_ns + 15_000_000_000 < current_time_now_ns {
                            return false;
                        }
                    }
                    ConnectionState::Connected(state) => {
                        debug_assert!(state.magic2 != 0, "Connection entered Connected state without negotiating magic2");
                        if state.last_ack_received_time + 15_000_000_000 < current_time_now_ns {
                            if VERBOSE { println!("Disconnected from: {:?}.", connection_tracking_data.other_transport_identity); }
                            return false; // connection timeout
                        }
                        
                        if current_time_now_ns > state.ack_timer {
                            { // send ack
                                let mut o = 0;
                                let virtual_nonce = state.send_sequence_number;
                                o += connection_tracking_data.two_byte_send_prefix.write_to(&mut packet_memory_send[o..]);
                                o += (virtual_nonce as u32).write_to(&mut packet_memory_send[o..]);
                                {
                                    let bit_index = state.send_sequence_number % (2048*64);
                                    state.packets_waiting_ack_field[(bit_index / 64) as usize] &= !(1u64 << (bit_index % 64));
                                    state.send_sequence_number += 1;
                                }
                                
                                if let Some(cipher) = &mut state.cipher { o += cipher.current.write_message(virtual_nonce, state.ack_field.as_bytes(), &mut packet_memory_send[o..]).unwrap(); }
                                else { o += state.ack_field.as_bytes().write_to(&mut packet_memory_send[o..]); }
                                let ack_ecn = state.ecn_congestion_received;
                                state.ecn_congestion_received = false;
                                if let Some(ns) = &connection_tracking_data.nym_sock { nym_udp_send_with_congestion_and_dscp(ns, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_send[..o], Dscp::Af21, ack_ecn); }
                                else { udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_send[..o], Dscp::Af21, ack_ecn); }
                                state.ack_timer = u64::MAX;
                            }
                        }
                        
                        if state.RTT_sample_cursor != 0 && state.last_ack_received_time + 15_000_000_000/2 < current_time_now_ns {
                            // blackhole detected, go to safe TU
                            state.current_tu = ASSUMED_UDP_PAYLOAD_SIZE_WITH_GUARANTEED_DELIVERY as u64;
                            state.RTT_sample_cursor = 0;
                            state.RTT_mean = u16::MAX;
                            state.ack_pace_estimate = 0;
                            state.congestion_event_time_ns = 0;
                            state.congestion_event_rate_upps = 0;
                            state.last_sent_data_packet = 0;
                            state.last_sent_could_have_sent_data_packet = 0;
                            if state.tu_probe_sequence_number != u64::MAX {
                                // Orphaning an in-flight TU probe. It was never counted in packets_in_flight,
                                // but a late ack or the tail-advance loop will try to decrement it.
                                // Count it now so the decrement balances.
                                state.packets_in_flight += 1;
                                state.max_in_flight_since_last_print = state.max_in_flight_since_last_print.max(state.packets_in_flight);
                            }
                            state.tu_probe_sequence_number = u64::MAX;
                            state.tu_probe_failed_count = 0;
                            if OVERLY_VERBOSE { println!("Blackhole detected!"); }
                        }
                        
                        if state.congestion_event_time_ns == 0 {
                            state.congestion_event_time_ns = current_time_now_ns;
                        }

                        if state.is_app_limited {
                            state.congestion_event_time_ns = current_time_now_ns.saturating_sub(state.app_limit_time_offset);
                        }

                        {
                            let old_k = CUBIC_K_RTT_MULTIPLIER * (state.RTT_mean as u64 * 100_000);
                            let current_t = current_time_now_ns.saturating_sub(state.congestion_event_time_ns);
                            let (old_rate, _) = cubic_rate(current_t, state.congestion_event_rate_upps, old_k);

                            let n = (state.RTT_sample_cursor as usize).min(16);
                            if n > 0 {
                                let mut total_sum = 0u64;
                                let mut total_count = 0u64;
                                for i in 0..n {
                                    total_sum += state.RTT_sample_sums[i] as u64;
                                    total_count += state.RTT_sample_counts[i] as u64;
                                }
                                state.RTT_mean = (total_sum / total_count) as u16;
                            }

                            let new_k = CUBIC_K_RTT_MULTIPLIER * (state.RTT_mean as u64 * 100_000);
                            // First RTT received — backdate to R→P transition
                            if state.RTT_mean != u16::MAX && old_k == CUBIC_K_RTT_MULTIPLIER * (u16::MAX as u64 * 100_000) {
                                state.congestion_event_time_ns = current_time_now_ns.saturating_sub(new_k);
                                if state.is_app_limited {
                                    state.app_limit_time_offset = new_k;
                                }
                            }
                            else if old_k > 0 && new_k > 0 {
                                let mut lo = if new_k >= old_k { current_t } else { 0u64 };
                                let mut hi = if new_k >= old_k { current_t * 2 + new_k } else { current_t };
                                for _ in 0..64 {
                                    let mid = lo + (hi - lo) / 2;
                                    if cubic_rate(mid, state.congestion_event_rate_upps, new_k).0 >= old_rate {
                                        hi = mid;
                                    } else {
                                        lo = mid + 1;
                                    }
                                }
                                state.congestion_event_time_ns = current_time_now_ns.saturating_sub(hi);
                                if state.is_app_limited {
                                    state.app_limit_time_offset = hi;
                                }
                            }
                        }

                        let time_to_bandwidth_recovery = CUBIC_K_RTT_MULTIPLIER * (state.RTT_mean as u64 * 100_000);

                        // Local Limit congestion event
                        if let Some(max_pps) = max_pps && max_pps*1_000_000 < cubic_rate(current_time_now_ns - state.congestion_event_time_ns, state.congestion_event_rate_upps, time_to_bandwidth_recovery).0 && current_time_now_ns > state.congestion_event_time_ns + (state.RTT_mean as u64 * 100_000) / 2 {
                            let send_dt = current_time_now_ns - state.congestion_event_time_ns;
                            let send_t_factor = send_dt as f64 / time_to_bandwidth_recovery as f64;
                            let (_, send_mode) = cubic_rate(send_dt, state.congestion_event_rate_upps, time_to_bandwidth_recovery);
                            let discover_dt = if send_mode == 'P' { (time_to_bandwidth_recovery + send_dt) / 2 } else { send_dt };
                            let mut discovered = cubic_rate(discover_dt, state.congestion_event_rate_upps, time_to_bandwidth_recovery).0;
                            if send_mode == 'G' || send_mode == 'S' {
                                let r_pg = (state.congestion_event_rate_upps as f64 * RATE_PG) as u64;
                                discovered = r_pg + ((discovered.saturating_sub(r_pg)) as f64 * 0.5) as u64;
                            }
                            state.congestion_event_rate_upps = max_pps*1_000_000;
                            state.congestion_event_time_ns = current_time_now_ns;
                            state.app_limit_time_offset = 0;
                            println!("local limit congestion ...  rate \"discovered\": {} pps  sent:{send_mode}@{send_t_factor:.2}", state.congestion_event_rate_upps as f64 / 1000000.0);
                        }

                        // ECN congestion event: the receiver saw CE-marked packets and relayed it via the ack's IP header
                        if state.ecn_congestion_received && state.ecn_oldest_send_time > state.congestion_event_time_ns + (state.RTT_mean as u64 * 100_000) / 2 {
                            state.ecn_congestion_received = false;
                            let send_dt = state.ecn_oldest_send_time - state.congestion_event_time_ns;
                            let send_t_factor = send_dt as f64 / time_to_bandwidth_recovery as f64;
                            let (_, send_mode) = cubic_rate(send_dt, state.congestion_event_rate_upps, time_to_bandwidth_recovery);
                            let discover_dt = if send_mode == 'P' { (time_to_bandwidth_recovery + send_dt) / 2 } else { send_dt };
                            let mut discovered = cubic_rate(discover_dt, state.congestion_event_rate_upps, time_to_bandwidth_recovery).0;
                            if send_mode == 'G' || send_mode == 'S' {
                                let r_pg = (state.congestion_event_rate_upps as f64 * RATE_PG) as u64;
                                discovered = r_pg + ((discovered.saturating_sub(r_pg)) as f64 * 0.5) as u64;
                            }
                            state.congestion_event_rate_upps = discovered;
                            state.congestion_event_time_ns = current_time_now_ns;
                            state.app_limit_time_offset = 0;
                            println!("congestion event ECN CE ...  rate discovered: {} pps  sent:{send_mode}@{send_t_factor:.2}", state.congestion_event_rate_upps as f64 / 1000000.0);
                        }

                        // NOTE(Sam): We may experience black holes if RTT suddenly increases since
                        // there is a cyclic dependency beteen RTT and measuring RTT because of drop.
                        
                        let time_to_considered_dropped = (state.RTT_mean as u64 * 100_000 * 2).max((2*(current_time_now_ns - state.last_ack_received_time)).min(1_000_000_000));
                        
                        // TODO: Block sending until we have space.
                        assert!(state.send_sequence_number - state.packets_waiting_ack_tail < 2048*64);
                        while state.packets_waiting_ack_tail < state.send_sequence_number {
                            let bit_index = state.packets_waiting_ack_tail % (2048*64);
                            if state.packets_waiting_ack_field[(bit_index / 64) as usize] & (1u64 << (bit_index % 64)) != 0 {
                                let mut send_time = get_send_time_for_sequence_number(state.packets_waiting_ack_tail, &state.send_time_band, state.send_time_band_head_index);
                                if send_time != 0 && (state.RTT_mean == 0 || (current_time_now_ns - send_time) < time_to_considered_dropped) {
                                    break;
                                }
                                if send_time == 0 { send_time = current_time_now_ns - 4_295_000_000; }
                                
                                if state.packets_waiting_ack_tail == state.tu_probe_sequence_number {
                                    state.tu_probe_sequence_number = u64::MAX;
                                    state.tu_probe_size_advance /= 5;
                                    state.tu_probe_failed_count += 1;
                                }
                                else {
                                    // Track losses in a rolling window of 1 RTT
                                    let rtt_ns = state.RTT_mean as u64 * 100_000;
                                    if current_time_now_ns - state.loss_window_start_ns > rtt_ns {
                                        state.loss_window_start_ns = current_time_now_ns;
                                        state.loss_count_in_window = 0;
                                    }
                                    state.loss_count_in_window += 1;

                                    if state.loss_count_in_window >= 2 && send_time > state.congestion_event_time_ns + (state.RTT_mean as u64 * 100_000) / 2 {
                                        let send_dt = send_time - state.congestion_event_time_ns;
                                        let send_t_factor = send_dt as f64 / time_to_bandwidth_recovery as f64;
                                        let (_, send_mode) = cubic_rate(send_dt, state.congestion_event_rate_upps, time_to_bandwidth_recovery);
                                        let now_dt = current_time_now_ns - state.congestion_event_time_ns;
                                        let now_t_factor = now_dt as f64 / time_to_bandwidth_recovery as f64;
                                        let (_, now_mode) = cubic_rate(now_dt, state.congestion_event_rate_upps, time_to_bandwidth_recovery);
                                        // R: use rate at send time.
                                        // P/G/S: blend 50% toward R/P boundary rate.
                                        let mut discovered = cubic_rate(send_dt, state.congestion_event_rate_upps, time_to_bandwidth_recovery).0;
                                        if send_mode != 'R' {
                                            let boundary = (state.congestion_event_rate_upps as f64 * RATE_RP) as u64;
                                            discovered = (discovered + boundary) / 2;
                                        }
                                        state.congestion_event_rate_upps = discovered;
                                        state.congestion_event_time_ns = current_time_now_ns;
                                        state.app_limit_time_offset = 0;
                                        state.loss_count_in_window = 0;
                                        println!("congestion event PACKET DROPPED {} ...  rate discovered: {} pps  now:{now_mode}@{now_t_factor:.2} sent:{send_mode}@{send_t_factor:.2}", state.packets_waiting_ack_tail, state.congestion_event_rate_upps as f64 / 1000000.0);
                                    }
                                    assert!(state.packets_in_flight > 0, "packets_in_flight underflow");
                                    state.packets_in_flight -= 1;
                                    state.packet_lost_since_last_print += 1;
                                }
                            }
                            state.packets_waiting_ack_field[(bit_index / 64) as usize] &= !(1u64 << (bit_index % 64));
                            state.packets_waiting_ack_tail += 1;
                        }
                        
                        state.congestion_event_rate_upps = state.congestion_event_rate_upps.max(1_000_000);
                        
                        let (allowed_bandwidth_upps, cubic_mode) = if state.is_app_limited {
                            let rtt_ns = state.RTT_mean as u64 * 100_000;
                            let a_t = state.app_limit_time_offset.max(rtt_ns * 2);
                            (cubic_rate(a_t, state.congestion_event_rate_upps, time_to_bandwidth_recovery).0, 'A')
                        } else {
                            cubic_rate(current_time_now_ns - state.congestion_event_time_ns, state.congestion_event_rate_upps, time_to_bandwidth_recovery)
                        };
                        let allowed_bandwidth_upps = allowed_bandwidth_upps.max(1_000_000);
                        // pps * 10^-6 * rtt * 10^-4 = p * 10^10
                        // BDP + sawtooth: pipe is full (BDP) plus ack batching excess (ack_pace_estimate is in 0.1ms units)
                        let bdp = allowed_bandwidth_upps * state.RTT_mean as u64 / 10_000;
                        let sawtooth = allowed_bandwidth_upps * state.ack_pace_estimate as u64 / 10_000;
                        let allowed_packets_in_flight = ((bdp + sawtooth) / 1_000_000).max(1);

                        // 1 / (upps * 10^-6) = 10^6 / upps <- seconds
                        // ns = (10^6 / upps) * 10^9 = 10^15 / upps
                        let time_between_sends_ns = 1_000_000_000_000_000 / allowed_bandwidth_upps.max(1);
                        // This is some Unity Game developer level crap approximation but it will have to do.
                        let mut packet_send_allowance_now = (current_time_now_ns + 1 - state.last_sent_could_have_sent_data_packet.max(state.last_ack_received_time.saturating_sub(ACK_BUFFER_TIME_NS))) / time_between_sends_ns;
                        let send_allowance_time_remainder = (current_time_now_ns + 1 - state.last_sent_could_have_sent_data_packet.max(state.last_ack_received_time.saturating_sub(ACK_BUFFER_TIME_NS)))
                                                                - time_between_sends_ns*packet_send_allowance_now;
                        
                        if state.send_pacer_acc_ns > time_between_sends_ns {
                            state.send_pacer_acc_ns -= time_between_sends_ns;
                            packet_send_allowance_now += 1;
                        }
                        let could_have_sent = packet_send_allowance_now != 0;
                        
                        let ring_buffer_not_full = state.send_sequence_number - state.packets_waiting_ack_tail < 2048*64 - 1;

                        state.current_tu = state.current_tu.min(ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE as u64);
                        if ring_buffer_not_full && state.current_tu != ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE as u64 && state.tu_probe_sequence_number == u64::MAX && state.last_sent_tu_probe_time_ns + (state.tu_probe_failed_count.min(50)*state.tu_probe_failed_count.min(50)*250_000_000).max(time_between_sends_ns) < current_time_now_ns {
                            state.tu_probe_size_advance = state.tu_probe_size_advance.max(1).min(ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE as u64);
                            state.tu_probe_size = (state.current_tu + state.tu_probe_size_advance).min(ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE as u64);
                            let payload_size = state.tu_probe_size as usize - total_packet_payload_overhead_from_connect_magic1_inside_udp_payload(state.magic1).unwrap();
                            let null_bytes = [0u8; ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE];
                            
                            state.tu_probe_sequence_number = state.send_sequence_number;
                        
                            let virtual_nonce = state.send_sequence_number;
                            store_u16(&mut packet_memory_encrypted[0..2], connection_tracking_data.two_byte_send_prefix);
                            store_u32(&mut packet_memory_encrypted[2..6], virtual_nonce as u32);

                            let packet_len;
                            if let Some(cipher) = &mut state.cipher {
                                packet_len = cipher.current.write_message(virtual_nonce, &null_bytes[0..payload_size], &mut packet_memory_encrypted[6..]).unwrap();
                            }
                            else {
                                (&null_bytes[0..payload_size]).write_to(&mut packet_memory_encrypted[6..]);
                                packet_len = payload_size;
                            }

                            let send_time_ns = if let Some(ns) = &connection_tracking_data.nym_sock { nym_udp_send_with_congestion_and_dscp(ns, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_encrypted[0..6+packet_len], Dscp::Af21, false) }
                            else { udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_encrypted[0..6+packet_len], Dscp::Af21, false) };
                            increment_sequence_number_and_account(send_time_ns, &mut state.send_sequence_number, &mut state.send_time_band, &mut state.send_time_band_head_index, &mut state.packets_waiting_ack_field);
                            state.last_sent_tu_probe_time_ns = send_time_ns;
                            state.packet_since_last_print += 1;
                        }
                        
                        let payload_size = state.current_tu as usize - total_packet_payload_overhead_from_connect_magic1_inside_udp_payload(state.magic1).unwrap();
                        
                        if ring_buffer_not_full && state.last_sent_data_packet + 15_000_000_000/3 < current_time_now_ns {
                            let null_bytes = [0u8; ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE];
                        
                            let virtual_nonce = state.send_sequence_number;
                            store_u16(&mut packet_memory_encrypted[0..2], connection_tracking_data.two_byte_send_prefix);
                            store_u32(&mut packet_memory_encrypted[2..6], virtual_nonce as u32);

                            let packet_len;
                            if let Some(cipher) = &mut state.cipher {
                                packet_len = cipher.current.write_message(virtual_nonce, &null_bytes[0..payload_size], &mut packet_memory_encrypted[6..]).unwrap();
                            }
                            else {
                                (&null_bytes[0..payload_size]).write_to(&mut packet_memory_encrypted[6..]);
                                packet_len = payload_size;
                            }

                            let send_time_ns = if let Some(ns) = &connection_tracking_data.nym_sock { nym_udp_send_with_congestion_and_dscp(ns, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_encrypted[0..6+packet_len], Dscp::Af21, false) }
                            else { udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_encrypted[0..6+packet_len], Dscp::Af21, false) };
                            increment_sequence_number_and_account(send_time_ns, &mut state.send_sequence_number, &mut state.send_time_band, &mut state.send_time_band_head_index, &mut state.packets_waiting_ack_field);
                            state.last_sent_data_packet = current_time_now_ns; // We use the older time here so the pacer skips less packets.
                            state.packets_in_flight += 1;
                            state.max_in_flight_since_last_print = state.max_in_flight_since_last_print.max(state.packets_in_flight);
                            packet_send_allowance_now = packet_send_allowance_now.saturating_sub(1);
                            state.packet_since_last_print += 1;
                        }

                        connection_tracking_data.unreliable_send_buffer.current_bytes_per_second = payload_size as u64 * allowed_bandwidth_upps / 1000_000;
                        connection_tracking_data.unreliable_send_buffer.cut_to_size();
                        while ring_buffer_not_full && packet_send_allowance_now > 0 && state.packets_in_flight < allowed_packets_in_flight && fill_packet_payload_with_unreliable_fragments(&mut packet_memory_send[0..payload_size], &mut connection_tracking_data.unreliable_send_buffer) {
                        
                            let virtual_nonce = state.send_sequence_number;
                            store_u16(&mut packet_memory_encrypted[0..2], connection_tracking_data.two_byte_send_prefix);
                            store_u32(&mut packet_memory_encrypted[2..6], virtual_nonce as u32);

                            let packet_len;
                            if let Some(cipher) = &mut state.cipher {
                                packet_len = cipher.current.write_message(virtual_nonce, &packet_memory_send[0..payload_size], &mut packet_memory_encrypted[6..]).unwrap();
                            }
                            else {
                                (&packet_memory_send[0..payload_size]).write_to(&mut packet_memory_encrypted[6..]);
                                packet_len = payload_size;
                            }

                            should_sleep = false;
                            let send_time_ns = if let Some(ns) = &connection_tracking_data.nym_sock { nym_udp_send_with_congestion_and_dscp(ns, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_encrypted[0..6+packet_len], Dscp::BestEffort, false) }
                            else { udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_encrypted[0..6+packet_len], Dscp::BestEffort, false) };
                            increment_sequence_number_and_account(send_time_ns, &mut state.send_sequence_number, &mut state.send_time_band, &mut state.send_time_band_head_index, &mut state.packets_waiting_ack_field);
                            state.last_sent_data_packet = current_time_now_ns; // We use the older time here so the pacer skips less packets.
                            state.packets_in_flight += 1;
                            state.max_in_flight_since_last_print = state.max_in_flight_since_last_print.max(state.packets_in_flight);
                            packet_send_allowance_now = packet_send_allowance_now.saturating_sub(1);
                            state.packet_since_last_print += 1;
                        }

                        let has_data = connection_tracking_data.unreliable_send_buffer.has_unsent_data();
                        if !has_data && !state.is_app_limited {
                            if OVERLY_VERBOSE { println!("####### START APP LIMITED ######"); }
                            state.is_app_limited = true;
                            state.app_limit_time_offset = current_time_now_ns - state.congestion_event_time_ns;
                            if cubic_mode == 'G' || cubic_mode == 'S' || cubic_mode == 'L' {
                                state.congestion_event_rate_upps = (allowed_bandwidth_upps as f64 / RATE_PG) as u64;
                                let t_pg = (TIME_PG * time_to_bandwidth_recovery as f64) as u64;
                                state.app_limit_time_offset = t_pg;
                                state.congestion_event_time_ns = current_time_now_ns.saturating_sub(t_pg);
                            }
                        } else if has_data && state.is_app_limited {
                            if OVERLY_VERBOSE { println!("####### END APP LIMITED ######"); }
                            state.is_app_limited = false;
                            let rtt_ns = state.RTT_mean as u64 * 100_000;
                            let exit_t = state.app_limit_time_offset.max(rtt_ns * 2);
                            state.congestion_event_time_ns = current_time_now_ns.saturating_sub(exit_t);
                        }
                        
                        if could_have_sent {
                            state.last_sent_could_have_sent_data_packet = current_time_now_ns;
                            if packet_send_allowance_now == 0 {
                                state.send_pacer_acc_ns += send_allowance_time_remainder;
                            }
                            else {
                                state.send_pacer_acc_ns = 0;
                            }
                        }
                        
                        // bonus status debug print
                        if state.last_status_print_time + 1_000_000_000 < current_time_now_ns {
                            state.last_status_print_time = current_time_now_ns;
                            let mode = cubic_mode;
                            let mean = state.RTT_mean as f64 / 10.0;
                            let ack_pace = state.ack_pace_estimate as f64 / 10.0;
                            let n = (state.RTT_sample_cursor as usize).min(16);
                            if OVERLY_VERBOSE { println!("{mode} RTT (ms): mean={mean:.1} ack_pace={ack_pace:.1} (n={n}) --- TU: {} B  failed probes: {} --- target: {} pps   in flight: {}/{} (max {})  pacing: {} us  lost/sent: {}/{}", state.current_tu, state.tu_probe_failed_count, allowed_bandwidth_upps as f64 / 1000000.0, state.packets_in_flight, allowed_packets_in_flight, state.max_in_flight_since_last_print, time_between_sends_ns / 1000, state.packet_lost_since_last_print, state.packet_since_last_print); }
                            state.packet_since_last_print = 0;
                            state.packet_lost_since_last_print = 0;
                            state.max_in_flight_since_last_print = state.packets_in_flight;
                        }
                    }
                } true});

//////// END SEND ////////////////////////////////////////////////////////////////////////

                if should_sleep { std::thread::yield_now(); }
            }
        }
    });

    NetworkThreadHandle {
        inner,
        thread,
    }
}

const CUBIC_BETA: f64 = 0.67;
const CUBIC_K_RTT_MULTIPLIER: u64 = 9;
const CUBIC_MAX_RATE_UPPS_PER_SEC: f64 = 83_668_000_000.0; // 1000 Mbps/s linear cap
const CUBIC_MIN_GROWTH_UPPSPS: f64 = 1_000_000.0; // 1 pps per second minimum linear growth

// -- Edit with Claude ------------------------------------------------------
// Piecewise rate curve after recovery. All times are multiples of k (the
// recovery time = CUBIC_K_RTT_MULTIPLIER * RTT).  Rate targets are
// multiples of r (the pre-drop rate, "congestion upps").
//
//  Transition times (as factors on k, cumulative from congestion event):
//    R-P  1.0  (implicit: recovery ends at t = k)
//    P-G  5.0
//    G-S  15.0
//
//  Rate targets at each transition (as factors on r):
//    R-P  0.98   (recovery stops 2% below r)
//    P-G  1.05   (5% above r)
//    G-S  4.0    (300% above r)
//
//  Curve shapes:
//    R: p^2 recovery from r*BETA up to r*RATE_RP
//    P: linear from r to r*P-G_factor, entry derivative carried into G
//    G: quadratic from P-G point, entry derivative matches P exit
//    S: p^10 explosive ramp (startup / bandwidth discovery)
//    L: linear cap at CUBIC_MAX_RATE_UPPS_PER_SEC
//
//  Modes: R P G S L
//
// Note(Claude): When changing constants below, always update this
// description block to match. The description is the source of truth
// for the curve design.
// -------------------------------------------------------------------------─
const RATE_RP: f64 = 0.98;  // rate factor at R-P transition
const RATE_PG: f64 = 1.05;  // rate factor at P-G transition
const RATE_GS: f64 = 4.0;   // rate factor at G-S transition
const TIME_PG: f64 = 5.0;   // time factor (in k) for P-G transition
const TIME_GS: f64 = 15.0;  // time factor (in k) for G-S transition

/// Evaluate the CUBIC sending-rate function.
/// t_ns:  nanoseconds since the last congestion event.
/// r_max: sending rate (upps) at the time the dropped packet was sent.
/// k_ns:  time in nanoseconds to recover back to r_max.
/// Returns (target sending rate in upps, mode char: R/P/G/S/L).
pub fn cubic_rate(t_ns: u64, r_max: u64, k_ns: u64) -> (u64, char) {
    if k_ns == 0 { return (r_max, 'R'); }
    let t = t_ns as f64;
    let r = r_max as f64;
    let k = k_ns as f64;
    let drop = r * (1.0 - CUBIC_BETA);
    let min_growth = CUBIC_MIN_GROWTH_UPPSPS / 1e9 * t;
    let linear_cap_slope = CUBIC_MAX_RATE_UPPS_PER_SEC / 1e9;

    // R: p^2 recovery from r*BETA up to r*RATE_RP
    let r_rp = r * RATE_RP; // rate at R→P
    let r_drop = r_rp - r * CUBIC_BETA; // total recovery range
    if t <= k {
        let p = 1.0 - t / k;
        let rate = r_rp - r_drop * p * p + min_growth;
        return ((rate.max(r * CUBIC_BETA)) as u64, 'R');
    }

    let dt = t - k;
    let t_pg = (TIME_PG - 1.0) * k; // duration of P phase
    let t_gs = (TIME_GS - 1.0) * k; // duration of P+G phase (from end of R)

    let r_pg = r * RATE_PG; // rate at P→G
    let r_gs = r * RATE_GS; // rate at G→S

    // P: linear from r_rp to r_pg
    let p_slope = (r_pg - r_rp) / t_pg; // derivative at P exit = G entry

    if dt <= t_pg {
        let rate = r_rp + p_slope * dt + min_growth;
        let capped = r_rp + linear_cap_slope * dt + min_growth;
        let rate = rate.min(capped);
        let rate = if rate.is_finite() { rate } else { r };
        return (rate as u64, 'P');
    }

    // G: quadratic from (t_pg, r_pg), entry derivative = p_slope
    // rate = r_pg + p_slope*(dt-t_pg) + a*(dt-t_pg)^2
    // At dt=t_gs: rate = r_gs → solve for a
    let g_dt = t_gs - t_pg; // duration of G phase
    let a = (r_gs - r_pg - p_slope * g_dt) / (g_dt * g_dt);

    if dt <= t_gs {
        let u = dt - t_pg;
        let rate = r_pg + p_slope * u + a * u * u + min_growth;
        let capped = r + linear_cap_slope * dt + min_growth;
        let rate = rate.min(capped);
        let rate = if rate.is_finite() { rate } else { r };
        return (rate as u64, 'G');
    }

    // S: p^10 explosive ramp from G→S point
    // Entry derivative = p_slope + 2*a*g_dt (G exit derivative)
    let g_exit_slope = p_slope + 2.0 * a * g_dt;
    // p^10 with time constant chosen so entry derivative matches G exit:
    // d/dt(amplitude * (u/k_s)^10) at u=0 = 0, so we add a linear term
    // rate = r_gs + g_exit_slope*(dt-t_gs) + amplitude * ((dt-t_gs)/k_s)^10
    let k_s = k * 3.0; // startup time constant
    let s_amplitude = drop * 10.0; // aggressive startup amplitude
    let u = dt - t_gs;
    let ps = u / k_s;
    let ps2 = ps * ps;
    let ps5 = ps2 * ps2 * ps;
    let rate = r_gs + g_exit_slope * u + s_amplitude * ps5 * ps5 + min_growth;
    let capped = r + linear_cap_slope * dt + min_growth;
    let uncapped = rate;
    let rate = rate.min(capped);
    let rate = if rate.is_finite() { rate } else { r };
    let mode = if uncapped > capped { 'L' } else { 'S' };
    (rate as u64, mode)
}


pub fn service_connections(network_thread_handle: &NetworkThreadHandle, mut req: NetworkThreadPush) -> NetworkThreadPull {
    while network_thread_handle.inner.state.load(std::sync::atomic::Ordering::Acquire) != 0 {
        std::hint::spin_loop();
    }

    #[allow(unsafe_code)]
    unsafe {
        std::mem::swap(&mut *network_thread_handle.inner.push.get(), &mut req);
    }

    network_thread_handle.inner.state.store(1, std::sync::atomic::Ordering::Release);

    while network_thread_handle.inner.state.load(std::sync::atomic::Ordering::Acquire) != 0 {
        //std::hint::spin_loop();
        std::thread::yield_now();
    }

    let mut resp = NetworkThreadPull::default();

    #[allow(unsafe_code)]
    unsafe {
        std::mem::swap(&mut *network_thread_handle.inner.pull.get(), &mut resp);
    }

    resp
}

pub fn get_handshake_hash(handshake: &snow::HandshakeState) -> [u8; 64] {
    let handshake_hash_slice = handshake.get_handshake_hash();
    assert!(handshake_hash_slice.len() <= 64);
    let mut handshake_hash = [0u8; 64];
    handshake_hash[..handshake_hash_slice.len()].copy_from_slice(handshake_hash_slice);

    handshake_hash
}

use crate::helpers::*;
use crate::native_sockets::*;
use crate::nym_sockets::*;
