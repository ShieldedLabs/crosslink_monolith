#![allow(unsafe_code)]
use std::collections::HashMap;
use rand::SeedableRng;
use static_assertions::const_assert;

#[test]
pub fn bwdth_test() {
    println!("Begin the test!");

    let _handle = std::thread::spawn(|| {
        do_the_test_program(32345, None);
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    do_the_test_program(29453, Some((Ipv6Addr::LOCALHOST, 32345)));
}

#[test]
pub fn handshake_test() {
    println!("Begin the test!");

    let kp1 = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s).unwrap();
    let kp1_pub = kp1.public.clone();
    let kp1_pub2 = kp1.public.clone();
    let kp2 = new_keypair_from_connect_magic1(CONNECT_MAGIC1_PLAIN_TEXT).unwrap();
    let kp3 = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s).unwrap();

    socket_setup();
    monotonic_clock_setup();

    let _handle = std::thread::spawn(|| {
        let mut kp1_but_plaintext = kp1.clone();
        kp1_but_plaintext.magic1 = CONNECT_MAGIC1_PLAIN_TEXT;
        do_the_test_program2(32845, vec![kp1, kp1_but_plaintext], None);
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _handle = std::thread::spawn(move || {
        do_the_test_program2(29854, vec![&kp2], Some((&kp2, &STPAddress{ ip: Ipv6Addr::LOCALHOST, port: 32845, magic1: CONNECT_MAGIC1_PLAIN_TEXT, key: kp1_pub })));
    });
    do_the_test_program2(29853, vec![&kp3], Some((&kp3, &STPAddress{ ip: Ipv6Addr::LOCALHOST, port: 32845, magic1: CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s, key: kp1_pub2 })));
}

#[test]
fn contested_test() {
    println!("Begin the test!");

    let kp1 = new_keypair_from_connect_magic1(CONNECT_MAGIC1_PLAIN_TEXT).unwrap();
    let kp1_pub = kp1.public.clone();
    let kp2 = new_keypair_from_connect_magic1(CONNECT_MAGIC1_PLAIN_TEXT).unwrap();
    let kp2_pub = kp2.public.clone();

    socket_setup();
    monotonic_clock_setup();

    let _handle = std::thread::spawn(move || {
        do_the_test_program2(32845, vec![&kp1], Some((&kp1, &STPAddress{ ip: Ipv6Addr::LOCALHOST, port: 29853, magic1: CONNECT_MAGIC1_PLAIN_TEXT, key: kp2_pub })));
    });
    do_the_test_program2(29853, vec![&kp2], Some((&kp2, &STPAddress{ ip: Ipv6Addr::LOCALHOST, port: 32845, magic1: CONNECT_MAGIC1_PLAIN_TEXT, key: kp1_pub })));
}

#[test]
fn contested_encrypted_test() {
    println!("Begin the test!");

    let kp1 = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s).unwrap();
    let kp1_pub = kp1.public.clone();
    let kp2 = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s).unwrap();
    let kp2_pub = kp2.public.clone();

    socket_setup();
    monotonic_clock_setup();

    let _handle = std::thread::spawn(move || {
        do_the_test_program2(32845, vec![kp1], Some((&kp1, &STPAddress { ip: Ipv6Addr::LOCALHOST, port: 29853, magic1: CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s, key: kp2_pub })));
    });
    do_the_test_program2(29853, vec![kp2], Some((&kp2, &STPAddress { ip: Ipv6Addr::LOCALHOST, port: 32845, magic1: CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s, key: kp1_pub })));
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
pub const MAGIC2_APP_CROSSLINK: u64 = 0xc85f36d10d278812;
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

pub const fn crypto_overhead_from_connect_magic1(magic: u64) -> Option<usize> {
    match magic {
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s => Some(16),
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b => Some(16),
        CONNECT_MAGIC1_PLAIN_TEXT => Some(0),
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

pub fn connect_to(connections_map: &mut HashMap<ConnectionKey, ConnectionTrackingData>, my_keypairs: &Vec<IdentityKeyPair>, address: &STPAddress) -> Result<(), String> {
    let key = ConnectionKey {
        ip: address.ip,
        port: address.port,
        key_15_bits: load_u16(&address.key[0..2]) << 1
    };
    if connections_map.contains_key(&key) {
        return Err(format!("Error: Already connected or connecting to {:?}.", address)); // Ok(());
    }

    for keypair in my_keypairs {
        if address.magic1 == keypair.magic1 {
            connect_to_endpoint(connections_map, keypair, address);
            return Ok(());
        }
    }

    Err(format!("Error: Can't connect to given peer: {:?}. No compatible keypair.", address).to_string())
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
    Connected {
        cipher: Option<ConnectionCipherTriplet>,
        magic1: u64,
        magic2: u64,

        send_sequence_number: u64,
        jumbogram_index: u32,
        last_sent_keep_alive_time_ns: u64,
        recv_time_ns: u64,
    },
}
impl ConnectionState {
    pub fn is_connected(&self) -> bool { if let ConnectionState::Connected { .. } = self { true } else { false } }
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
    if let ConnectionState::Connected { jumbogram_index, .. } = &mut conn.connection_state {
        let id = *jumbogram_index;
        *jumbogram_index = jumbogram_index.wrapping_add(1) & (MAX_JUMBOGRAM_IDS - 1);
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
        ConnectionState::Connected                   { .. } => { return "Connected"; },
    };
    return "<INVALID>";
}

pub fn do_the_test_program2(my_port: u16, my_listen_keypairs: Vec<IdentityKeyPair>, beam_to: Option<(&IdentityKeyPair, &STPAddress)>) {
    let mut packet_memory_encrypted = new_packet_memory(); // Incoming Encrypted / Outgoing Encrypted
    let mut packet_memory_recv = new_packet_memory(); // Incoming Decrypted
    let mut packet_memory_send = new_packet_memory(); // Outgoing Decrypted

    let mut connections_map = HashMap::<ConnectionKey, ConnectionTrackingData>::new();

    let socket = setup_and_bind_udp_socket(my_port).unwrap();

    if let Some((my_connect_keypair, beam_to)) = beam_to {
        connect_to_endpoint(&mut connections_map, my_connect_keypair, beam_to);
    }

    //println!("{:#?}", connections_map);

    loop {
        service_connections(&mut connections_map, &mut Vec::new(), &Vec::new(), &mut packet_memory_encrypted, &mut packet_memory_recv, &mut packet_memory_send, socket, &my_listen_keypairs);
    }
}

pub fn connect_to_endpoint(
    connections_map: &mut HashMap::<ConnectionKey, ConnectionTrackingData>,
    my_connect_keypair: &IdentityKeyPair,
    endpoint: &STPAddress,
) {
    if my_connect_keypair.public == endpoint.key
    { return; }
    {
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
                endpoint.into(),
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
                endpoint.into(),
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
                },
            );
        }
    }
}

const        VERBOSE                :bool=0!=                (1);
const OVERLY_VERBOSE                :bool=0!=                (0);

