use std::net::{Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};

#[test]
fn bwdth_test() {
    println!("Begin the test!");

    let _handle = std::thread::spawn(|| {
        do_the_reflector(32345);
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    do_the_test_program(29453, 32345);
}

fn do_the_test_program(port: u16, reflector_port: u16) {
    let socket = setup_and_bind_udp_socket(port);
    let mut time_of_last_status_print = std::time::Instant::now();

    let mut drop_cursor = 50_u64;
    let mut serial_number = 50_u64;
    let mut bytes_on_the_wire = 0_u64;

    let mut min_seen_rtt_buckets = [u64::MAX; 10];
    let mut rtt_bucket_cursor = 0_u64;
    let mut rtt_bucket_cursor_last_time = 0_u64;

    let mut bytes_delivered_buckets = [0_u64; 10];
    let mut bytes_delivered_bucket_cursor = 0_u64;
    let mut bytes_delivered_bucket_cursor_last_time = 0_u64;

    let mut state_machine_cursor = 0_u64;
    let mut state_machine_cursor_last_time = 0_u64;
    let mut old_measured_allowed_bytes_on_the_wire = 0_u64;

    let mut packet_buffer = vec![0_u32; PACKET_HISTORY_BUFFER_LEN];

    let mut buf = [0_u8; 16384];
    loop {
        let current_min_rtt_on_connection_ns = {
            let a = min_seen_rtt_buckets[0].min(min_seen_rtt_buckets[1]);
            let b = min_seen_rtt_buckets[2].min(min_seen_rtt_buckets[3]);
            let c = min_seen_rtt_buckets[4].min(min_seen_rtt_buckets[5]);
            let d = min_seen_rtt_buckets[6].min(min_seen_rtt_buckets[7]);
            let e = min_seen_rtt_buckets[8].min(min_seen_rtt_buckets[9]);
            a.min(b)
                .min(c)
                .min(d)
                .min(e)
                .min(10_000_000_000) // RTT assumed to be always less than 10 seconds.
                .max(5_000_000) // The maths breaks down with RTT close to zero so we pad up to 5 ms always.
        };
        let current_max_delivered_bucket_bytes = {
            let a = bytes_delivered_buckets[0].max(bytes_delivered_buckets[1]);
            let b = bytes_delivered_buckets[2].max(bytes_delivered_buckets[3]);
            let c = bytes_delivered_buckets[4].max(bytes_delivered_buckets[5]);
            let d = bytes_delivered_buckets[6].max(bytes_delivered_buckets[7]);
            let e = bytes_delivered_buckets[8].max(bytes_delivered_buckets[9]);
            a.max(b).max(c).max(d).max(e)
        };

        let bottleneck_bandwidth_Bps =
            (current_max_delivered_bucket_bytes * 1_000_000_000) / current_min_rtt_on_connection_ns;
        let measured_allowed_bytes_on_the_wire = ((bottleneck_bandwidth_Bps as u128
            * current_min_rtt_on_connection_ns as u128)
            / 1_000_000_000)
            .max(2000) as u64;

        let drop_back_edge_timestamp_ns = monotonic_clock_ns();

        if drop_back_edge_timestamp_ns
            > state_machine_cursor_last_time + current_min_rtt_on_connection_ns
        {
            state_machine_cursor += 1;
            state_machine_cursor_last_time = drop_back_edge_timestamp_ns;
        }
        if measured_allowed_bytes_on_the_wire > old_measured_allowed_bytes_on_the_wire * 102 / 100 {
            state_machine_cursor = 0;
            println!("GROW");
        }
        old_measured_allowed_bytes_on_the_wire = measured_allowed_bytes_on_the_wire;
        if state_machine_cursor >= 12 {
            state_machine_cursor = 2;
        }

        let allowed_bytes_on_the_wire = if state_machine_cursor < 2 {
            (measured_allowed_bytes_on_the_wire * 2)
                .max(measured_allowed_bytes_on_the_wire + 100_000)
        } else if state_machine_cursor < 4 {
            (measured_allowed_bytes_on_the_wire * 125 / 100)
                .max(measured_allowed_bytes_on_the_wire + 20_000)
        } else if state_machine_cursor < 6 {
            measured_allowed_bytes_on_the_wire * 75 / 100
        } else {
            measured_allowed_bytes_on_the_wire
        };

        if time_of_last_status_print.elapsed() > std::time::Duration::from_millis(1000) {
            time_of_last_status_print = std::time::Instant::now();
            println!(
                "rtt: {} us MaxBucket: {} B bottleneck bandwidth: {}",
                current_min_rtt_on_connection_ns / 1000,
                current_max_delivered_bucket_bytes,
                BytesPerSecond(bottleneck_bandwidth_Bps)
            );
            println!(
                "{} < m: {} t: {}",
                bytes_on_the_wire, measured_allowed_bytes_on_the_wire, allowed_bytes_on_the_wire
            );
        }

        while drop_cursor < serial_number {
            // The drop back edge.
            let (packet_size_bytes, send_timestamp_ns, _ecn_marked, acked) = decompress_packet_info(
                packet_buffer[drop_cursor as usize % PACKET_HISTORY_BUFFER_LEN],
            );
            let time_since_send_ns = subtract_22_bit_timestamps_with_a_known_more_recent(
                drop_back_edge_timestamp_ns,
                send_timestamp_ns,
            );
            if time_since_send_ns
                < current_min_rtt_on_connection_ns * bytes_delivered_buckets.len() as u64
            {
                break;
            }
            if acked == false {
                bytes_on_the_wire -= packet_size_bytes as u64;
            }
            drop_cursor += 1;
        }

        if serial_number + 1 >= drop_cursor + (PACKET_HISTORY_BUFFER_LEN as u64) {
            eprintln!("Error! PACKET_HISTORY_BUFFER_LEN is too small.\n");
            continue;
        } else {
            let to_send_len_compressed =
                decompress_packet_size_to_8_bits(compress_packet_size_to_8_bits(1280)) as u64;
            if to_send_len_compressed + bytes_on_the_wire <= allowed_bytes_on_the_wire {
                store_u64(&mut buf[0..8], serial_number);
                buf[8] = 1;
                let packet_size = 1280;
                let res = udp_send_with_congestion_and_dscp(
                    socket,
                    Ipv6Addr::LOCALHOST,
                    reflector_port,
                    &buf[0..packet_size],
                    Dscp::BestEffort,
                );
                if let Ok(timestamp_ns) = res {
                    packet_buffer[serial_number as usize % PACKET_HISTORY_BUFFER_LEN] =
                        compress_packet_info(packet_size as u16, timestamp_ns, false, false);
                    serial_number += 1;
                    bytes_on_the_wire += to_send_len_compressed;
                }
            }
        }

        let res = udp_recv_with_congestion_and_dscp(socket, &mut buf);
        if matches!(res, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock) {
            continue;
        }
        //println!("{}: res = {:?}", port, res);
        if res.is_err() {
            continue;
        }
        let (buf_len, other_ip_addr, other_port, ecn_marked, _service_class, timestamp_ns) =
            res.unwrap();
        if buf_len < 8 {
            continue;
        }
        let packet_serial = load_u64(&buf[0..8]);
        let packet_plaintext = &buf[8..buf_len];

        if packet_plaintext[0] == 2 {
            if packet_plaintext.len() < 1 + 8 + 3 || (packet_plaintext.len() - 1 - 8) % 3 != 0 {
                eprintln!("Error! Bad Ack. data = {}\n", hex::encode(packet_plaintext));
                continue;
            }
            let mut min_rtt_this_ack = u64::MAX;
            let mut total_bytes_acked_this_ack = 0_u64;

            let ack_base = load_u64(&packet_plaintext[1..9]);
            let mut o = 9;
            while o < packet_plaintext.len() {
                let val = load_u24(&packet_plaintext[o..o + 3]);
                o += 3;
                let ecn_marked = val & 0x80_0000 != 0;
                let ack_number = ack_base + (val & 0x7f_ffff) as u64;
                if ack_number >= serial_number {
                    eprintln!("Error! Ack number out of range. Too new. {}\n", ack_number);
                    continue;
                }
                if ack_number + (PACKET_HISTORY_BUFFER_LEN as u64) < serial_number {
                    eprintln!(
                        "Error! Ack number out of range. Too old for buffer. {}\n",
                        ack_number
                    );
                    continue;
                }
                let (packet_size_bytes, send_timestamp_ns, _ecn_marked, acked) =
                    decompress_packet_info(
                        packet_buffer[ack_number as usize % PACKET_HISTORY_BUFFER_LEN],
                    );
                if acked {
                    eprintln!("Error! Already recieved ack for {}\n", ack_number);
                    continue;
                }
                packet_buffer[ack_number as usize % PACKET_HISTORY_BUFFER_LEN] |=
                    1 | (ecn_marked as u32) << 1;
                let rtt_ns = subtract_22_bit_timestamps_with_a_known_more_recent(
                    timestamp_ns,
                    send_timestamp_ns,
                );
                min_rtt_this_ack = min_rtt_this_ack.min(rtt_ns);
                if ack_number >= drop_cursor {
                    total_bytes_acked_this_ack += packet_size_bytes as u64;
                }
            }
            if total_bytes_acked_this_ack > 0 {
                bytes_on_the_wire -= total_bytes_acked_this_ack;

                let current_time_ns = monotonic_clock_ns();

                if current_time_ns > rtt_bucket_cursor_last_time + 1_000_000_000 {
                    rtt_bucket_cursor += 1;
                    min_seen_rtt_buckets[rtt_bucket_cursor as usize % min_seen_rtt_buckets.len()] =
                        u64::MAX;
                    rtt_bucket_cursor_last_time = current_time_ns;
                }
                min_seen_rtt_buckets[rtt_bucket_cursor as usize % min_seen_rtt_buckets.len()] =
                    min_seen_rtt_buckets[rtt_bucket_cursor as usize % min_seen_rtt_buckets.len()]
                        .min(min_rtt_this_ack);

                if current_time_ns
                    > bytes_delivered_bucket_cursor_last_time + current_min_rtt_on_connection_ns
                {
                    bytes_delivered_bucket_cursor += 1;
                    bytes_delivered_buckets
                        [bytes_delivered_bucket_cursor as usize % bytes_delivered_buckets.len()] =
                        0;
                    bytes_delivered_bucket_cursor_last_time = current_time_ns;
                }
                bytes_delivered_buckets
                    [bytes_delivered_bucket_cursor as usize % bytes_delivered_buckets.len()] +=
                    total_bytes_acked_this_ack;
            }
        } else {
            println!("{}: data = {:?}", port, packet_plaintext);
        }
    }
}

fn do_the_reflector(port: u16) {
    let socket = setup_and_bind_udp_socket(port);

    let mut saved_other_ip_addr = Ipv6Addr::LOCALHOST;
    let mut saved_other_port = 0;

    let mut serial_number = 2000;

    let mut acks_in_waiting_min = 0_u64;
    let mut acks_in_waiting_buf = [(0_u64, false); ASSUMED_ACK_CAPACITY];
    let mut acks_in_waiting_count = 0;
    let mut first_waiting_ack_time_ns = 0_u64;
    let mut ack_send_buf = [0_u8; ASSUMED_DELIVERY_INNER_PAYLOAD_SIZE];

    let mut buf = [0_u8; 16384];
    loop {
        if acks_in_waiting_count > 0
            && monotonic_clock_ns() - first_waiting_ack_time_ns
                > MAX_WAIT_BEFORE_SENDING_NON_FULL_ACK
        {
            store_u64(&mut ack_send_buf[0..8], serial_number);
            ack_send_buf[8] = 2;
            serial_number += 1;
            let mut o = 9;
            store_u64(&mut ack_send_buf[o..o + 8], acks_in_waiting_min);
            o += 8;
            for i in 0..acks_in_waiting_count {
                let val = ((acks_in_waiting_buf[i].0 - acks_in_waiting_min) as u32 & 0x7f_ffff)
                    | ((acks_in_waiting_buf[i].1 as u32) << 23);
                store_u24(&mut ack_send_buf[o..o + 3], val);
                o += 3;
            }
            let res = udp_send_with_congestion_and_dscp(
                socket,
                saved_other_ip_addr,
                saved_other_port,
                &ack_send_buf[0..o],
                Dscp::Af21,
            );
            acks_in_waiting_count = 0;
        }

        let res = udp_recv_with_congestion_and_dscp(socket, &mut buf);
        if matches!(res, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock) {
            continue;
        }
        //println!("{}: res = {:?}", port, res);
        if res.is_err() {
            continue;
        }
        let (buf_len, other_ip_addr, other_port, ecn_marked, _service_class, timestamp_ns) =
            res.unwrap();
        if buf_len < 8 {
            continue;
        }
        let packet_serial = load_u64(&buf[0..8]);
        let packet_plaintext = &buf[8..buf_len - 8];
        //println!("{}: data = {:?}", port, packet_plaintext);

        saved_other_ip_addr = other_ip_addr;
        saved_other_port = other_port;

        if acks_in_waiting_count == 0 {
            acks_in_waiting_min = packet_serial;
            first_waiting_ack_time_ns = timestamp_ns;
        } else {
            acks_in_waiting_min = acks_in_waiting_min.min(packet_serial);
        }
        acks_in_waiting_buf[acks_in_waiting_count] = (packet_serial, ecn_marked);
        acks_in_waiting_count += 1;
        if acks_in_waiting_count == ASSUMED_ACK_CAPACITY
            || monotonic_clock_ns() - first_waiting_ack_time_ns
                > MIN_WAIT_BEFORE_SENDING_NON_FULL_ACK
        {
            store_u64(&mut ack_send_buf[0..8], serial_number);
            ack_send_buf[8] = 2;
            serial_number += 1;
            let mut o = 9;
            store_u64(&mut ack_send_buf[o..o + 8], acks_in_waiting_min);
            o += 8;
            for i in 0..acks_in_waiting_count {
                let val = ((acks_in_waiting_buf[i].0 - acks_in_waiting_min) as u32 & 0x7f_ffff)
                    | ((acks_in_waiting_buf[i].1 as u32) << 23);
                store_u24(&mut ack_send_buf[o..o + 3], val);
                o += 3;
            }
            let res = udp_send_with_congestion_and_dscp(
                socket,
                saved_other_ip_addr,
                saved_other_port,
                &ack_send_buf[0..o],
                Dscp::Af21,
            );
            acks_in_waiting_count = 0;
        }
    }
}

const ASSUMED_DELIVERY_INNER_PAYLOAD_SIZE: usize = 1232;
const ASSUMED_ACK_CAPACITY: usize = (ASSUMED_DELIVERY_INNER_PAYLOAD_SIZE - 8 - 1 - 8) / 3;

const MIN_WAIT_BEFORE_SENDING_NON_FULL_ACK: u64 = 5_000_000;
const MAX_WAIT_BEFORE_SENDING_NON_FULL_ACK: u64 = 20_000_000;

const PACKET_HISTORY_BUFFER_LEN: usize = 1048576;

#[inline]
fn compress_packet_info(
    packet_size_bytes: u16,
    timestamp_ns: u64,
    ecn_marked: bool,
    acked: bool,
) -> u32 {
    let size8 = compress_packet_size_to_8_bits(packet_size_bytes) as u32;
    let ts22 = ((compress_timestamp_to_22_bits(timestamp_ns) >> 13) as u32) & ((1u32 << 22) - 1);
    (size8 << 24) | (ts22 << 2) | ((ecn_marked as u32) << 1) | (acked as u32)
}
#[inline]
fn decompress_packet_info(x: u32) -> (u16, u64, bool, bool) {
    let size8 = (x >> 24) as u8;
    let ts22 = (x >> 2) & ((1u32 << 22) - 1);
    let ecn = ((x >> 1) & 1) != 0;
    let ack = (x & 1) != 0;

    let packet_size_bytes = decompress_packet_size_to_8_bits(size8);
    let timestamp_ns_quantized = (ts22 as u64) << 13;

    (packet_size_bytes, timestamp_ns_quantized, ecn, ack)
}

#[inline]
fn subtract_22_bit_timestamps_with_a_known_more_recent(mut recent: u64, mut old: u64) -> u64 {
    const ROUND_MASK: u64 = 0x1fff; // clear low 13 bits
    const KEEP_MASK: u64 = 0x0000_0007_ffff_ffff; // keep low 35 bits
    const MOD: u64 = 0x8_0000_0000; // 1 << 35

    recent = recent.wrapping_add(ROUND_MASK) & !ROUND_MASK;
    recent &= KEEP_MASK;

    old = old.wrapping_add(ROUND_MASK) & !ROUND_MASK;
    old &= KEEP_MASK;

    recent = recent.wrapping_add(((recent < old) as u64) * MOD);
    recent.wrapping_sub(old)
}
#[inline]
fn compress_timestamp_to_22_bits(mut n: u64) -> u64 {
    const ROUND_MASK: u64 = 0x1fff;
    const KEEP_MASK: u64 = 0x0000_0007_ffff_ffff;

    n = n.wrapping_add(ROUND_MASK) & !ROUND_MASK;
    n & KEEP_MASK
}

#[inline]
fn compress_packet_size_to_8_bits(n: u16) -> u8 {
    const BASE: u16 = 200;
    const K: [u16; 8] = [16, 48, 128, 384, 768, 1408, 3136, 6656];

    // remainder we need to represent as a subset-sum of K
    let mut rem = n.saturating_sub(BASE);

    // Greedy works because K is superincreasing.
    let mut out: u8 = 0;

    // i = 7 ..= 0
    let t7 = (rem >= K[7]) as u16;
    rem = rem.wrapping_sub(t7 * K[7]);
    out |= (t7 as u8) << 7;
    let t6 = (rem >= K[6]) as u16;
    rem = rem.wrapping_sub(t6 * K[6]);
    out |= (t6 as u8) << 6;
    let t5 = (rem >= K[5]) as u16;
    rem = rem.wrapping_sub(t5 * K[5]);
    out |= (t5 as u8) << 5;
    let t4 = (rem >= K[4]) as u16;
    rem = rem.wrapping_sub(t4 * K[4]);
    out |= (t4 as u8) << 4;
    let t3 = (rem >= K[3]) as u16;
    rem = rem.wrapping_sub(t3 * K[3]);
    out |= (t3 as u8) << 3;
    let t2 = (rem >= K[2]) as u16;
    rem = rem.wrapping_sub(t2 * K[2]);
    out |= (t2 as u8) << 2;
    let t1 = (rem >= K[1]) as u16;
    rem = rem.wrapping_sub(t1 * K[1]);
    out |= (t1 as u8) << 1;
    let t0 = (rem >= K[0]) as u16;
    rem = rem.wrapping_sub(t0 * K[0]);
    out |= (t0 as u8) << 0;

    out
}

#[inline]
fn decompress_packet_size_to_8_bits(n: u8) -> u16 {
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

#[inline]
fn store_u64(buf: &mut [u8], value: u64) {
    assert!(buf.len() == 8);
    buf[..8].copy_from_slice(&value.to_le_bytes());
}
#[inline]
fn load_u64(buf: &[u8]) -> u64 {
    assert!(buf.len() == 8);
    u64::from_le_bytes(buf[..8].try_into().unwrap())
}
#[inline]
fn store_u24(buf: &mut [u8], value: u32) {
    assert!(buf.len() == 3);
    buf.copy_from_slice(&value.to_le_bytes()[..3]);
}
#[inline]
fn load_u24(buf: &[u8]) -> u32 {
    assert!(buf.len() == 3);

    let mut tmp = [0u8; 4];
    tmp[..3].copy_from_slice(buf);
    u32::from_le_bytes(tmp)
}

#[cfg(unix)]
pub use linux::*;
#[cfg(unix)]
#[allow(unsafe_code)]
mod linux {
    use super::*;

    #[inline]
    pub fn monotonic_clock_ns() -> u64 {
        unsafe {
            let mut ts: libc::timespec = std::mem::zeroed();
            if libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) != 0 {
                panic!(
                    "clock_gettime() failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            (ts.tv_sec as u64) * 1_000_000_000u64 + (ts.tv_nsec as u64)
        }
    }

    pub fn setup_and_bind_udp_socket(port: u16) -> SockHandle {
        let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, libc::IPPROTO_UDP) };

        if fd < 0 {
            panic!("socket() failed: {}", std::io::Error::last_os_error());
        }

        // Make socket non-blocking
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 {
                panic!("fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                panic!("fcntl(F_SETFL) failed: {}", std::io::Error::last_os_error());
            }
        }

        unsafe {
            let zero: libc::c_int = 0;

            // Dual-stack: allow IPv4-mapped IPv6 addresses.
            if libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                &zero as *const _ as *const libc::c_void,
                std::mem::size_of_val(&zero) as libc::socklen_t,
            ) != 0
            {
                panic!(
                    "Failed to disable IPV6_V6ONLY: {}",
                    std::io::Error::last_os_error()
                );
            }
        }

        // Bind [::]:port
        unsafe {
            let mut addr: libc::sockaddr_in6 = std::mem::zeroed();
            addr.sin6_family = libc::AF_INET6 as _;
            addr.sin6_port = port.to_be();
            addr.sin6_addr = libc::in6_addr { s6_addr: [0; 16] };

            if libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            ) != 0
            {
                panic!("bind() failed: {}", std::io::Error::last_os_error());
            }
        }

        unsafe {
            let one: libc::c_int = 1;

            // IPv4 TOS (includes ECN bits) as CMSG on recvmsg
            if libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_RECVTOS,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of_val(&one) as libc::socklen_t,
            ) != 0
            {
                panic!(
                    "Failed to Enable IPv4 TOS, error: {}",
                    std::io::Error::last_os_error()
                );
            }

            // IPv6 Traffic Class (includes ECN bits) as CMSG on recvmsg
            if libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_RECVTCLASS,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of_val(&one) as libc::socklen_t,
            ) != 0
            {
                panic!(
                    "Failed to Enable IPv6 TOS, error: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        SockHandle::from_native(fd)
    }

    /// SEND one UDP packet to (dst_ip6, dst_port) on a dual-stack socket.
    /// - If `dst_ip6` is IPv4-mapped (::ffff:a.b.c.d), it sends to IPv4 using sockaddr_in
    ///   and uses IP_TOS cmsg.
    /// - Otherwise sends to IPv6 using sockaddr_in6 and IPV6_TCLASS cmsg.
    /// Return value is a nanosecond timestamp of the send.
    pub fn udp_send_with_congestion_and_dscp(
        udp_socket: SockHandle,
        dst_ip6: Ipv6Addr,
        dst_port: u16,
        payload: &[u8],
        dscp: Dscp,
    ) -> std::io::Result<u64> {
        let fd = udp_socket.to_native();

        let mut iov = libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        };

        // Decide whether this is IPv4-mapped
        let (mut name_buf, name_len, is_v4) = if let Some(v4) = dst_ip6.to_ipv4_mapped() {
            let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sin.sin_family = libc::AF_INET as _;
            sin.sin_port = dst_port.to_be();
            sin.sin_addr = libc::in_addr {
                s_addr: u32::from_ne_bytes(v4.octets()).to_be(),
            };

            let mut buf = vec![0u8; std::mem::size_of::<libc::sockaddr_in>()];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sin as *const _ as *const u8,
                    buf.as_mut_ptr(),
                    buf.len(),
                );
            }
            let buf_len = buf.len() as libc::socklen_t;
            (buf, buf_len, true)
        } else {
            let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sin6.sin6_family = libc::AF_INET6 as _;
            sin6.sin6_port = dst_port.to_be();
            sin6.sin6_addr = libc::in6_addr {
                s6_addr: dst_ip6.octets(),
            };

            let mut buf = vec![0u8; std::mem::size_of::<libc::sockaddr_in6>()];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sin6 as *const _ as *const u8,
                    buf.as_mut_ptr(),
                    buf.len(),
                );
            }
            let buf_len = buf.len() as libc::socklen_t;
            (buf, buf_len, false)
        };

        // Control buffer
        let mut cbuf = [0u8; 128];

        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = name_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_namelen = name_len;
        msg.msg_iov = &mut iov as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cbuf.len() as u32;

        let tclass_byte = ((dscp as u8) << 2) | 0b10; // ecn

        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg as *const _ as *mut _);
            if cmsg.is_null() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "CMSG_FIRSTHDR returned null",
                ));
            }

            // Linux uses int for send cmsg values.
            let val: libc::c_int = tclass_byte as libc::c_int;

            (*cmsg).cmsg_level = if is_v4 {
                libc::IPPROTO_IP
            } else {
                libc::IPPROTO_IPV6
            };
            (*cmsg).cmsg_type = if is_v4 {
                libc::IP_TOS
            } else {
                libc::IPV6_TCLASS
            };
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;

            let data = libc::CMSG_DATA(cmsg) as *mut libc::c_int;
            *data = val;

            msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as _;

            let timestamp_ns = monotonic_clock_ns();
            let n = libc::sendmsg(fd, &msg as *const _ as *mut _, 0);
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let sent = n as usize;
            if sent != payload.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    format!("partial UDP send: {sent} of {}", payload.len()),
                ));
            }
            Ok(timestamp_ns)
        }
    }

    /// RECV one UDP packet, returning:
    /// (len, src_ip6, src_port, congested, dscp)
    ///
    /// - If the peer is IPv4, it is returned as an IPv4-mapped IPv6 address (::ffff:a.b.c.d).
    /// - `congested=true` iff ECN == CE (0b11).
    /// - If no TOS/TCLASS cmsg was provided by the kernel, returns congested=false and dscp=BestEffort.
    pub fn udp_recv_with_congestion_and_dscp(
        udp_socket: SockHandle,
        buf: &mut [u8],
    ) -> std::io::Result<(usize, Ipv6Addr, u16, bool, Dscp, u64)> {
        let fd = udp_socket.to_native();

        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };

        let mut addr_storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut cbuf = [0u8; 128];

        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = (&mut addr_storage as *mut _) as *mut libc::c_void;
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as _;
        msg.msg_iov = &mut iov as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cbuf.len() as u32;

        let n = unsafe { libc::recvmsg(fd, &mut msg as *mut libc::msghdr, 0) };
        let timestamp_ns = monotonic_clock_ns();
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }

        if (msg.msg_flags & libc::MSG_TRUNC) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "UDP datagram truncated (buffer too small)",
            ));
        }

        // Peer address -> always return IPv6 (IPv4 becomes v4-mapped IPv6)
        let (src_ip6, src_port) = if (addr_storage.ss_family as i32) == libc::AF_INET {
            let sin: &libc::sockaddr_in =
                unsafe { &*(&addr_storage as *const _ as *const libc::sockaddr_in) };
            let ip4 = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            (ip4.to_ipv6_mapped(), port)
        } else if (addr_storage.ss_family as i32) == libc::AF_INET6 {
            let sin6: &libc::sockaddr_in6 =
                unsafe { &*(&addr_storage as *const _ as *const libc::sockaddr_in6) };
            let ip6 = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            (ip6, port)
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown sockaddr family",
            ));
        };

        // Defaults if no cmsg
        let mut congested = false;
        let mut dscp = Dscp::BestEffort;

        unsafe {
            let mut cmsg_ptr = libc::CMSG_FIRSTHDR(&msg as *const _);
            while !cmsg_ptr.is_null() {
                let cmsg = &*cmsg_ptr;

                let mut tclass_opt: Option<u8> = None;

                if cmsg.cmsg_level == libc::IPPROTO_IP && cmsg.cmsg_type == libc::IP_TOS {
                    // With IP_RECVTOS, Linux provides 1 byte.
                    let data = libc::CMSG_DATA(cmsg_ptr) as *const u8;
                    tclass_opt = Some(*data);
                } else if cmsg.cmsg_level == libc::IPPROTO_IPV6
                    && cmsg.cmsg_type == libc::IPV6_TCLASS
                {
                    // With IPV6_RECVTCLASS, Linux usually provides an int.
                    let data = libc::CMSG_DATA(cmsg_ptr) as *const libc::c_int;
                    tclass_opt = Some((*data as u8));
                }

                if let Some(tclass) = tclass_opt {
                    let ecn_bits = tclass & 0b11;
                    congested = ecn_bits == 0b11; // CE
                    dscp = Dscp::from_u8(tclass >> 2);
                    break;
                }

                cmsg_ptr = libc::CMSG_NXTHDR(&msg as *const _, cmsg_ptr);
            }
        }

        Ok((n as usize, src_ip6, src_port, congested, dscp, timestamp_ns))
    }
}

