
use std::collections::HashMap;

#[test]
fn bwdth_test() {
    println!("Begin the test!");

    let _handle = std::thread::spawn(|| {
        do_the_test_program(32345, None);
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    do_the_test_program(29453, Some((Ipv6Addr::LOCALHOST, 32345)));
}

#[test]
fn handshake_test() {
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
        do_the_test_program2(32845, kp1.clone(), vec![kp1, kp1_but_plaintext], None);
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _handle = std::thread::spawn(|| {
        do_the_test_program2(29854, kp2.clone(), vec![kp2], Some((Ipv6Addr::LOCALHOST, 32845, CONNECT_MAGIC1_PLAIN_TEXT, kp1_pub)));
    });
    do_the_test_program2(29853, kp3.clone(), vec![kp3], Some((Ipv6Addr::LOCALHOST, 32845, CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s, kp1_pub2)));
}

// LSB of this 48 bit value must be 1 for this to be recognized as an incoming connect handshake.
const CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s: u64 = 0x7193_c304_f8d5;
const CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b: u64 = 0xbe53_b364_1ce1;
const CONNECT_MAGIC1_PLAIN_TEXT: u64 = 0x5bb2_2856_ae53;
fn noise_string_from_connect_magic1(magic: u64) -> Option<&'static str> {
    match magic {
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s => Some("Noise_IK_25519_ChaChaPoly_BLAKE2s"),
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b => Some("Noise_IK_25519_ChaChaPoly_BLAKE2b"),
        CONNECT_MAGIC1_PLAIN_TEXT => Some("plaintext"),
        _ => None,
    }
}

fn new_keypair_from_connect_magic1(magic1: u64) -> Option<IdentityKeyPair> {
    if magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
        let mut ret = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s).unwrap();
        ret.magic1 = CONNECT_MAGIC1_PLAIN_TEXT;
        return Some(ret);
    }
    if let Some(noise_string) = noise_string_from_connect_magic1(magic1) {
        let kp = snow::Builder::new(noise_string.parse().unwrap()).generate_keypair().unwrap();
        Some(IdentityKeyPair { magic1, private: kp.private, public: kp.public })
    } else { None }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdentityKeyPair {
    pub magic1: u64,
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionKey {
    ip: Ipv6Addr,
    port: u16,
    key_15_bits: u16, // LSB is just always 1.
}

#[derive(Debug)]
pub struct ConnectionTrackingData {
    creation_time_ns: u64,
    my_ip: Ipv6Addr,
    my_transport_identity_keypair: IdentityKeyPair,
    two_byte_send_prefix: u16,
    other_ip: Ipv6Addr,
    other_port: u16,
    other_transport_identity: Vec<u8>,
    connection_state: ConnectionState,
}

#[derive(Debug)]
pub enum ConnectionState {
    SendingClientHelloPlaintext {
        last_sent_time_ns: u64,
        hello_packet_payload: Vec<u8>,
    },
    SendingClientHello {
        last_sent_time_ns: u64,
        hello_packet_payload: Vec<u8>,
        handshake: snow::HandshakeState,
    },
    SendingServerHelloPlaintext {
        last_sent_time_ns: u64,
        hello_packet_payload: Vec<u8>,
    },
    Connected {
        handshake: Option<snow::HandshakeState>,
        send_sequence_number: u64,
        last_sent_keep_alive_time_ns: u64,
    },
}

pub fn do_the_test_program2(my_port: u16, my_connect_keypair: IdentityKeyPair, my_listen_keypairs: Vec<IdentityKeyPair>, beam_to: Option<(Ipv6Addr, u16, u64, Vec<u8>)>) {
    let mut packet_memory_encrypted = new_packet_memory(); // Incoming Encrypted / Outgoing Encrypted
    let mut packet_memory_recv = new_packet_memory(); // Incoming Decrypted
    let mut packet_memory_send = new_packet_memory(); // Outgoing Decrypted
    
    let mut connections_map = HashMap::<ConnectionKey, ConnectionTrackingData>::new();

    let socket = setup_and_bind_udp_socket(my_port);
    if let Some(beam_to) = beam_to {
        assert!(my_connect_keypair.magic1 == beam_to.2);

        // TODO list of supported Application Level protocols for e.g. zcash network upgrades.
        // packet_memory_send
        let list_of_protocols_len_bytes = 0;

        store_u48(&mut packet_memory_encrypted[0..6], my_connect_keypair.magic1);

        if my_connect_keypair.magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
            assert_eq!(my_connect_keypair.public.len(), 32);
            packet_memory_encrypted[6..6+32].copy_from_slice(&my_connect_keypair.public[..]);
            packet_memory_encrypted[6+32..6+32+list_of_protocols_len_bytes].copy_from_slice(&packet_memory_send[0..list_of_protocols_len_bytes]);
            let hello_packet_payload = Vec::from(&packet_memory_encrypted[0..6+32+list_of_protocols_len_bytes]);
            
            let my_ip = if beam_to.0.to_ipv4_mapped().is_some() { Ipv6Addr::UNSPECIFIED } else { Ipv6Addr::UNSPECIFIED };
            connections_map.insert(
                ConnectionKey { ip: beam_to.0, port: beam_to.1, key_15_bits: load_u16(&beam_to.3[0..2]) << 1 },
                ConnectionTrackingData {
                    creation_time_ns: monotonic_clock_ns(),
                    my_ip,
                    my_transport_identity_keypair: my_connect_keypair.clone(),
                    two_byte_send_prefix: load_u16(&my_connect_keypair.public[0..2]) << 1,
                    other_ip: beam_to.0,
                    other_port: beam_to.1,
                    other_transport_identity: beam_to.3.clone(),
                    connection_state: ConnectionState::SendingClientHelloPlaintext { last_sent_time_ns: 0, hello_packet_payload },
                },
            );
        }
        else {
            let mut handshake = snow::Builder::new(noise_string_from_connect_magic1(my_connect_keypair.magic1).unwrap().parse().unwrap())
                .prologue(&packet_memory_encrypted[0..6]).unwrap()
                .local_private_key(&my_connect_keypair.private[..]).unwrap()
                .remote_public_key(&beam_to.3[..]).unwrap()
                .build_initiator().unwrap();
            let handshake_size = handshake.write_message(&packet_memory_send[0..list_of_protocols_len_bytes], &mut packet_memory_encrypted[6..]).unwrap();
            let hello_packet_payload = Vec::from(&packet_memory_encrypted[0..6+handshake_size]);
            
            let my_ip = if beam_to.0.to_ipv4_mapped().is_some() { Ipv6Addr::UNSPECIFIED } else { Ipv6Addr::UNSPECIFIED };
            connections_map.insert(
                ConnectionKey { ip: beam_to.0, port: beam_to.1, key_15_bits: load_u16(&beam_to.3[0..2]) << 1 },
                ConnectionTrackingData {
                    creation_time_ns: monotonic_clock_ns(),
                    my_ip,
                    my_transport_identity_keypair: my_connect_keypair.clone(),
                    two_byte_send_prefix: load_u16(&my_connect_keypair.public[0..2]) << 1,
                    other_ip: beam_to.0,
                    other_port: beam_to.1,
                    other_transport_identity: beam_to.3.clone(),
                    connection_state: ConnectionState::SendingClientHello { last_sent_time_ns: 0, hello_packet_payload, handshake },
                },
            );
        }
    }
    
    //println!("{:#?}", connections_map);

    loop {
        if let Ok((buf_len, other_ip_addr, other_port, ecn_marked, ecn_enabled, service_class, timestamp_ns)) = udp_recv_with_congestion_and_dscp(socket, &mut packet_memory_encrypted[..]) {
            if buf_len >= 6 {
                let magic1 = load_u48(&packet_memory_encrypted[0..6]);
                if magic1 & 1 != 0 { // Client Hello

                    if magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
                        for key_i in 0..my_listen_keypairs.len() {
                            let my_kp = &my_listen_keypairs[key_i];
                            if my_kp.magic1 == CONNECT_MAGIC1_PLAIN_TEXT {
                                if buf_len >= 6 + 32 {
                                    let client_key = &packet_memory_encrypted[6..6+32];
                                    let list_of_protocols_len_bytes = buf_len - 6 - 32;
                                    assert_eq!(list_of_protocols_len_bytes, 0); // temp
                                    // TODO list of supported Application Level protocols for e.g. zcash network upgrades.
                                    
                                    let connection_key = ConnectionKey { ip: other_ip_addr, port: other_port, key_15_bits: load_u16(&client_key[0..2]) << 1 };
                                    if let Some(existing_connection) = connections_map.get_mut(&connection_key) {
                                        println!("TODO DUAL WAY HANDSHAKE");
                                    }
                                    else {
                                        store_u48(&mut packet_memory_send[0..6], 0xffff_ffff_0000 | (load_u16(&my_kp.public[0..2]) << 1) as u64);
                                        packet_memory_send[6..6+32].copy_from_slice(&my_kp.public[..]);
                                        // TODO single chosen Application Level protocols for e.g. zcash network upgrades.
                                        let hello_packet_payload = Vec::from(&packet_memory_send[0..6+32]);
                                        
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
                                                other_transport_identity: Vec::from(client_key),
                                                connection_state: ConnectionState::SendingServerHelloPlaintext { last_sent_time_ns: 0, hello_packet_payload },
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                    else if let Some(noise_string) = noise_string_from_connect_magic1(magic1) {
                        println!("incoming {}", noise_string);
                        for key_i in 0..my_listen_keypairs.len() {
                            let my_kp = &my_listen_keypairs[key_i];
                            if my_kp.magic1 == magic1 {
                                let mut handshake = snow::Builder::new(noise_string.parse().unwrap())
                                    .prologue(&packet_memory_encrypted[0..6]).unwrap()
                                    .local_private_key(&my_kp.private).unwrap()
                                    .build_responder().unwrap();
                                if let Ok(list_of_protocols_len_bytes) = handshake.read_message(&packet_memory_encrypted[6..buf_len], &mut packet_memory_recv[..]) {
                                    if let Some(client_key) = handshake.get_remote_static() {
                                        assert_eq!(list_of_protocols_len_bytes, 0); // temp
                                        // TODO list of supported Application Level protocols for e.g. zcash network upgrades.
                                        println!("success with {:?}", client_key);

                                        // TODO add connection to tracking -- Tracking must include continued handshake state. It does not imply a finished connection. And we have to handle two people connecting to each other gracefully for the hole punch. Tracking state is a hash table where the key is (ip_addr, port, first 15 bits of the identity key)
                                    }
                                }
                            }
                        }
                    }
                }
                else { // Not client hello
                    loop { // just for early out
                        let connection_key = ConnectionKey { ip: other_ip_addr, port: other_port, key_15_bits: magic1 as u16 };
                        if let Some(existing_connection) = connections_map.get_mut(&connection_key) {
                            if let ConnectionState::SendingClientHelloPlaintext { last_sent_time_ns, hello_packet_payload } = &mut existing_connection.connection_state {
                                if magic1 >> 16 == 0xffff_ffff && buf_len >= 6 + 32 {
                                    let server_key = &packet_memory_encrypted[6..6+32];
                                    if server_key == existing_connection.other_transport_identity {
                                        println!("Connected to new server {:?}", server_key);
                                        existing_connection.connection_state = ConnectionState::Connected {
                                            handshake: None,
                                            send_sequence_number: 0,
                                            last_sent_keep_alive_time_ns: 0,
                                        };
                                    }
                                }
                                break;
                            }
                            if let ConnectionState::SendingServerHelloPlaintext { last_sent_time_ns, hello_packet_payload } = &mut existing_connection.connection_state {
                                println!("Connected to new client {:?}", existing_connection.other_transport_identity);
                                existing_connection.connection_state = ConnectionState::Connected {
                                    handshake: None,
                                    send_sequence_number: 0,
                                    last_sent_keep_alive_time_ns: 0,
                                };
                                // FALLTHROUGH TO PLAINTEXT CONNECTED
                            }
                            
                            if let ConnectionState::Connected { handshake, send_sequence_number, last_sent_keep_alive_time_ns } = &mut existing_connection.connection_state {
                                let unencrypted_payload = &packet_memory_encrypted[6..buf_len];
                                println!("Got data from {:?}  data: {:?}", existing_connection.other_transport_identity, unencrypted_payload);
                            }
                        }
                        break;
                    }
                }
            }
        }
        
        let current_time_now_ns = monotonic_clock_ns();
        connections_map.retain(|connection_key, connection_tracking_data| {
            if let ConnectionState::SendingClientHelloPlaintext { last_sent_time_ns, hello_packet_payload } = &mut connection_tracking_data.connection_state {
                if *last_sent_time_ns + 1_000_000_000 < current_time_now_ns {
                    udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21);
                    *last_sent_time_ns = current_time_now_ns;
                    return true;
                }
                if connection_tracking_data.creation_time_ns + 15_000_000_000 < current_time_now_ns {
                    return false;
                }
            }
            if let ConnectionState::SendingClientHello { last_sent_time_ns, hello_packet_payload, handshake } = &mut connection_tracking_data.connection_state {
                if *last_sent_time_ns + 1_000_000_000 < current_time_now_ns {
                    udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21);
                    *last_sent_time_ns = current_time_now_ns;
                    return true;
                }
                if connection_tracking_data.creation_time_ns + 15_000_000_000 < current_time_now_ns {
                    return false;
                }
            }
            if let ConnectionState::SendingServerHelloPlaintext { last_sent_time_ns, hello_packet_payload } = &mut connection_tracking_data.connection_state {
                if *last_sent_time_ns + 1_000_000_000 < current_time_now_ns {
                    udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &hello_packet_payload, Dscp::Af21);
                    *last_sent_time_ns = current_time_now_ns;
                    return true;
                }
                if connection_tracking_data.creation_time_ns + 15_000_000_000 < current_time_now_ns {
                    return false;
                }
            }
            if let ConnectionState::Connected { handshake, send_sequence_number, last_sent_keep_alive_time_ns } = &mut connection_tracking_data.connection_state {
                if *last_sent_keep_alive_time_ns + 5_000_000_000 < current_time_now_ns {
                    store_u16(&mut packet_memory_encrypted[0..2], connection_tracking_data.two_byte_send_prefix);
                    store_u32(&mut packet_memory_encrypted[2..6], (*send_sequence_number) as u32);
                    *send_sequence_number += 1;
                    udp_send_with_congestion_and_dscp(socket, connection_tracking_data.other_ip, connection_tracking_data.other_port, &packet_memory_encrypted[0..6], Dscp::Af21);
                    *last_sent_keep_alive_time_ns = current_time_now_ns;
                    return true;
                }
            }
            true
        });
    }
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
        socket: setup_and_bind_udp_socket(port),
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
    fn send_acks_helper(ack_state: &mut AckState, send_state: &mut SendState, ecn_down: bool) {
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

const ASSUMED_DELIVERY_INNER_PAYLOAD_SIZE: usize = 1200 - 8;
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