macro_rules! pod { ($($item:item)*) => { $(#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)] $item)* }; }

#[repr(u8)] pod! { pub enum PackletTag { #[default] Acknowledgements = 0, AnEntireDatagram = 1, OneJumboFragment = 2, ReliableStreamed = 3 } }
#[repr(C)]  pod! { pub struct PackletHeader(u16); }
#[repr(C)]  pod! { pub struct PackletOneJumboFragment { pub bits: u64 } } // id:18 | total_len:23 | byte_idx:23
#[repr(C)]  pod! { pub struct PackletReliableStreamed { pub id: u32, pub seq: u32 } }

impl PackletHeader {
    pub fn new(tag: PackletTag, len: usize) -> Self { debug_assert!(len < (1 << 14)); Self((tag as u16) | ((len as u16) << 2)) }
    pub fn tag(&self) -> PackletTag { match self.0 & 0x3 { 0 => PackletTag::Acknowledgements, 1 => PackletTag::AnEntireDatagram, 2 => PackletTag::OneJumboFragment, _ => PackletTag::ReliableStreamed } }
    pub fn len(&self) -> usize { (self.0 >> 2) as usize }
}

impl PackletOneJumboFragment {
    pub fn new(id: u32, total_len: usize, byte_idx: usize) -> Self {
        debug_assert!(id        < (1 << 18));
        debug_assert!(total_len < (1 << 23));
        debug_assert!(byte_idx  < (1 << 23));
        Self { bits: (id as u64) | ((total_len as u64) << 18) | ((byte_idx as u64) << 41) }
    }
    pub fn id(&self)        -> u32 { (self.bits & ((1 << 18) - 1)) as u32 }
    pub fn total_len(&self) -> usize { ((self.bits >> 18) & ((1 << 23) - 1)) as usize }
    pub fn byte_idx(&self)  -> usize { ((self.bits >> 41) & ((1 << 23) - 1)) as usize }
}


// Jumbo fragment reassembly validation:
// exact duplicate range with exact duplicate bytes: kill connection
// exact duplicate range with different bytes: kill connection
// partial overlap: kill connection
// out of bounds past total_len: kill connection
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReassemblySlot {
    pub buf: Vec<u8>,
    pub total_len: u32,
    pub received: Vec<(u32, u32)>, // sorted non-overlapping non-adjacent ranges: [start, end)
}

impl ReassemblySlot {
    pub fn new(total_len: u32) -> Self {
        Self {
            buf: vec![0u8; total_len as usize],
            total_len,
            received: Vec::new(),
        }
    }

    // @assume_fixed_size_packets
    /// returns is_complete, new_bytes_n on Ok
    pub fn insert(&mut self, offset: usize, data: &[u8]) -> Result<(bool, usize), ()> {
        let start = offset as u32;
        if start >= self.total_len {
            if OVERLY_VERBOSE { tracing::error!("Reassembly: Start {start} was >= total_len {}.", self.total_len); }
            return Err(());
        }

        let end = (offset + data.len()) as u32;
        if end > self.total_len {
            if OVERLY_VERBOSE { tracing::error!("Reassembly: End {end} was >= total_len {}.", self.total_len); }
            return Err(());
        }

        // NOTE: I think this is ~equivalent to self.received.partition_point(|&(s, _)| s < start);
        // Binary search: find first range whose end > start (skip adjacent)
        let pos = self.received.partition_point(|&(_, e)| e <= start);

        // Check if [start, end) is fully contained in an already-received range (duplicate fragment).
        // This happens when a jumbogram ID is reused for retransmission.
        let fully_covered =
            (pos < self.received.len() && self.received[pos].0 <= start && self.received[pos].1 >= end) ||
            (pos > 0 && self.received[pos - 1].0 <= start && self.received[pos - 1].1 >= end);
        if fully_covered {
            // Verify the data matches what we already have (detect corruption / adversarial tampering)
            if self.buf[offset..offset + data.len()] == *data {
                return Ok((self.received.len() == 1 && self.received[0] == (0, self.total_len), 0));
            } else {
                if OVERLY_VERBOSE { tracing::error!("Reassembly: Fragment with range ({start}, {end}) did not match existing data."); }
                return Err(()); // same range, different data - kill connection
            }
        }

        // TODO: loop over & merge all overlapping ranges; error if non-matching
        // Check overlap with the range at `pos`
        if pos < self.received.len() && self.received[pos].0 < end {
            if OVERLY_VERBOSE { tracing::error!("Reassembly: Fragment ({start}, {end}) partially overlaps existing range ({}, {}).", self.received[pos].0, self.received[pos].1); }
            return Err(());
        }
        // Check overlap with the range before `pos`
        if pos > 0 && self.received[pos - 1].1 > start {
            if OVERLY_VERBOSE { tracing::error!("Reassembly: Fragment ({start}, {end}) partially overlaps existing range ({}, {}).", self.received[pos - 1].0, self.received[pos - 1].1); }
            return Err(());
        }

        self.buf[offset..offset + data.len()].copy_from_slice(data);

        // Insert and merge with adjacent ranges
        let merge_prev = pos > 0 && self.received[pos - 1].1 == start;
        let merge_next = pos < self.received.len() && self.received[pos].0 == end;

        match (merge_prev, merge_next) {
            (true, true) => {
                self.received[pos - 1].1 = self.received[pos].1;
                self.received.remove(pos);
            }
            (true, false) => {
                self.received[pos - 1].1 = end;
            }
            (false, true) => {
                self.received[pos].0 = start;
            }
            (false, false) => {
                self.received.insert(pos, (start, end));
            }
        }

        Ok((self.received.len() == 1 && self.received[0] == (0, self.total_len), data.len()))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct JumboReassembly {
    slots: HashMap<u32, ReassemblySlot>,
}

pub const MAX_REASSEMBLY_SLOTS: usize = 128; // @Todo: convert max slots into max bytes instead -- which is what we really want anyway!
pub const MAX_JUMBOGRAM_LEN: usize = 1 << 23; // 8 MB, matches the 23-bit field
pub const MAX_JUMBOGRAM_IDS: u32   = 1 << 18; // matches 18-bit field

impl SliceRead  for PackletHeader           { fn       read_from(buf: &mut &[u8]) -> Option<Self> { Some(Self(u16::read_from(buf)?)) } }
impl SliceWrite for PackletHeader           { fn write_to(&self, buf: &mut  [u8]) -> usize        { self.0  .write_to(buf) } }


impl SliceRead  for PackletOneJumboFragment {
    fn read_from(buf: &mut &[u8]) -> Option<Self> { Some(Self { bits: u64::read_from(buf)? }) }
}
impl SliceWrite for PackletOneJumboFragment {
    fn write_to(&self, buf: &mut [u8]) -> usize { self.bits.write_to(buf) }
}

impl SliceRead  for PackletReliableStreamed {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self {
            id:  u32::read_from(buf)?,
            seq: u32::read_from(buf)?
        })
    }
}
impl SliceWrite for PackletReliableStreamed {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.id .write_to(&mut buf[o..]);
        o += self.seq.write_to(&mut buf[o..]);
        o
    }
}

#[derive(Default)]
pub struct NetworkThreadPush {
    pub wanted_connections: Vec<(STPAddress, [u8; 64])>,
    pub messages_to_send: Vec<(ConnectionKey, Vec<u8>, Option<u32>)>,
}

#[derive(Default)]
pub struct NetworkThreadPull {
    pub current_connections: Vec<(STPAddress, [u8; 64])>,
    pub messages_received: Vec<(ConnectionKey, Vec<u8>, Option<u32>)>,
}

struct NetworkThreadInner {
    state: std::sync::atomic::AtomicUsize, // 0 empty, 1 full
    push: std::cell::UnsafeCell<NetworkThreadPush>,
    pull: std::cell::UnsafeCell<NetworkThreadPull>,
}

#[allow(unsafe_code)]
unsafe impl std::marker::Sync for NetworkThreadInner {}

// Note(Sam): Using this handle from two threads at once is UB. Only one thread may call new_service_connections at any given time.
pub struct NetworkThreadHandle {
    inner: std::sync::Arc<NetworkThreadInner>,
    thread: std::thread::JoinHandle<()>,
}

pub fn new_network_thread(my_keypairs: Vec<IdentityKeyPair>, my_port: u16) -> NetworkThreadHandle {

    // STP setup
    socket_setup();
    monotonic_clock_setup();

    let socket = setup_and_bind_udp_socket(my_port).expect("Failed to bind socket, try again.");

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

        let mut packets_to_send:  Vec<(ConnectionKey, Vec<u8>, Option<u32>)> = Vec::new();
        let mut packets_received: Vec<(ConnectionKey, Vec<u8>, Option<u32>)> = Vec::new();

        let mut connections_map = HashMap::<ConnectionKey, ConnectionTrackingData>::new();

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
                for (w, _) in &req.wanted_connections {
                    connect_to(&mut connections_map, &my_keypairs, w);
                }
                packets_to_send.extend(req.messages_to_send);
                // if packets_to_send.len() > 2_000 {
                //     packets_to_send.truncate(2_000);
                // }

                let mut resp = NetworkThreadPull::default();
                resp.current_connections = connections_map.iter().filter_map(|(key, value)| {
                    if value.is_connected() {
                        Some((value.address().clone(), value.handshake_hash))
                    } else {
                        None
                    }
                }).collect();
                std::mem::swap(&mut resp.messages_received, &mut packets_received);

                #[allow(unsafe_code)]
                unsafe {
                    std::mem::swap(&mut *thread_inner.pull.get(), &mut resp);
                }

                thread_inner.state.store(0, std::sync::atomic::Ordering::Release);
            } else {
                let got_packet = service_connections(
                    &mut connections_map,
                    &mut packets_received,
                    &mut packets_to_send,
                    &mut packet_memory_encrypted,
                    &mut packet_memory_recv,
                    &mut packet_memory_send,
                    socket,
                    &my_keypairs,
                );
                packets_to_send.clear();
                if got_packet == false { std::thread::yield_now(); }
            }
        }
    });

    NetworkThreadHandle {
        inner,
        thread,
    }
}