#[cfg(windows)]
pub use windows::*;
#[cfg(windows)]
mod windows {
    use super::*;

    #[inline]
    pub fn monotonic_clock_ns() -> u64 {
        panic!("Not implemented");
    }
    pub fn setup_and_bind_udp_socket(port: u16) -> SockHandle {
        panic!("Not implemented");
    }
    pub fn udp_send_with_congestion_and_dscp(
        udp_socket: SockHandle,
        dst_ip6: Ipv6Addr,
        dst_port: u16,
        payload: &[u8],
        dscp: Dscp,
    ) -> std::io::Result<()> {
        panic!("Not implemented");
    }
    pub fn udp_recv_with_congestion_and_dscp(
        udp_socket: SockHandle,
        buf: &mut [u8],
    ) -> std::io::Result<(usize, Ipv6Addr, u16, bool, Dscp)> {
        panic!("Not implemented");
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SockHandle(u64);

impl SockHandle {
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.0
    }

    #[inline]
    pub fn from_u64(v: u64) -> Self {
        Self(v)
    }
}

#[cfg(unix)]
impl SockHandle {
    #[inline]
    pub fn from_native(fd: libc::c_int) -> Self {
        // Preserve negative values like -1 by sign-extending through i64.
        SockHandle(fd as i64 as u64)
    }