pub fn new_service_connections(network_thread_handle: &NetworkThreadHandle, mut req: NetworkThreadPush) -> NetworkThreadPull {
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

// Returns true if a packet was received. You should use this information to decide how to schedule your connection servicing.
pub fn service_connections(
    connections_map: &mut HashMap::<ConnectionKey, ConnectionTrackingData>,
    packets_received_this_call: &mut Vec<(ConnectionKey, Vec<u8>, Option<u32>)>,
    packets_to_send: &Vec<(ConnectionKey, Vec<u8>, Option<u32>)>,
    packet_memory_encrypted: &mut PacketMemory,
    packet_memory_recv: &mut PacketMemory,
    packet_memory_send: &mut PacketMemory,
    socket: SockHandle,
    my_listen_keypairs: &Vec<IdentityKeyPair>,
) -> bool {
    let mut result = false;

    if let Ok((buf_len, other_ip_addr, other_port, ecn_marked, ecn_enabled, service_class, timestamp_ns)) = udp_recv_with_congestion_and_dscp(socket, &mut packet_memory_encrypted[..]) {
        result = true;

        if buf_len >= 6 {
            let first_six_bytes = load_u48(&packet_memory_encrypted[0..6]);
            if first_six_bytes & 1 != 0 { // Client Hello
                // if OVERLY_VERBOSE { println!("Got a HELLO."); }
            } else {
                // if OVERLY_VERBOSE { println!("Got a non-hello."); }
            }
            if first_six_bytes & 1 != 0 { // Client Hello
                let magic1 = first_six_bytes;
                if magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
                    for key_i in 0..my_listen_keypairs.len() {
                        let my_kp = &my_listen_keypairs[key_i];
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
                    for key_i in 0..my_listen_keypairs.len() {
                        let my_kp = &my_listen_keypairs[key_i];
                        if my_kp.magic1 == magic1 {
                            let mut new_handshake = snow::Builder::new(crypto_string.parse().unwrap())
                                .prologue(&packet_memory_encrypted[0..6]).unwrap()
                                .local_private_key(&my_kp.private).unwrap()
                                .build_responder().unwrap();
                            let read_message_maybe = new_handshake.read_message(&packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]);
                            if let Ok(magic2_block_len) = read_message_maybe {
                                if let Some(client_key) = new_handshake.get_remote_static() {
                                    let Some(chosen_magic2) = negotiate_magic2(&packet_memory_recv[..magic2_block_len]) else {
                                        // if OVERLY_VERBOSE { println!("Did NOT respond to Client Hello from {:?}: magic2 negotiation failed.", (other_ip_addr, other_port)); }
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
            else { // Not client hello
                // if OVERLY_VERBOSE { println!("Not a client hello. Processing..."); }

                let mut kill_connection: Option<ConnectionKey> = None;
                'conn: {
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
                                    existing_connection.connection_state = ConnectionState::Connected {
                                        cipher: None,
                                        magic1: CONNECT_MAGIC1_PLAIN_TEXT,
                                        magic2: server_magic2,
                                        send_sequence_number: 0,
                                        jumbogram_index: 0,
                                        last_sent_keep_alive_time_ns: 0,
                                        recv_time_ns: timestamp_ns,
                                    };
                                }
                            } else {
                                if OVERLY_VERBOSE {
                                    println!("Dropping from {connection_key:?} because this was false: first_six_bytes >> 16 == 0xffff_ffff && buf_len >= 6 + 32 + 8");
                                }
                            }
                            break 'conn;
                        }
                        ConnectionState::SendingClientHello { magic1, last_sent_time_ns, hello_packet_payload, handshake } => {
                            if first_six_bytes >> 16 == 0xffff_ffff && buf_len >= 6 + 32 {
                                if let Ok(payload_len) = handshake.read_message(&packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]) {
                                    debug_assert!(handshake.is_handshake_finished());
                                    existing_connection.handshake_hash = get_handshake_hash(&handshake);
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
                                    let new_state = ConnectionState::Connected {
                                        cipher: None,
                                        magic1: *magic1,
                                        magic2: server_magic2,
                                        send_sequence_number: 0,
                                        jumbogram_index: 0,
                                        last_sent_keep_alive_time_ns: 0,
                                        recv_time_ns: timestamp_ns,
                                    };
                                    let old_state = std::mem::replace(&mut existing_connection.connection_state, new_state);

                                    let ConnectionState::SendingClientHello { handshake, .. } = old_state else { panic!(); };
                                    let ConnectionState::Connected { cipher, .. } = &mut existing_connection.connection_state else { panic!(); };
                                    *cipher = Some(ConnectionCipherTriplet::new_from_old_init_only(handshake.into_stateless_transport_mode().expect("Cannot fail given assert above.")));
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
                            existing_connection.connection_state = ConnectionState::Connected {
                                cipher: None,
                                magic1: CONNECT_MAGIC1_PLAIN_TEXT,
                                magic2: *magic2,
                                send_sequence_number: 0,
                                jumbogram_index: 0,
                                last_sent_keep_alive_time_ns: 0,
                                recv_time_ns: timestamp_ns,
                            };
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
                            existing_connection.connection_state = ConnectionState::Connected {
                                cipher: Some(cipher.clone()),
                                magic1: *magic1,
                                magic2: *magic2,
                                send_sequence_number: 0,
                                jumbogram_index: 0,
                                last_sent_keep_alive_time_ns: 0,
                                recv_time_ns: timestamp_ns,
                            };
                            // FALLTHROUGH TO CONNECTED
                        }
                        ConnectionState::Connected { .. } => (),
                    }
                    if let ConnectionState::Connected { cipher, magic1, magic2: _, send_sequence_number, jumbogram_index, last_sent_keep_alive_time_ns, recv_time_ns } = &mut existing_connection.connection_state {
                        let non_virtual_nonce = first_six_bytes >> 16; // @Todo: convert this to virtual.

                        // TODO acks etc
                        let payload;
                        if let Some(cipher) = cipher {
                            if let Ok(payload_len) = cipher.current.read_message(non_virtual_nonce, &packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]) {
                                payload = &packet_memory_recv[0..payload_len];
                            }
                            else if let Ok(payload_len) = cipher.old.read_message(non_virtual_nonce, &packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]) {
                                payload = &packet_memory_recv[0..payload_len];
                            }
                            else if let Ok(payload_len) = cipher.new.read_message(non_virtual_nonce, &packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]) {
                                payload = &packet_memory_recv[0..payload_len];
                                cipher.ratchet_forward_incoming();
                            }
                            else { break 'conn; }
                        }
                        else {
                            payload = &packet_memory_encrypted[6..buf_len];
                        }

                        *recv_time_ns = timestamp_ns;

                        // if OVERLY_VERBOSE { println!("Got data from {:?}  data: {:?}", existing_connection.other_transport_identity, payload); }

                        let mMTU_inside_udp = ASSUMED_SMALLEST_POSSIBLE_UDP_FRAME_WITH_GUARANTEED_DELIVERY;
                        let mMTU_inside_stp = mMTU_inside_udp - 6 - crypto_overhead_from_connect_magic1(*magic1).unwrap();
                        let  MTU_fragmented = mMTU_inside_stp - std::mem::size_of::<PackletHeader>();

                        let mut msg = payload;
                        while !msg.is_empty() {
                            let (tag, len) = match PackletHeader::read_from(&mut msg) {
                                Some(hdr) => if hdr.0 == 0u16 {
                                    continue
                                } else {
                                    (hdr.tag(), hdr.len())
                                }
                                None => break
                            };
                            if msg.len() < len {
                                eprintln!("truncated packlet");
                                kill_connection = Some(connection_key); break 'conn;
                            }
                            let (mut body, remainder) = msg.split_at(len);

                            match tag {
                                PackletTag::OneJumboFragment => {
                                    let Some(frag) = PackletOneJumboFragment::read_from(&mut body)
                                    else {
                                        eprintln!("truncated jumbogram fragment");
                                        kill_connection = Some(connection_key); break 'conn;
                                    };
                                    let frag_data = body;

                                    if OVERLY_VERBOSE {
                                        println!("Jumbo fragment: id={} total_len={} byte_idx={} frag_len={}",
                                                 frag.id(), frag.total_len(), frag.byte_idx(), frag_data.len());
                                    }

                                    let frag_id   = frag.id();
                                    let total_len = frag.total_len();
                                    let byte_idx  = frag.byte_idx();

                                    if total_len <= MTU_fragmented {
                                        eprintln!("jumbogram fragment is so small that it should have been a datagram");
                                        kill_connection = Some(connection_key); break 'conn;
                                    }
                                    if total_len > MAX_JUMBOGRAM_LEN {
                                        eprintln!("jumbogram is bigger than the biggest allowed jumbogram");
                                        kill_connection = Some(connection_key); break 'conn;
                                    }
                                    if frag_data.is_empty() {
                                        eprintln!("jumbogram fragment is empty");
                                        kill_connection = Some(connection_key); break 'conn;
                                    }
                                    if frag_data.len() >= total_len {
                                        eprintln!("jumbogram fragment is bigger than the entire jumbogram (\"TARDIS jumbogram\")");
                                        kill_connection = Some(connection_key); break 'conn;
                                    }

                                    let reasm = &mut existing_connection.jumbo_reassembly;

                                    // Create slot if needed, evicting oldest if at capacity
                                    if !reasm.slots.contains_key(&frag_id) {
                                        if reasm.slots.len() >= MAX_REASSEMBLY_SLOTS {
                                            let mask = MAX_JUMBOGRAM_IDS - 1;
                                            let oldest = *reasm.slots.keys().max_by_key(|&&k| (frag_id.wrapping_sub(k)) & mask).unwrap();
                                            reasm.slots.remove(&oldest);
                                        }
                                        reasm.slots.insert(frag_id, ReassemblySlot::new(total_len as u32));
                                    }

                                    let slot = reasm.slots.get_mut(&frag_id).unwrap();

                                    // Validate total_len matches what we saw on the first fragment
                                    if slot.total_len != total_len as u32 {
                                        eprintln!("total_len mismatch, killing connection");
                                        kill_connection = Some(connection_key); break 'conn;
                                    }

                                    match slot.insert(byte_idx, frag_data) {
                                        Ok((true, _new_bytes_n)) => {
                                            // Message complete - deliver it
                                            let completed = reasm.slots.remove(&frag_id).unwrap();
                                            packets_received_this_call.push((connection_key, completed.buf, Some(frag_id)));
                                        }
                                        Ok((false, _)) => {
                                            // More fragments needed
                                        }
                                        Err(()) => {
                                            eprintln!("overlapping/out-of-bounds fragment, killing connection");
                                            kill_connection = Some(connection_key); break 'conn;
                                        }
                                    }
                                }
                                PackletTag::AnEntireDatagram => {
                                    packets_received_this_call.push((connection_key, body.to_vec(), None));
                                }
                                PackletTag::ReliableStreamed => { eprintln!("TODO {:?}", tag); kill_connection = Some(connection_key); break 'conn; }
                                PackletTag::Acknowledgements => { eprintln!("TODO {:?}", tag); kill_connection = Some(connection_key); break 'conn; }
                            }

                            msg = remainder;
                        }
                    }
                }
                if let Some(key) = kill_connection {
                    if OVERLY_VERBOSE {
                        eprintln!("Killing {key:?}.");
                    }
                    connections_map.remove(&key);
                }
            }
        }
    }

    let current_time_now_ns = monotonic_clock_ns();
    connections_map.retain(|connection_key, connection_tracking_data| {match &mut connection_tracking_data.connection_state {
        ConnectionState::SendingClientHelloPlaintext { last_sent_time_ns, hello_packet_payload } => {
            if *last_sent_time_ns + 2_500_000_000 < current_time_now_ns {
                udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21);
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
                udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21);
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
                udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21);
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
                udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21);
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
        ConnectionState::Connected { magic1, magic2, cipher, send_sequence_number, jumbogram_index, last_sent_keep_alive_time_ns, recv_time_ns } => {
            debug_assert!(*magic2 != 0, "Connection entered Connected state without negotiating magic2");
            if *last_sent_keep_alive_time_ns + 5_000_000_000 < current_time_now_ns {
                let virtual_nonce = *send_sequence_number;
                store_u16(&mut packet_memory_encrypted[0..2], connection_tracking_data.two_byte_send_prefix);
                store_u32(&mut packet_memory_encrypted[2..6], virtual_nonce as u32);
                *send_sequence_number += 1;

                let packet_len;
                if let Some(cipher) = cipher {
                    packet_len = cipher.current.write_message(virtual_nonce, &[], &mut packet_memory_encrypted[6..]).unwrap();
                }
                else {
                    packet_len = 0;
                }

                udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_encrypted[0..6+packet_len], Dscp::Af21);
                *last_sent_keep_alive_time_ns = current_time_now_ns;
                return true;
            }
            if *recv_time_ns + 15_000_000_000 < current_time_now_ns {
                if VERBOSE { println!("Disconnected from: {:?}.", connection_tracking_data.other_transport_identity); }
                return false; // connection timeout
            }
        }
    } true});


    for (connection_key, data, jumbo_id_override) in packets_to_send {
        let Some(ref mut connection) = connections_map.get_mut(connection_key)
        else {
            continue;
        };

        let ConnectionState::Connected { magic1, magic2: _, cipher, send_sequence_number, jumbogram_index, last_sent_keep_alive_time_ns, recv_time_ns } = &mut connection.connection_state
        else {
            continue;
        };

        // mMTU == minimum MTU
        let mMTU_inside_udp = ASSUMED_SMALLEST_POSSIBLE_UDP_FRAME_WITH_GUARANTEED_DELIVERY;
        let mMTU_inside_stp = mMTU_inside_udp - 6 - crypto_overhead_from_connect_magic1(*magic1).unwrap();
        let  MTU_fragmented = mMTU_inside_stp - std::mem::size_of::<PackletHeader>();

        let mut send = |payload: &[u8]| {
            let mut o = 0;

            let virtual_nonce = *send_sequence_number;
            o += connection.two_byte_send_prefix.write_to(&mut packet_memory_encrypted[o..]);
            o += (virtual_nonce as u32)         .write_to(&mut packet_memory_encrypted[o..]);
            *send_sequence_number += 1;

            if let Some(cipher) = cipher {
                o += cipher.current.write_message(virtual_nonce, payload, &mut packet_memory_encrypted[o..]).unwrap();
            }
            else {
                o += payload.write_to(&mut packet_memory_encrypted[o..]);
            }

            assert!(o == mMTU_inside_udp);

            udp_send_with_congestion_and_dscp(socket, connection.other_ip, connection.other_port, &packet_memory_encrypted[..o], Dscp::BestEffort);
        };

        if data.len() > MTU_fragmented {
            let frag_mtu = MTU_fragmented - std::mem::size_of::<PackletOneJumboFragment>();
            let jumbo_id = match jumbo_id_override {
                Some(id) => {
                    debug_assert!(*id < MAX_JUMBOGRAM_IDS, "jumbogram ID override {id} out of range (max {})", MAX_JUMBOGRAM_IDS);
                    *id & (MAX_JUMBOGRAM_IDS - 1)
                }
                None => {
                    let id = *jumbogram_index;
                    *jumbogram_index = jumbogram_index.wrapping_add(1) & (MAX_JUMBOGRAM_IDS - 1);
                    id
                }
            };
            let mut shuffle_vec: Vec<_> = data.chunks(frag_mtu).enumerate().collect();
            use rand::seq::SliceRandom;
            shuffle_vec.shuffle(&mut rand::thread_rng());
            for (i, fragment) in shuffle_vec {
                let offset = i * frag_mtu;
                let hdr  = PackletHeader::new(PackletTag::OneJumboFragment, std::mem::size_of::<PackletOneJumboFragment>() + fragment.len());
                let frag = PackletOneJumboFragment::new(jumbo_id, data.len(), offset);

                let mut o = 0;
                o += hdr     .write_to(&mut packet_memory_send[o..]);
                o += frag    .write_to(&mut packet_memory_send[o..]);
                o += fragment.write_to(&mut packet_memory_send[o..]);
                if o < mMTU_inside_stp {
                    packet_memory_send[o..mMTU_inside_stp].fill(0); // @Todo: real packlet framing
                }

                send(&packet_memory_send[..mMTU_inside_stp]);
            }
        } else {
            let hdr  = PackletHeader::new(PackletTag::AnEntireDatagram, data.len());

            let mut o = 0;
            o += hdr .write_to(&mut packet_memory_send[o..]);
            o += data.write_to(&mut packet_memory_send[o..]);
            if o < mMTU_inside_stp {
                packet_memory_send[o..mMTU_inside_stp].fill(0); // @Todo: real packlet framing
            }

            send(&packet_memory_send[..mMTU_inside_stp]);
        }
    }

    result
}

pub fn do_the_test_program(port: u16, beam_to: Option<(Ipv6Addr, u16)>) {
    socket_setup();
    monotonic_clock_setup();

    let mut time_of_last_status_print = std::time::Instant::now();
    let mut ecn_up = false;
    let mut ecn_down = false;

    struct SendState {
        socket: SockHandle,
        drop_cursor: u64,
        serial_number: u64,
        packet_buffer: Vec<u32>,
    }
    let mut send_state = SendState {
        socket: setup_and_bind_udp_socket(port).unwrap(),
        drop_cursor: 50,
        serial_number: 50,
        packet_buffer: vec![0; PACKET_HISTORY_BUFFER_LEN],
    };

    let mut bytes_on_the_wire = 0_u64;

    let mut min_seen_rtt_buckets = [u64::MAX; 10];
    let mut rtt_bucket_cursor = 0_u64;
    let mut rtt_bucket_cursor_last_time = 0_u64;

    let mut bytes_delivered_buckets = [0_u64; 20];
    let mut bytes_delivered_bucket_cursor = 0_u64;
    let mut bytes_delivered_bucket_cursor_last_time = 0_u64;

    let mut state_machine_cursor = 0_u64;
    let mut state_machine_cursor_last_time = 0_u64;
    let mut old_measured_allowed_bytes_on_the_wire = 0_u64;


    let mut buf = [0_u8; 16384];


    struct AckState {
        // temp
        saved_other_ip_addr: Ipv6Addr,
        saved_other_port: u16,

        acks_in_waiting_min: u64,
        acks_in_waiting_buf: [(u64, bool); ASSUMED_ACK_CAPACITY],
        acks_in_waiting_count: usize,
        first_waiting_ack_time_ns: u64,

        ack_send_buf: [u8; 8 + ASSUMED_DELIVERY_INNER_PAYLOAD_SIZE],
    }
    let mut ack_state = AckState {
        saved_other_ip_addr: Ipv6Addr::LOCALHOST,
        saved_other_port: 0,

        acks_in_waiting_min: 0,
        acks_in_waiting_buf: [(0, false); ASSUMED_ACK_CAPACITY],
        acks_in_waiting_count: 0,
        first_waiting_ack_time_ns: 0,

        ack_send_buf: [0; 8 + ASSUMED_DELIVERY_INNER_PAYLOAD_SIZE],
    };
    pub fn send_acks_helper(ack_state: &mut AckState, send_state: &mut SendState, ecn_down: bool) {
        assert!(ack_state.acks_in_waiting_count > 0);

        if send_state.serial_number + 1 >= send_state.drop_cursor + (PACKET_HISTORY_BUFFER_LEN as u64) {
            eprintln!("Error! PACKET_HISTORY_BUFFER_LEN is too small.\n");
            return;
        }

        store_u64(&mut ack_state.ack_send_buf[0..8], send_state.serial_number);
        ack_state.ack_send_buf[8] = 2;
        let mut o = 9;
        store_u64(&mut ack_state.ack_send_buf[o..o+8], (ack_state.acks_in_waiting_min & 0x7fff_ffff_ffff_ffff) | ((ecn_down as u64) << 63));
        o += 8;
        for i in 0..ack_state.acks_in_waiting_count {
            let val = ((ack_state.acks_in_waiting_buf[i].0 - ack_state.acks_in_waiting_min) as u32 & 0x7f_ffff) | ((ack_state.acks_in_waiting_buf[i].1 as u32) << 23);
            store_u24(&mut ack_state.ack_send_buf[o..o+3], val);
            o += 3;
        }
        let res = udp_send_with_congestion_and_dscp(send_state.socket, ack_state.saved_other_ip_addr, ack_state.saved_other_port, &ack_state.ack_send_buf[0..o], Dscp::Af21);
        if let Ok(_timestamp_ns) = res {
            send_state.packet_buffer[send_state.serial_number as usize % PACKET_HISTORY_BUFFER_LEN] = u32::MAX;
            send_state.serial_number += 1;
        }
        ack_state.acks_in_waiting_count = 0;
    };


    loop {
        // Send non full ack packet if needed.
        if ack_state.acks_in_waiting_count > 0 && monotonic_clock_ns() - ack_state.first_waiting_ack_time_ns > MAX_WAIT_BEFORE_SENDING_NON_FULL_ACK {
            send_acks_helper(&mut ack_state, &mut send_state, ecn_down);
        }

        let current_min_rtt_on_connection_ns = {
            let a = min_seen_rtt_buckets[0].min(min_seen_rtt_buckets[1]);
            let b = min_seen_rtt_buckets[2].min(min_seen_rtt_buckets[3]);
            let c = min_seen_rtt_buckets[4].min(min_seen_rtt_buckets[5]);
            let d = min_seen_rtt_buckets[6].min(min_seen_rtt_buckets[7]);
            let e = min_seen_rtt_buckets[8].min(min_seen_rtt_buckets[9]);
            a.min(b).min(c).min(d).min(e)
                .min(10_000_000_000) // RTT assumed to be always less than 10 seconds.
                .max(10_000) // The maths breaks down with RTT close to zero so we pad up to 10 us always.
        };
        let current_max_delivered_bucket_bytes = {
            let a = bytes_delivered_buckets[0].max(bytes_delivered_buckets[1]);
            let b = bytes_delivered_buckets[2].max(bytes_delivered_buckets[3]);
            let c = bytes_delivered_buckets[4].max(bytes_delivered_buckets[5]);
            let d = bytes_delivered_buckets[6].max(bytes_delivered_buckets[7]);
            let e = bytes_delivered_buckets[8].max(bytes_delivered_buckets[9]);
            let f = bytes_delivered_buckets[10].max(bytes_delivered_buckets[11]);
            let g = bytes_delivered_buckets[12].max(bytes_delivered_buckets[13]);
            let h = bytes_delivered_buckets[14].max(bytes_delivered_buckets[15]);
            let i = bytes_delivered_buckets[16].max(bytes_delivered_buckets[17]);
            let j = bytes_delivered_buckets[18].max(bytes_delivered_buckets[19]);
            a.max(b).max(c).max(d).max(e).max(f).max(g).max(h).max(i).max(j)
        };
        let data_delivery_bucket_time = current_min_rtt_on_connection_ns / 4;

        let tu_bytes = 0_u64.max(ASSUMED_DELIVERY_INNER_PAYLOAD_SIZE as u64);

        let bottleneck_bandwidth_Bps = (current_max_delivered_bucket_bytes*1_000_000_000) / data_delivery_bucket_time;
        let measured_allowed_bytes_on_the_wire = (((bottleneck_bandwidth_Bps as u128 * current_min_rtt_on_connection_ns as u128) / 1_000_000_000) as u64).max(tu_bytes);

        let drop_back_edge_timestamp_ns = monotonic_clock_ns();

        if drop_back_edge_timestamp_ns > state_machine_cursor_last_time + data_delivery_bucket_time {
            state_machine_cursor += 1;
            state_machine_cursor_last_time = drop_back_edge_timestamp_ns;
        }
        if measured_allowed_bytes_on_the_wire > old_measured_allowed_bytes_on_the_wire*124/100 { state_machine_cursor = 0; println!("GROW"); }
        old_measured_allowed_bytes_on_the_wire = measured_allowed_bytes_on_the_wire;
        if state_machine_cursor >= 12 { state_machine_cursor = 2; }

        let allowed_bytes_on_the_wire =
            if state_machine_cursor < 2 { (measured_allowed_bytes_on_the_wire*130/100).max(measured_allowed_bytes_on_the_wire + tu_bytes*10) }
            else if state_machine_cursor < 10 { measured_allowed_bytes_on_the_wire }
            else { (measured_allowed_bytes_on_the_wire*125/100).max(measured_allowed_bytes_on_the_wire + tu_bytes) };

        if time_of_last_status_print.elapsed() > std::time::Duration::from_millis(1000) {
            time_of_last_status_print = std::time::Instant::now();
            println!("ecn up/down:{}/{} rtt: {} us MaxBucket: {} B bottleneck bandwidth: {}", ecn_up as u8, ecn_down as u8, current_min_rtt_on_connection_ns / 1000, current_max_delivered_bucket_bytes, BytesPerSecond(bottleneck_bandwidth_Bps));
            //println!("{} < m: {} t: {}", bytes_on_the_wire, measured_allowed_bytes_on_the_wire, allowed_bytes_on_the_wire);
        }

        while send_state.drop_cursor < send_state.serial_number { // The drop back edge.
            let stored_int = send_state.packet_buffer[send_state.drop_cursor as usize % PACKET_HISTORY_BUFFER_LEN];
            if stored_int != u32::MAX {
                let (packet_size_bytes, send_timestamp_ns, _ecn_marked, acked) = decompress_packet_info(stored_int);
                let time_since_send_ns = subtract_22_bit_timestamps_with_a_known_more_recent(drop_back_edge_timestamp_ns, send_timestamp_ns);
                if time_since_send_ns < data_delivery_bucket_time * bytes_delivered_buckets.len() as u64 { break; }
                if acked == false {
                    bytes_on_the_wire -= packet_size_bytes as u64;
                }
            }
            send_state.drop_cursor += 1;
        }

        let mut cannot_send_should_sleep = false;
        if send_state.serial_number + 1 >= send_state.drop_cursor + (PACKET_HISTORY_BUFFER_LEN as u64) {
            eprintln!("Error! PACKET_HISTORY_BUFFER_LEN is too small.\n");
            cannot_send_should_sleep = true;
        }
        else {
            let to_send_len_compressed = decompress_packet_size_to_8_bits(compress_packet_size_to_8_bits(tu_bytes as u16)) as u64;
            if to_send_len_compressed + bytes_on_the_wire <= allowed_bytes_on_the_wire {
                if let Some((beam_to_ip, beam_to_port)) = beam_to {
                    store_u64(&mut buf[0..8], send_state.serial_number);
                    buf[8] = 1;
                    let packet_size = tu_bytes as usize;
                    let res = udp_send_with_congestion_and_dscp(send_state.socket, beam_to_ip, beam_to_port, &buf[0..8+packet_size], Dscp::Af11);
                    if let Ok(timestamp_ns) = res {
                        send_state.packet_buffer[send_state.serial_number as usize % PACKET_HISTORY_BUFFER_LEN] = compress_packet_info(packet_size as u16, timestamp_ns, false, false);
                        send_state.serial_number += 1;
                        bytes_on_the_wire += to_send_len_compressed;
                    }
                } else {
                    cannot_send_should_sleep = true;
                }
            } else {
                cannot_send_should_sleep = true;
            }
        }

        let res = udp_recv_with_congestion_and_dscp(send_state.socket, &mut buf);
        if matches!(res, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock) { continue; }
        //println!("{}: res = {:?}", port, res);
        if res.is_err() {
            if cannot_send_should_sleep { std::thread::yield_now(); }
            continue;
        }
        let (buf_len, other_ip_addr, other_port, ecn_marked, _ecn_enabled, _service_class, timestamp_ns) = res.unwrap();
        ecn_down = _ecn_enabled;
        if buf_len < 8 { continue; }
        let packet_serial = load_u64(&buf[0..8]);
        let packet_plaintext = &buf[8..buf_len];

        if packet_plaintext[0] == 2 {
            if packet_plaintext.len() < 1+8+3 || (packet_plaintext.len()-1-8) % 3 != 0 {
                eprintln!("Error! Bad Ack. data = {}\n", hex::encode(packet_plaintext));
                continue;
            }
            let mut min_rtt_this_ack = u64::MAX;
            let mut total_bytes_acked_this_ack = 0_u64;

            let ack_base_and_ecn_info = load_u64(&packet_plaintext[1..9]);
            let ack_base = ack_base_and_ecn_info & 0x7fff_ffff_ffff_ffff;
            ecn_up = ack_base_and_ecn_info & (1 << 63) != 0;
            let mut o = 9;
            while o < packet_plaintext.len() {
                let val = load_u24(&packet_plaintext[o..o+3]);
                o += 3;
                let ecn_marked = val & 0x80_0000 != 0;
                let ack_number = ack_base + (val & 0x7f_ffff) as u64;
                if ack_number >= send_state.serial_number {
                    eprintln!("Error! Ack number out of range. Too new. {}\n", ack_number);
                    continue;
                }
                if ack_number + (PACKET_HISTORY_BUFFER_LEN as u64) < send_state.serial_number {
                    eprintln!("Error! Ack number out of range. Too old for buffer. {}\n", ack_number);
                    continue;
                }
                let (packet_size_bytes, send_timestamp_ns, is_mtu_poll, acked) = decompress_packet_info(send_state.packet_buffer[ack_number as usize % PACKET_HISTORY_BUFFER_LEN]);
                if acked {
                    eprintln!("Error! Already recieved ack for {}\n", ack_number);
                    continue;
                }
                send_state.packet_buffer[ack_number as usize % PACKET_HISTORY_BUFFER_LEN] |= 1;
                let rtt_ns = subtract_22_bit_timestamps_with_a_known_more_recent(timestamp_ns, send_timestamp_ns);
                min_rtt_this_ack = min_rtt_this_ack.min(rtt_ns);
                if ack_number >= send_state.drop_cursor {
                    total_bytes_acked_this_ack += packet_size_bytes as u64;
                }

                if ecn_marked { println!("ECN"); }
            }
            if total_bytes_acked_this_ack > 0 {
                bytes_on_the_wire -= total_bytes_acked_this_ack;

                let current_time_ns = monotonic_clock_ns();

                if current_time_ns > rtt_bucket_cursor_last_time + 1_000_000_000 {
                    rtt_bucket_cursor += 1;
                    min_seen_rtt_buckets[rtt_bucket_cursor as usize % min_seen_rtt_buckets.len()] = u64::MAX;
                    rtt_bucket_cursor_last_time = current_time_ns;
                }
                min_seen_rtt_buckets[rtt_bucket_cursor as usize % min_seen_rtt_buckets.len()] = min_seen_rtt_buckets[rtt_bucket_cursor as usize % min_seen_rtt_buckets.len()].min(min_rtt_this_ack);

                if current_time_ns > bytes_delivered_bucket_cursor_last_time + data_delivery_bucket_time {
                    bytes_delivered_bucket_cursor += 1;
                    bytes_delivered_buckets[bytes_delivered_bucket_cursor as usize % bytes_delivered_buckets.len()] = 0;
                    bytes_delivered_bucket_cursor_last_time = current_time_ns;
                }
                bytes_delivered_buckets[bytes_delivered_bucket_cursor as usize % bytes_delivered_buckets.len()] += total_bytes_acked_this_ack;
            }
        }
        else {
            if ecn_marked { println!("ECN!"); }

            ack_state.saved_other_ip_addr = other_ip_addr;
            ack_state.saved_other_port = other_port;

            if ack_state.acks_in_waiting_count == 0 {
                ack_state.acks_in_waiting_min = packet_serial;
                ack_state.first_waiting_ack_time_ns = timestamp_ns;
            }
            else {
                ack_state.acks_in_waiting_min = ack_state.acks_in_waiting_min.min(packet_serial);
            }
            ack_state.acks_in_waiting_buf[ack_state.acks_in_waiting_count] = (packet_serial, ecn_marked);
            ack_state.acks_in_waiting_count += 1;
            if ack_state.acks_in_waiting_count == ASSUMED_ACK_CAPACITY || monotonic_clock_ns() - ack_state.first_waiting_ack_time_ns > MIN_WAIT_BEFORE_SENDING_NON_FULL_ACK {
                send_acks_helper(&mut ack_state, &mut send_state, ecn_down);
            }
        }
    }
}