    #[inline]
    pub fn to_native(self) -> libc::c_int {
        self.0 as i64 as libc::c_int
    }
}

#[cfg(windows)]
impl SockHandle {
    #[inline]
    pub fn from_native(sock: usize) -> Self {
        SockHandle(sock as u64)
    }

    #[inline]
    pub fn to_native(self) -> usize {
        self.0 as usize
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Dscp {
    BestEffort = 0, // I do not care about this packet. Dropped first.
    Af11 = 10,      // Important but low priority packet. Make more effort to deliver this.
    Af21 = 18, // Important and high priority packet. High delivery effort and lower latency. QUIC control frames use this.
    Ef = 46, // Expedited Forwarding. VoIP. Low Latency or useless. Very heavily policed, must be low bandwidth.
}
impl Dscp {
    pub fn from_u8(v: u8) -> Dscp {
        match v & 0b11_1111 {
            0 => Dscp::BestEffort,
            10 => Dscp::Af11,
            18 => Dscp::Af21,
            46 => Dscp::Ef,
            _ => Dscp::BestEffort,
        }
    }
}

/// Bytes per second, formatted in binary units (KiB/MiB/GiB/TiB).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BytesPerSecond(pub u64);

impl BytesPerSecond {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    const TIB: u64 = 1024 * 1024 * 1024 * 1024;

    fn best_unit(bps: u64) -> (u64, &'static str) {
        if bps >= Self::TIB {
            (Self::TIB, "TiB/s")
        } else if bps >= Self::GIB {
            (Self::GIB, "GiB/s")
        } else if bps >= Self::MIB {
            (Self::MIB, "MiB/s")
        } else if bps >= Self::KIB {
            (Self::KIB, "KiB/s")
        } else {
            (1, "B/s")
        }
    }

    fn format_value(value: u64, unit: u64) -> (u64, u64) {
        // integer + 2-decimal fixed point, rounded half-up:
        // scaled = round(value * 100 / unit)
        let scaled = (value.saturating_mul(100) + unit / 2) / unit;
        (scaled / 100, scaled % 100)
    }
}

impl std::fmt::Display for BytesPerSecond {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bps = self.0;
        let (unit, suffix) = Self::best_unit(bps);

        if unit == 1 {
            return write!(f, "{} {}", bps, suffix);
        }

        let (whole, frac) = Self::format_value(bps, unit);

        // If it's an exact integer in that unit, print without decimals.
        if frac == 0 {
            write!(f, "{} {}", whole, suffix)
        } else if whole >= 10 {
            // For >= 10, 1 decimal place is usually plenty.
            let one_decimal = (bps.saturating_mul(10) + unit / 2) / unit; // rounded
            write!(f, "{}.{:01} {}", one_decimal / 10, one_decimal % 10, suffix)
        } else {
            // For < 10, print 2 decimals for a bit more resolution.
            write!(f, "{}.{:02} {}", whole, frac, suffix)
        }
    }
}

impl std::fmt::Debug for BytesPerSecond {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Debug prints both the pretty display and the raw value.
        f.debug_tuple("BytesPerSecond")
            .field(&format_args!("{}", self))
            .field(&self.0)
            .finish()
    }
}