const ASSUMED_DELIVERY_INNER_PAYLOAD_SIZE: usize = ASSUMED_SMALLEST_POSSIBLE_UDP_FRAME_WITH_GUARANTEED_DELIVERY - 8;
const ASSUMED_ACK_CAPACITY: usize = (ASSUMED_DELIVERY_INNER_PAYLOAD_SIZE-1-8)/3;

const MIN_WAIT_BEFORE_SENDING_NON_FULL_ACK: u64 = 5_000_000;
const MAX_WAIT_BEFORE_SENDING_NON_FULL_ACK: u64 = 20_000_000;

const PACKET_HISTORY_BUFFER_LEN: usize = 1048576;

#[inline]
pub fn compress_packet_info(packet_size_bytes: u16, timestamp_ns: u64, is_mtu_poll: bool, acked: bool) -> u32 {
    let size8 = compress_packet_size_to_8_bits(packet_size_bytes) as u32;
    let ts22  = ((compress_timestamp_to_22_bits(timestamp_ns) >> 13) as u32) & ((1u32 << 22) - 1);
    (size8 << 24) | (ts22 << 2) | ((is_mtu_poll as u32) << 1) | (acked as u32)
}
#[inline]
pub fn decompress_packet_info(x: u32) -> (u16, u64, bool, bool) {
    let size8 = (x >> 24) as u8;
    let ts22  = (x >> 2) & ((1u32 << 22) - 1);
    let is_mtu_poll   = ((x >> 1) & 1) != 0;
    let ack   = (x & 1) != 0;

    let packet_size_bytes = decompress_packet_size_to_8_bits(size8);
    let timestamp_ns_quantized = (ts22 as u64) << 13;

    (packet_size_bytes, timestamp_ns_quantized, is_mtu_poll, ack)
}

#[inline]
pub fn subtract_22_bit_timestamps_with_a_known_more_recent(mut recent: u64, mut old: u64) -> u64 {
    const ROUND_MASK: u64 = 0x1fff;                 // clear low 13 bits
    const KEEP_MASK:  u64 = 0x0000_0007_ffff_ffff;  // keep low 35 bits
    const MOD:        u64 = 0x8_0000_0000;            // 1 << 35

    recent = recent.wrapping_add(ROUND_MASK) & !ROUND_MASK;
    recent &= KEEP_MASK;

    old = old.wrapping_add(ROUND_MASK) & !ROUND_MASK;
    old &= KEEP_MASK;

    recent = recent.wrapping_add(((recent < old) as u64) * MOD);
    recent.wrapping_sub(old)
}
#[inline]
pub fn compress_timestamp_to_22_bits(mut n: u64) -> u64 {
    const ROUND_MASK: u64 = 0x1fff;
    const KEEP_MASK:  u64 = 0x0000_0007_ffff_ffff;

    n = n.wrapping_add(ROUND_MASK) & !ROUND_MASK;
    n & KEEP_MASK
}

#[inline]
pub fn compress_packet_size_to_8_bits(n: u16) -> u8 {
    const BASE: u16 = 200;
    const K: [u16; 8] = [16, 48, 128, 384, 768, 1408, 3136, 6656];

    // remainder we need to represent as a subset-sum of K
    let mut rem = n.saturating_sub(BASE);

    // Greedy works because K is superincreasing.
    let mut out: u8 = 0;

    // i = 7 ..= 0
    let t7 = (rem >= K[7]) as u16; rem = rem.wrapping_sub(t7 * K[7]); out |= (t7 as u8) << 7;
    let t6 = (rem >= K[6]) as u16; rem = rem.wrapping_sub(t6 * K[6]); out |= (t6 as u8) << 6;
    let t5 = (rem >= K[5]) as u16; rem = rem.wrapping_sub(t5 * K[5]); out |= (t5 as u8) << 5;
    let t4 = (rem >= K[4]) as u16; rem = rem.wrapping_sub(t4 * K[4]); out |= (t4 as u8) << 4;
    let t3 = (rem >= K[3]) as u16; rem = rem.wrapping_sub(t3 * K[3]); out |= (t3 as u8) << 3;
    let t2 = (rem >= K[2]) as u16; rem = rem.wrapping_sub(t2 * K[2]); out |= (t2 as u8) << 2;
    let t1 = (rem >= K[1]) as u16; rem = rem.wrapping_sub(t1 * K[1]); out |= (t1 as u8) << 1;
    let t0 = (rem >= K[0]) as u16; rem = rem.wrapping_sub(t0 * K[0]); out |= (t0 as u8) << 0;

    out
}

#[inline]
pub fn decompress_packet_size_to_8_bits(n: u8) -> u16 {
    const BASE: u16 = 200;
    const K: [u16; 8] = [16, 48, 128, 384, 768, 1408, 3136, 6656];

    let mut size = BASE;

    // size = BASE + Σ K[i] * bit(i)
    size += K[0] * ((n >> 0) & 1) as u16;
    size += K[1] * ((n >> 1) & 1) as u16;
    size += K[2] * ((n >> 2) & 1) as u16;
    size += K[3] * ((n >> 3) & 1) as u16;
    size += K[4] * ((n >> 4) & 1) as u16;
    size += K[5] * ((n >> 5) & 1) as u16;
    size += K[6] * ((n >> 6) & 1) as u16;
    size += K[7] * ((n >> 7) & 1) as u16;

    size
}

use crate::helpers::*;
use crate::native_sockets::*;
