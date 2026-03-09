pub use std::net::Ipv6Addr;

pub const ASSUMED_BIGGEST_POSSIBLE_UDP_FRAME_ON_EXISTING_HARDWARE: usize = 15972;
pub const ASSUMED_SMALLEST_POSSIBLE_UDP_FRAME_WITH_GUARANTEED_DELIVERY: usize = 1200;

pub type PacketMemory = Box<[u8; ASSUMED_BIGGEST_POSSIBLE_UDP_FRAME_ON_EXISTING_HARDWARE]>;

// Note(Sam): We will be reusing this memory across packets so we already do not have memory safety with regards to contents.
#[allow(unsafe_code)]
pub fn new_packet_memory() -> PacketMemory { unsafe { Box::<[u8; ASSUMED_BIGGEST_POSSIBLE_UDP_FRAME_ON_EXISTING_HARDWARE]>::new_uninit().assume_init() } }

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Dscp {
    BestEffort = 0, // I do not care about this packet. Dropped first.
    Af11       = 10, // Important but low priority packet. Make more effort to deliver this.
    Af21       = 18, // Important and high priority packet. High delivery effort and lower latency. QUIC control frames use this.
    Ef         = 46, // Expedited Forwarding. VoIP. Low Latency or useless. Very heavily policed, must be low bandwidth.
}
impl Dscp {
    #[inline]
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

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use super::*;

    #[inline]
    pub fn socket_setup() {} // Linux
    #[inline]
    pub fn monotonic_clock_setup() {} // Linux

    #[inline]
    pub fn monotonic_clock_ns() -> u64 { // Linux
        unsafe {
            let mut ts: libc::timespec = std::mem::zeroed();
            if libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) != 0 {
                panic!("clock_gettime() failed: {}", std::io::Error::last_os_error());
            }
            (ts.tv_sec as u64) * 1_000_000_000u64 + (ts.tv_nsec as u64)
        }
    }
    
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct SockHandle(libc::c_int, Option<Ipv6Addr>); // Linux
    
    /*
        This procedure is needed because on some vps' the ipv6 setup is wrong so that
        we end up using only the prefix address. Here is an example bad setup.
2: enp1s0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 state UP qlen 1000
    inet6 2001:19f0:5c00:48f2::/64 scope global noprefixroute 
       valid_lft forever preferred_lft forever
    inet6 2001:19f0:5c00:48f2:5400:5ff:fec9:bad/64 scope global noprefixroute 
       valid_lft forever preferred_lft forever
    inet6 fe80::5400:5ff:fec9:bad/64 scope link noprefixroute 
       valid_lft forever preferred_lft forever
       
       Versus the good a setup.
2: enp5s0f3u1u2u1: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 state UP qlen 1000
    inet6 2001:2043:9c85:9800:1f2e:cb54:e493:5106/64 scope global dynamic noprefixroute 
       valid_lft 304sec preferred_lft 303sec
    inet6 fd85:130a:53bf:0:39ee:50cb:ce14:1b86/64 scope global noprefixroute 
       valid_lft forever preferred_lft forever
    inet6 fe80::8e37:1f3e:4c28:db95/64 scope link noprefixroute 
       valid_lft forever preferred_lft forever
       
       I think it is as simple as the 128 bit full address not being at the top of
       the list. So instead of requiring Linux config to be run like most bad software
       this function puts in the effort to work around this issue.
    */
    #[inline]
    fn first_usable_ipv6() -> Option<Ipv6Addr> {
        unsafe {
            let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
            if libc::getifaddrs(&mut ifap) != 0 {
                // If you prefer, return None instead of panicking.
                panic!("getifaddrs failed: {}", std::io::Error::last_os_error());
            }
    
            let mut index = 0;
            let mut first_is_loopback = false;
            
            let mut cur = ifap;
            while !cur.is_null() {
                let ifa = &*cur;
    
                if !ifa.ifa_addr.is_null()
                    && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET6
                {
                    index += 1;
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    let addr = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
    
                    // Skip loopback (::1)
                    if addr.is_loopback() {
                        cur = (*cur).ifa_next;
                        if index == 1 { first_is_loopback = true; }
                        continue;
                    }
    
                    // Skip multicast (ff00::/8)
                    if addr.is_multicast() {
                        cur = (*cur).ifa_next;
                        continue;
                    }
    
                    // Skip link-local (fe80::/10)
                    if (addr.segments()[0] & 0xffc0) == 0xfe80 {
                        cur = (*cur).ifa_next;
                        continue;
                    }
    
                    // Skip subnet-router anycast (IID all zeros): xxxx:xxxx:xxxx:xxxx::
                    let seg = addr.segments();
                    if seg[4] == 0 && seg[5] == 0 && seg[6] == 0 && seg[7] == 0 {
                        cur = (*cur).ifa_next;
                        continue;
                    }
    
                    libc::freeifaddrs(ifap);
                    
                    if index == 2 && first_is_loopback {
                        // This is a normally configured Linux box and we do not need the workaround.
                        return None;
                    }
                    eprintln!("[WARNING] This linux machine has incorrectly configured it's ipv6 addresses causing a bug when sending ipv6 packets. We will be overriding the sender ip field with '{}' in order to try and work around this issue.", addr);
                    return Some(addr);
                }
    
                cur = (*cur).ifa_next;
            }
    
            libc::freeifaddrs(ifap);
            None
        }
    }

    #[inline]
    pub fn setup_and_bind_udp_socket(port: u16) -> SockHandle { // Linux
        let fd = unsafe {
            libc::socket(
                libc::AF_INET6,
                libc::SOCK_DGRAM,
                libc::IPPROTO_UDP
            )
        };
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
                panic!("Failed to disable IPV6_V6ONLY: {}", std::io::Error::last_os_error());
            }
        }
    
        let known_good_ipv6_address = first_usable_ipv6();
        // Bind [::]:port
        unsafe {
            let mut addr: libc::sockaddr_in6 = std::mem::zeroed();
            addr.sin6_family = libc::AF_INET6 as _;
            addr.sin6_port = port.to_be();
            // Note(Sam): We cannot bind it here because then we break dual stack. Instead we
            // must manually set the sender ip on every packet which is why we embedd it in
            // the socket handle.
            // addr.sin6_addr = match known_good_ipv6_address {
            //     Some(ip6) => libc::in6_addr { s6_addr: ip6.octets() },
            //     None => libc::in6_addr { s6_addr: [0; 16] },
            // };
    
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
                panic!("Failed to Enable IPv4 TOS, error: {}", std::io::Error::last_os_error());
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
                panic!("Failed to Enable IPv6 TOS, error: {}", std::io::Error::last_os_error());
            }
    
            // IPv6 Packet Info (source addr / ifindex) as CMSG on sendmsg/recvmsg
            if libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_RECVPKTINFO,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of_val(&one) as libc::socklen_t,
            ) != 0
            {
                panic!("Failed to Enable IPv6 PKTINFO, error: {}", std::io::Error::last_os_error());
            }
        }
        SockHandle(fd, known_good_ipv6_address)
    }
    
    /// SEND one UDP packet to (dst_ip6, dst_port) on a dual-stack socket.
    /// - If `dst_ip6` is IPv4-mapped (::ffff:a.b.c.d), it sends to IPv4 using sockaddr_in
    ///   and uses IP_TOS cmsg.
    /// - Otherwise sends to IPv6 using sockaddr_in6 and IPV6_TCLASS cmsg.
    /// Return value is a nanosecond timestamp of the send.
    #[inline]
    pub fn udp_send_with_congestion_and_dscp( // Linux
        udp_socket: SockHandle,
        dst_ip6: Ipv6Addr,
        dst_port: u16,
        payload: &[u8],
        dscp: Dscp,
    ) -> std::io::Result<u64> {
        let fd = udp_socket.0;
        let known_good_ipv6_address = udp_socket.1;
    
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
                s_addr: u32::from_le_bytes(v4.octets()),
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
            sin6.sin6_addr = libc::in6_addr { s6_addr: dst_ip6.octets() };
    
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
        let mut cbuf = [0u8; 256];
    
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = name_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_namelen = name_len;
        msg.msg_iov = &mut iov as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cbuf.len();
    
        let tclass_byte = ((dscp as u8) << 2) | 0b10; // ecn
    
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg as *const _ as *mut _);
            if cmsg.is_null() {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "CMSG_FIRSTHDR returned null"));
            }
    
            // Linux uses int for send cmsg values.
            let val: libc::c_int = tclass_byte as libc::c_int;
    
            (*cmsg).cmsg_level = if is_v4 { libc::IPPROTO_IP } else { libc::IPPROTO_IPV6 };
            (*cmsg).cmsg_type = if is_v4 { libc::IP_TOS } else { libc::IPV6_TCLASS };
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;
    
            let data = libc::CMSG_DATA(cmsg) as *mut libc::c_int;
            *data = val;
    
            let mut used = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as usize;
    
            if !is_v4 {
                if let Some(ip6) = known_good_ipv6_address {
                    let cmsg2 = libc::CMSG_NXTHDR(&msg as *const _ as *mut _, cmsg);
                    if cmsg2.is_null() {
                        return Err(std::io::Error::new(std::io::ErrorKind::Other, "CMSG_NXTHDR returned null"));
                    }
    
                    (*cmsg2).cmsg_level = libc::IPPROTO_IPV6;
                    (*cmsg2).cmsg_type = libc::IPV6_PKTINFO;
                    (*cmsg2).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::in6_pktinfo>() as _) as _;
    
                    let pkt6 = libc::CMSG_DATA(cmsg2) as *mut libc::in6_pktinfo;
                    std::ptr::write_bytes(pkt6 as *mut u8, 0, std::mem::size_of::<libc::in6_pktinfo>());
    
                    (*pkt6).ipi6_ifindex = 0;
                    (*pkt6).ipi6_addr = libc::in6_addr { s6_addr: ip6.octets() };
    
                    used += libc::CMSG_SPACE(std::mem::size_of::<libc::in6_pktinfo>() as _) as usize;
                }
            }
    
            msg.msg_controllen = used as _;
    
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
    #[inline]
    pub fn udp_recv_with_congestion_and_dscp( // Linux
        udp_socket: SockHandle,
        buf: &mut [u8],
    ) -> std::io::Result<(usize, Ipv6Addr, u16, bool, bool, Dscp, u64)> {
        let fd = udp_socket.0;
    
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
        msg.msg_controllen = cbuf.len();
    
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
            let ip4 = std::net::Ipv4Addr::from(sin.sin_addr.s_addr);
            let port = u16::from_be(sin.sin_port);
            (ip4.to_ipv6_mapped(), port)
        } else if (addr_storage.ss_family as i32) == libc::AF_INET6 {
            let sin6: &libc::sockaddr_in6 =
                unsafe { &*(&addr_storage as *const _ as *const libc::sockaddr_in6) };
            let ip6 = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            (ip6, port)
        } else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unknown sockaddr family"));
        };
    
        // Defaults if no cmsg
        let mut congested = false;
        let mut ecn_enabled = false;
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
                } else if cmsg.cmsg_level == libc::IPPROTO_IPV6 && cmsg.cmsg_type == libc::IPV6_TCLASS {
                    // With IPV6_RECVTCLASS, Linux usually provides an int.
                    let data = libc::CMSG_DATA(cmsg_ptr) as *const libc::c_int;
                    tclass_opt = Some((*data as u8));
                }
    
                if let Some(tclass) = tclass_opt {
                    let ecn_bits = tclass & 0b11;
                    congested = ecn_bits == 0b11; // CE
                    ecn_enabled = ecn_bits != 0;
                    dscp = Dscp::from_u8(tclass >> 2);
                    break;
                }
    
                cmsg_ptr = libc::CMSG_NXTHDR(&msg as *const _, cmsg_ptr);
            }
        }
    
        Ok((n as usize, src_ip6, src_port, congested, ecn_enabled, dscp, timestamp_ns))
    }
    
    #[inline]
    pub fn udp_probe_source_addresses( // Linux
        udp_socket: SockHandle,
    ) -> (Option<Ipv6Addr>, Option<Ipv6Addr>) {
        unsafe {
            let ipv4 = {
                let ipv4_probe_fd = unsafe {
                    libc::socket(
                        libc::AF_INET,
                        libc::SOCK_DGRAM,
                        libc::IPPROTO_UDP
                    )
                };
                if ipv4_probe_fd < 0 {
                    panic!("socket() failed: {}", std::io::Error::last_os_error());
                }
            
                let mut dst: libc::sockaddr_in = std::mem::zeroed();
                dst.sin_family = libc::AF_INET as _;
                dst.sin_port = 53u16.to_be();
                dst.sin_addr = libc::in_addr {
                    s_addr: u32::from_be_bytes([1, 1, 1, 1]),
                };
            
                let ret = if libc::connect(
                    ipv4_probe_fd,
                    &dst as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                ) != 0
                {
                    let err = std::io::Error::last_os_error();
                    match err.raw_os_error() {
                        Some(libc::ENETUNREACH)
                        | Some(libc::EHOSTUNREACH)
                        | Some(libc::EADDRNOTAVAIL) => None,
                        _ => panic!("ipv4 probe connect() failed: {}", err),
                    }
                } else {
                    let mut local: libc::sockaddr_in = std::mem::zeroed();
                    let mut local_len =
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            
                    if libc::getsockname(
                        ipv4_probe_fd,
                        &mut local as *mut _ as *mut libc::sockaddr,
                        &mut local_len as *mut _,
                    ) != 0
                    {
                        panic!("ipv4 probe getsockname() failed: {}", std::io::Error::last_os_error());
                    }
            
                    let addr = std::net::Ipv4Addr::from(u32::from_be(local.sin_addr.s_addr)).to_ipv6_mapped();
            
                    if addr.is_unspecified() || addr.is_loopback() || addr.is_unicast_link_local() {
                        None
                    } else {
                        Some(addr)
                    }
                };
                
                if libc::close(ipv4_probe_fd) != 0 {
                    panic!("close() failed: {}", std::io::Error::last_os_error());
                }
                ret
            };
            
            let known_good_ipv6_address = udp_socket.1;
            if known_good_ipv6_address.is_some() {
                return (ipv4, known_good_ipv6_address);
            }
    
            let ipv6 = {
                let ipv6_probe_fd = unsafe {
                    libc::socket(
                        libc::AF_INET6,
                        libc::SOCK_DGRAM,
                        libc::IPPROTO_UDP
                    )
                };
                if ipv6_probe_fd < 0 {
                    panic!("socket() failed: {}", std::io::Error::last_os_error());
                }
        
                let mut dst: libc::sockaddr_in6 = std::mem::zeroed();
                dst.sin6_family = libc::AF_INET6 as _;
                dst.sin6_port = 53u16.to_be();
                dst.sin6_addr = libc::in6_addr {
                    // 2606:4700:4700::1111
                    s6_addr: [
                        0x26, 0x06, 0x47, 0x00,
                        0x47, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x11, 0x11,
                    ],
                };
    
                let ret = if libc::connect(
                    ipv6_probe_fd,
                    &dst as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                ) != 0
                {
                    let err = std::io::Error::last_os_error();
                    match err.raw_os_error() {
                        Some(libc::ENETUNREACH)
                        | Some(libc::EHOSTUNREACH)
                        | Some(libc::EADDRNOTAVAIL) => None,
                        _ => panic!("ipv6 probe connect() failed: {}", err),
                    }
                } else {
                    let mut local: libc::sockaddr_in6 = std::mem::zeroed();
                    let mut local_len =
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
    
                    if libc::getsockname(
                        ipv6_probe_fd,
                        &mut local as *mut _ as *mut libc::sockaddr,
                        &mut local_len as *mut _,
                    ) != 0
                    {
                        panic!("ipv6 probe getsockname() failed: {}", std::io::Error::last_os_error());
                    }
    
                    let addr = Ipv6Addr::from(local.sin6_addr.s6_addr);
                    let seg = addr.segments();
    
                    if addr.is_unspecified()
                        || addr.is_loopback()
                        || addr.is_multicast()
                        || addr.is_unicast_link_local()
                        || (seg[4] == 0 && seg[5] == 0 && seg[6] == 0 && seg[7] == 0)
                    {
                        None
                    } else {
                        Some(addr)
                    }
                };
                
                if libc::close(ipv6_probe_fd) != 0 {
                    panic!("close() failed: {}", std::io::Error::last_os_error());
                }
                ret
            };
    
            (ipv4, ipv6)
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows::*;
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod windows {
    use super::*;
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use std::ptr::{null_mut};

    use windows_sys::Win32::Networking::WinSock::*;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    // Windows docs list IP_ECN / IPV6_ECN cmsg_type as 50 decimal.
    const CMSG_IP_ECN: i32 = 50;
    const CMSG_IPV6_ECN: i32 = 50;

    // Raw sockaddr layouts, avoids wrestling with Windows union field names.
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct SockAddrInRaw {
        sin_family: u16,
        sin_port: u16,
        sin_addr: [u8; 4], // network byte order bytes
        sin_zero: [u8; 8],
    }

    #[repr(C)]
    #[derive(Copy, Clone)]
    struct SockAddrIn6Raw {
        sin6_family: u16,
        sin6_port: u16,
        sin6_flowinfo: u32,
        sin6_addr: [u8; 16],
        sin6_scope_id: u32,
    }

    type WsaRecvMsgFn = unsafe extern "system" fn(
        s: SOCKET,
        lpMsg: *mut WSAMSG,
        lpdwNumberOfBytesRecvd: *mut u32,
        lpOverlapped: *mut OVERLAPPED,
        lpCompletionRoutine: LPWSAOVERLAPPED_COMPLETION_ROUTINE,
    ) -> i32;

    #[inline]
    fn wsa_last_error() -> std::io::Error {
        unsafe { std::io::Error::from_raw_os_error(WSAGetLastError()) }
    }

    #[inline]
    fn setsockopt_i32(sock: SOCKET, level: i32, optname: i32, val: i32) -> i32 {
        unsafe {
            setsockopt(
                sock,
                level,
                optname,
                &val as *const _ as *const u8,
                size_of::<i32>() as i32,
            )
        }
    }

    #[inline]
    fn setsockopt_u32(sock: SOCKET, level: i32, optname: i32, val: u32) -> i32 {
        unsafe {
            setsockopt(
                sock,
                level,
                optname,
                &val as *const _ as *const u8,
                size_of::<u32>() as i32,
            )
        }
    }

    #[inline]
    fn cmsg_align(n: usize) -> usize {
        let a = size_of::<usize>() - 1;
        (n + a) & !a
    }

    #[inline]
    fn cmsg_hdr_aligned_len() -> usize {
        cmsg_align(size_of::<CMSGHDR>())
    }

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn QueryPerformanceCounter(counter:     *mut i64) -> i32;
        fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
    }

    static mut QPC_INV_FREQ_SCALE: u128 = 0;

    #[inline]
    pub fn socket_setup() { // Windows
        unsafe {
            let mut data: WSADATA = zeroed();
            // MAKEWORD(2,2)
            let ver: u16 = 0x0202;
            let rc = WSAStartup(ver, &mut data as *mut _);
            if rc != 0 {
                panic!("WSAStartup failed: {}", std::io::Error::from_raw_os_error(rc));
            }
        }
    }
    
    #[inline]
    pub fn monotonic_clock_setup() { // Windows
        unsafe {
            let mut freq: u128 = 0; QueryPerformanceFrequency(&mut freq as *mut _ as *mut i64);
            QPC_INV_FREQ_SCALE = ((1_000_000_000u128 << 64) / freq);
        }
    }

    #[inline]
    pub fn monotonic_clock_ns() -> u64 { // Windows
        unsafe {
            let mut counter: u128 = 0; QueryPerformanceCounter(&mut counter as *mut _ as *mut i64);
            ((counter * QPC_INV_FREQ_SCALE) >> 64) as u64
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct SockHandle(SOCKET, WsaRecvMsgFn); // Windows

    #[inline]
    fn get_wsarecvmsg(sock: SOCKET) -> WsaRecvMsgFn {
        unsafe {
            let mut bytes: u32 = 0;
            let mut recvmsg_opt: LPFN_WSARECVMSG = None;
            let mut guid = WSAID_WSARECVMSG;

            let rc = WSAIoctl(
                sock,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                &mut guid as *mut _ as *mut c_void,
                size_of::<windows_sys::core::GUID>() as u32,
                &mut recvmsg_opt as *mut _ as *mut c_void,
                size_of::<LPFN_WSARECVMSG>() as u32,
                &mut bytes as *mut u32,
                null_mut(),
                None,
            );
            if rc == SOCKET_ERROR {
                panic!("WSAIoctl(SIO_GET_EXTENSION_FUNCTION_POINTER/WSARecvMsg) failed: {}", wsa_last_error());
            }

            recvmsg_opt.expect("WSARecvMsg extension pointer was null")
        }
    }

    #[inline]
    pub fn setup_and_bind_udp_socket(port: u16) -> SockHandle { // Windows
        let sock = unsafe { socket(AF_INET6 as i32, SOCK_DGRAM as i32, IPPROTO_UDP as i32) };
        if sock == INVALID_SOCKET {
            panic!("socket(AF_INET6, SOCK_DGRAM, UDP) failed: {}", wsa_last_error());
        }

        // non-blocking
        unsafe {
            let mut nonblocking: u32 = 1;
            if ioctlsocket(sock, FIONBIO as i32, &mut nonblocking as *mut u32) == SOCKET_ERROR {
                panic!("ioctlsocket(FIONBIO) failed: {}", wsa_last_error());
            }
        }

        // dual-stack: IPV6_V6ONLY = 0
        if setsockopt_u32(sock, IPPROTO_IPV6 as i32, IPV6_V6ONLY as i32, 0) == SOCKET_ERROR {
            panic!("setsockopt(IPV6_V6ONLY=0) failed: {}", wsa_last_error());
        }

        // Bind [::]:port
        let addr = SockAddrIn6Raw {
            sin6_family: AF_INET6 as u16,
            sin6_port: port.to_be(),
            sin6_flowinfo: 0,
            sin6_addr: [0; 16], // in6addr_any
            sin6_scope_id: 0,
        };

        let rc = unsafe {
            bind(
                sock,
                &addr as *const _ as *const SOCKADDR,
                size_of::<SockAddrIn6Raw>() as i32,
            )
        };
        if rc == SOCKET_ERROR {
            panic!("bind([::]:{}) failed: {}", port, wsa_last_error());
        }

        // Receive ancillary metadata:
        // - IPv4 TOS on v4 packets (dual-stack)
        // - IPv6 traffic-class/ECN metadata on v6 packets
        //
        // On some systems, IPPROTO_IP setsockopt on a dual-stack socket can fail with WSAEINVAL
        // if IPv4 is disabled. We ignore only that case.
        let rc_v4_tos = setsockopt_u32(sock, IPPROTO_IP as i32, IP_RECVTOS as i32, 1);
        if rc_v4_tos == SOCKET_ERROR {
            let e = unsafe { WSAGetLastError() };
            if e != WSAEINVAL {
                panic!("setsockopt(IP_RECVTOS=1) failed: {}", std::io::Error::from_raw_os_error(e));
            }
        }

        if setsockopt_u32(sock, IPPROTO_IPV6 as i32, IPV6_RECVTCLASS as i32, 1) == SOCKET_ERROR {
            panic!("setsockopt(IPV6_RECVTCLASS=1) failed: {}", wsa_last_error());
        }

        let recvmsg = get_wsarecvmsg(sock);

        SockHandle(sock, recvmsg)
    }

    #[inline]
    pub fn udp_send_with_congestion_and_dscp( // Windows
        udp_socket: SockHandle,
        dst_ip6: Ipv6Addr,
        dst_port: u16,
        payload: &[u8],
        dscp: Dscp,
    ) -> std::io::Result<u64> {
        let sock = udp_socket.0;

        // full TCLASS/TOS byte: DSCP in upper 6 bits, ECN=ECT(0) (0b10)
        let tclass_byte: u8 = ((dscp as u8) << 2) | 0b10;
        let tclass_i32 = tclass_byte as i32;

        // Best-effort per-send tagging via socket option before send.
        // For v4-mapped destinations, use IP_TOS.
        // For v6, use IPV6_TCLASS (some providers may not support it).
        let is_v4_mapped = dst_ip6.to_ipv4_mapped().is_some();
        let set_rc = if is_v4_mapped {
            setsockopt_i32(sock, IPPROTO_IP as i32, IP_TOS as i32, tclass_i32)
        } else {
            setsockopt_i32(sock, IPPROTO_IPV6 as i32, IPV6_TCLASS as i32, tclass_i32)
        };

        if set_rc == SOCKET_ERROR {
            let e = unsafe { WSAGetLastError() };
            // Some Windows stacks/providers may reject IPV6_TCLASS. In that case,
            // continue untagged rather than failing the send.
            if !( !is_v4_mapped && e == WSAENOPROTOOPT ) {
                return Err(std::io::Error::from_raw_os_error(e));
            }
        }

        // Dual-stack sockets want AF_INET6 sockaddr, IPv4 must be IPv4-mapped IPv6.
        let dst = SockAddrIn6Raw {
            sin6_family: AF_INET6 as u16,
            sin6_port: dst_port.to_be(),
            sin6_flowinfo: 0,
            sin6_addr: dst_ip6.octets(),
            sin6_scope_id: 0,
        };

        let timestamp_ns = monotonic_clock_ns();

        let n = unsafe {
            sendto(
                sock,
                payload.as_ptr() as *const u8,
                payload.len() as i32,
                0,
                &dst as *const _ as *const SOCKADDR,
                size_of::<SockAddrIn6Raw>() as i32,
            )
        };
        if n == SOCKET_ERROR {
            return Err(wsa_last_error());
        }
        if n as usize != payload.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("partial UDP send: {} of {}", n, payload.len()),
            ));
        }

        Ok(timestamp_ns)
    }

    #[inline]
    pub fn udp_recv_with_congestion_and_dscp( // Windows
        udp_socket: SockHandle,
        buf: &mut [u8],
    ) -> std::io::Result<(usize, Ipv6Addr, u16, bool, bool, Dscp, u64)> {
        let sock = udp_socket.0;

        let mut data_wsa = WSABUF {
            len: buf.len() as u32,
            buf: buf.as_mut_ptr() as *mut u8,
        };

        let mut name_storage = [0u8; 128];
        let mut control = [0u8; 256];

        let mut msg = WSAMSG {
            name: name_storage.as_mut_ptr() as *mut SOCKADDR,
            namelen: name_storage.len() as i32,
            lpBuffers: &mut data_wsa as *mut WSABUF,
            dwBufferCount: 1,
            Control: WSABUF {
                len: control.len() as u32,
                buf: control.as_mut_ptr() as *mut u8,
            },
            dwFlags: 0,
        };

        let mut nbytes: u32 = 0;
        let rc = unsafe {
            (udp_socket.1)(
                sock,
                &mut msg as *mut WSAMSG,
                &mut nbytes as *mut u32,
                null_mut(),
                None,
            )
        };
        let timestamp_ns = monotonic_clock_ns();

        if rc == SOCKET_ERROR {
            let e = unsafe { WSAGetLastError() };
            if e == WSAEMSGSIZE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "UDP datagram truncated (buffer too small)",
                ));
            }
            return Err(std::io::Error::from_raw_os_error(e));
        }

        if (msg.dwFlags & (MSG_TRUNC as u32)) != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "UDP datagram truncated (buffer too small)",
            ));
        }

        // Parse peer address.
        let family = u16::from_ne_bytes([name_storage[0], name_storage[1]]) as i32;

        let (src_ip6, src_port) = if family == AF_INET6 as i32 {
            if (msg.namelen as usize) < size_of::<SockAddrIn6Raw>() {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "short sockaddr_in6"));
            }
            let sin6 = unsafe { &*(name_storage.as_ptr() as *const SockAddrIn6Raw) };
            (Ipv6Addr::from(sin6.sin6_addr), u16::from_be(sin6.sin6_port))
        } else if family == AF_INET as i32 {
            if (msg.namelen as usize) < size_of::<SockAddrInRaw>() {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "short sockaddr_in"));
            }
            let sin = unsafe { &*(name_storage.as_ptr() as *const SockAddrInRaw) };
            let ip4 = std::net::Ipv4Addr::new(sin.sin_addr[0], sin.sin_addr[1], sin.sin_addr[2], sin.sin_addr[3]);
            (ip4.to_ipv6_mapped(), u16::from_be(sin.sin_port))
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown sockaddr family: {}", family),
            ));
        };

        // Defaults if no ancillary info.
        let mut congested = false;
        let mut ecn_enabled = false;
        let mut dscp = Dscp::BestEffort;

        // Parse CMSGHDR chain manually.
        let ctl_len = msg.Control.len as usize;
        let hdr_aligned = cmsg_hdr_aligned_len();
        let mut off = 0usize;

        while off + size_of::<CMSGHDR>() <= ctl_len {
            let hdr = unsafe { &*(control.as_ptr().add(off) as *const CMSGHDR) };
            let cmsg_len = hdr.cmsg_len as usize;

            if cmsg_len < hdr_aligned {
                break;
            }
            if off + cmsg_len > ctl_len {
                break;
            }

            let data_off = off + hdr_aligned;
            let data_len = cmsg_len - hdr_aligned;

            let level = hdr.cmsg_level;
            let ty = hdr.cmsg_type;

            // Full TOS/TCLASS case (provider-dependent).
            if (level == IPPROTO_IP as i32 && ty == IP_TOS as i32)
                || (level == IPPROTO_IPV6 as i32 && ty == IPV6_TCLASS as i32)
            {
                // Some providers use 1 byte, some use int.
                let tclass: u8 = if data_len >= 1 {
                    control[data_off]
                } else {
                    0
                };

                let ecn_bits = tclass & 0b11;
                congested = ecn_bits == 0b11;
                ecn_enabled = ecn_bits != 0;
                dscp = Dscp::from_u8(tclass >> 2);
                break;
            }

            // Windows ECN-only cmsg case (documented for WSASetRecvIPEcn / IPV6_RECVTCLASS).
            if (level == IPPROTO_IP as i32 && ty == CMSG_IP_ECN)
                || (level == IPPROTO_IPV6 as i32 && ty == CMSG_IPV6_ECN)
            {
                if data_len >= size_of::<i32>() {
                    let ecn_val = unsafe {
                        *(control.as_ptr().add(data_off) as *const i32)
                    } as u8;

                    let ecn_bits = ecn_val & 0b11;
                    congested = ecn_bits == 0b11;
                    ecn_enabled = ecn_bits != 0;

                    // DSCP is not available from ECN-only cmsg.
                    dscp = Dscp::BestEffort;
                    break;
                }
            }

            let step = cmsg_align(cmsg_len);
            if step == 0 {
                break;
            }
            off = off.saturating_add(step);
        }

        Ok((nbytes as usize, src_ip6, src_port, congested, ecn_enabled, dscp, timestamp_ns))
    }

    #[inline]
    pub fn udp_probe_source_addresses( // Windows
        _udp_socket: SockHandle,
    ) -> (Option<Ipv6Addr>, Option<Ipv6Addr>) {
        unsafe {
            let ipv4 = {
                let probe_sock = socket(AF_INET as i32, SOCK_DGRAM as i32, IPPROTO_UDP as i32);
                if probe_sock == INVALID_SOCKET {
                    panic!("socket(AF_INET) failed: {}", wsa_last_error());
                }

                let dst = SockAddrInRaw {
                    sin_family: AF_INET as u16,
                    sin_port: 53u16.to_be(),
                    sin_addr: [1, 1, 1, 1],
                    sin_zero: [0; 8],
                };

                let ret = if connect(
                    probe_sock,
                    &dst as *const _ as *const SOCKADDR,
                    size_of::<SockAddrInRaw>() as i32,
                ) == SOCKET_ERROR
                {
                    match WSAGetLastError() {
                        WSAENETUNREACH | WSAEHOSTUNREACH | WSAEADDRNOTAVAIL => None,
                        e => panic!("ipv4 probe connect() failed: {}", std::io::Error::from_raw_os_error(e)),
                    }
                } else {
                    let mut local: SockAddrInRaw = zeroed();
                    let mut local_len = size_of::<SockAddrInRaw>() as i32;

                    if getsockname(
                        probe_sock,
                        &mut local as *mut _ as *mut SOCKADDR,
                        &mut local_len as *mut _,
                    ) == SOCKET_ERROR
                    {
                        panic!("ipv4 probe getsockname() failed: {}", wsa_last_error());
                    }

                    let addr = std::net::Ipv4Addr::from(local.sin_addr).to_ipv6_mapped();

                    if addr.is_unspecified() || addr.is_loopback() || addr.is_unicast_link_local() {
                        None
                    } else {
                        Some(addr)
                    }
                };

                if closesocket(probe_sock) == SOCKET_ERROR {
                    panic!("closesocket() failed: {}", wsa_last_error());
                }
                ret
            };

            let ipv6 = {
                let probe_sock = socket(AF_INET6 as i32, SOCK_DGRAM as i32, IPPROTO_UDP as i32);
                if probe_sock == INVALID_SOCKET {
                    panic!("socket(AF_INET6) failed: {}", wsa_last_error());
                }

                let dst = SockAddrIn6Raw {
                    sin6_family: AF_INET6 as u16,
                    sin6_port: 53u16.to_be(),
                    sin6_flowinfo: 0,
                    // 2606:4700:4700::1111
                    sin6_addr: [
                        0x26, 0x06, 0x47, 0x00,
                        0x47, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x11, 0x11,
                    ],
                    sin6_scope_id: 0,
                };

                let ret = if connect(
                    probe_sock,
                    &dst as *const _ as *const SOCKADDR,
                    size_of::<SockAddrIn6Raw>() as i32,
                ) == SOCKET_ERROR
                {
                    match WSAGetLastError() {
                        WSAENETUNREACH | WSAEHOSTUNREACH | WSAEADDRNOTAVAIL => None,
                        e => panic!("ipv6 probe connect() failed: {}", std::io::Error::from_raw_os_error(e)),
                    }
                } else {
                    let mut local: SockAddrIn6Raw = zeroed();
                    let mut local_len = size_of::<SockAddrIn6Raw>() as i32;

                    if getsockname(
                        probe_sock,
                        &mut local as *mut _ as *mut SOCKADDR,
                        &mut local_len as *mut _,
                    ) == SOCKET_ERROR
                    {
                        panic!("ipv6 probe getsockname() failed: {}", wsa_last_error());
                    }

                    let addr = Ipv6Addr::from(local.sin6_addr);
                    let seg = addr.segments();

                    if addr.is_unspecified()
                        || addr.is_loopback()
                        || addr.is_multicast()
                        || addr.is_unicast_link_local()
                        || (seg[4] == 0 && seg[5] == 0 && seg[6] == 0 && seg[7] == 0)
                    {
                        None
                    } else {
                        Some(addr)
                    }
                };

                if closesocket(probe_sock) == SOCKET_ERROR {
                    panic!("closesocket() failed: {}", wsa_last_error());
                }
                ret
            };

            (ipv4, ipv6)
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos {
    use super::*;

    #[inline]
    pub fn socket_setup() {} // Mac
    #[inline]
    pub fn monotonic_clock_setup() {} // Mac

    #[inline]
    pub fn monotonic_clock_ns() -> u64 { // Mac
        unsafe {
            let time = libc::mach_absolute_time();

            let mut info = std::mem::zeroed::<libc::mach_timebase_info>();
            libc::mach_timebase_info(&mut info);

            time * info.numer as u64 / info.denom as u64
        }
    }
    
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct SockHandle(libc::c_int); // Mac
    
    #[inline]
    pub fn setup_and_bind_udp_socket(port: u16) -> SockHandle { // Mac
        // Create an IPv6 UDP socket (we'll run it dual-stack via IPV6_V6ONLY=0).
        let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, libc::IPPROTO_UDP) };
        if fd < 0 {
            panic!("socket() failed: {}", std::io::Error::last_os_error());
        }

        // Make socket non-blocking.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 {
                panic!("fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                panic!("fcntl(F_SETFL) failed: {}", std::io::Error::last_os_error());
            }
        }

        // Dual-stack: allow IPv4-mapped IPv6 addresses (::ffff:a.b.c.d).
        unsafe {
            let zero: libc::c_int = 0;
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
            addr.sin6_addr = libc::in6addr_any;

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

            // macOS: receive IPv6 Traffic Class as a cmsg (type IPV6_TCLASS).
            // On a dual-stack IPv6 socket this is the supported way to read DSCP/ECN.
            if libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_RECVTCLASS,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of_val(&one) as libc::socklen_t,
            ) != 0
            {
                panic!(
                    "Failed to enable IPV6_RECVTCLASS: {}",
                    std::io::Error::last_os_error()
                );
            }

            // Receive IPV6_PKTINFO cmsg (dst addr + ifindex) on recvmsg (and usable for sendmsg).
            if libc::setsockopt(
                fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_RECVPKTINFO,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of_val(&one) as libc::socklen_t,
            ) != 0
            {
                panic!(
                    "Failed to enable IPV6_RECVPKTINFO: {}",
                    std::io::Error::last_os_error()
                );
            }
        }

        SockHandle(fd)
    }

    /// SEND one UDP packet to (dst_ip6, dst_port) on a dual-stack IPv6 socket.
    ///
    /// - Sets SO_NET_SERVICE_TYPE based on DSCP (inlined mapping).
    /// - Sets per-packet IPV6_TCLASS (DSCP + ECN).
    /// - Works for IPv6 and IPv4-mapped IPv6 destinations.
    /// Return value is a nanosecond timestamp of the send.
    #[inline]
    pub fn udp_send_with_congestion_and_dscp( // Mac
        udp_socket: SockHandle,
        dst_ip6: Ipv6Addr,
        dst_port: u16,
        payload: &[u8],
        dscp: Dscp,
    ) -> std::io::Result<u64> {
        // Darwin constants (not always exposed by Rust libc)
        const SO_NET_SERVICE_TYPE: libc::c_int = 0x1116;

        const NET_SERVICE_TYPE_BE: libc::c_int = 0;
        const NET_SERVICE_TYPE_BK: libc::c_int = 1;
        const NET_SERVICE_TYPE_SIG: libc::c_int = 2;
        const NET_SERVICE_TYPE_VO: libc::c_int = 4;

        let fd = udp_socket.0;

        // 1) Set SO_NET_SERVICE_TYPE (per-socket QoS intent) — inlined DSCP mapping.
        unsafe {
            let svc: libc::c_int = match dscp {
                Dscp::BestEffort => NET_SERVICE_TYPE_BK,
                Dscp::Af11       => NET_SERVICE_TYPE_BE,
                Dscp::Af21       => NET_SERVICE_TYPE_SIG,
                Dscp::Ef         => NET_SERVICE_TYPE_VO,
            };

            if libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                SO_NET_SERVICE_TYPE,
                &svc as *const _ as *const libc::c_void,
                std::mem::size_of_val(&svc) as libc::socklen_t,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
        }

        // 2) Destination: always sockaddr_in6 (works for IPv6 and IPv4-mapped IPv6).
        let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        sin6.sin6_family = libc::AF_INET6 as _;
        sin6.sin6_port = dst_port.to_be();
        sin6.sin6_addr = libc::in6_addr { s6_addr: dst_ip6.octets() };

        let mut iov = libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        };

        // 3) Control buffer for one IPV6_TCLASS cmsg carrying an int.
        let cmsg_space =
            unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) } as usize;
        let mut cbuf = vec![0u8; cmsg_space];

        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = (&mut sin6 as *mut libc::sockaddr_in6).cast::<libc::c_void>();
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
        msg.msg_iov = &mut iov as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr().cast::<libc::c_void>();
        msg.msg_controllen = cbuf.len() as u32;

        // 4) DSCP + ECN (ECT(0) = 0b10).
        let tclass_byte: u8 = ((dscp as u8) << 2) | 0b10;

        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg as *const _ as *mut _);
            if cmsg.is_null() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "CMSG_FIRSTHDR returned null",
                ));
            }

            (*cmsg).cmsg_level = libc::IPPROTO_IPV6;
            (*cmsg).cmsg_type = libc::IPV6_TCLASS;
            (*cmsg).cmsg_len =
                libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as _) as _;

            let data = libc::CMSG_DATA(cmsg).cast::<libc::c_int>();
            *data = tclass_byte as libc::c_int;

            msg.msg_controllen =
                libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as _;

            let timestamp_ns = monotonic_clock_ns();
            let n = libc::sendmsg(fd, &msg as *const _ as *mut _, 0);
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }

            if n as usize != payload.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    format!("partial UDP send: {n} of {}", payload.len()),
                ));
            }

            Ok(timestamp_ns)
        }
    }

    /// RECV one UDP packet, returning:
    /// (len, src_ip6, src_port, congested, ecn_enabled, dscp, timestamp_ns)
    ///
    /// - If the peer is IPv4, it is returned as an IPv4-mapped IPv6 address (::ffff:a.b.c.d).
    /// - congested=true iff ECN == CE (0b11).
    /// - If no TCLASS cmsg was provided by the kernel, returns congested=false, ecn_enabled=false,
    ///   and dscp=BestEffort.
    #[inline]
    pub fn udp_recv_with_congestion_and_dscp( // Mac
        udp_socket: SockHandle,
        buf: &mut [u8],
    ) -> std::io::Result<(usize, Ipv6Addr, u16, bool, bool, Dscp, u64)> {
        let fd = udp_socket.0;

        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };

        let mut addr_storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };

        // Control buffer: room for at least one IPV6_TCLASS (int) cmsg (plus some slack).
        let mut cbuf = [0u8; 128];

        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = (&mut addr_storage as *mut _) as *mut libc::c_void;
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        msg.msg_iov = &mut iov as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cbuf.len() as _; // macOS expects u32-ish, not usize

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
            // s_addr is in network byte order; from(u32) expects big-endian representation.
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
        let mut ecn_enabled = false;
        let mut dscp = Dscp::BestEffort;

        // macOS: Traffic Class arrives as IPV6_TCLASS with an `int` payload when IPV6_RECVTCLASS is enabled.
        unsafe {
            let mut cmsg_ptr = libc::CMSG_FIRSTHDR(&msg as *const _ as *mut _);
            while !cmsg_ptr.is_null() {
                let cmsg = &*cmsg_ptr;

                if cmsg.cmsg_level == libc::IPPROTO_IPV6 && cmsg.cmsg_type == libc::IPV6_TCLASS {
                    let data = libc::CMSG_DATA(cmsg_ptr) as *const libc::c_int;
                    let tclass = (*data as u32 & 0xFF) as u8;

                    let ecn_bits = tclass & 0b11;
                    congested = ecn_bits == 0b11; // CE
                    ecn_enabled = ecn_bits != 0;
                    dscp = Dscp::from_u8(tclass >> 2);
                    break;
                }

                cmsg_ptr = libc::CMSG_NXTHDR(&msg as *const _ as *mut _, cmsg_ptr);
            }
        }

        Ok((
            n as usize,
            src_ip6,
            src_port,
            congested,
            ecn_enabled,
            dscp,
            timestamp_ns,
        ))
    }

    #[inline]
    pub fn udp_probe_source_addresses( // Mac
        _udp_socket: SockHandle,
    ) -> (Option<Ipv6Addr>, Option<Ipv6Addr>) {
        unsafe {
            let ipv4 = {
                let ipv4_probe_fd = libc::socket(
                    libc::AF_INET,
                    libc::SOCK_DGRAM,
                    libc::IPPROTO_UDP,
                );
                if ipv4_probe_fd < 0 {
                    panic!("socket() failed: {}", std::io::Error::last_os_error());
                }

                let mut dst: libc::sockaddr_in = std::mem::zeroed();
                dst.sin_family = libc::AF_INET as _;
                dst.sin_port = 53u16.to_be();
                dst.sin_addr = libc::in_addr {
                    s_addr: u32::from_be_bytes([1, 1, 1, 1]),
                };

                let ret = if libc::connect(
                    ipv4_probe_fd,
                    &dst as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                ) != 0
                {
                    let err = std::io::Error::last_os_error();
                    match err.raw_os_error() {
                        Some(libc::ENETUNREACH)
                        | Some(libc::EHOSTUNREACH)
                        | Some(libc::EADDRNOTAVAIL) => None,
                        _ => panic!("ipv4 probe connect() failed: {}", err),
                    }
                } else {
                    let mut local: libc::sockaddr_in = std::mem::zeroed();
                    let mut local_len =
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;

                    if libc::getsockname(
                        ipv4_probe_fd,
                        &mut local as *mut _ as *mut libc::sockaddr,
                        &mut local_len as *mut _,
                    ) != 0
                    {
                        panic!(
                            "ipv4 probe getsockname() failed: {}",
                            std::io::Error::last_os_error()
                        );
                    }

                    let addr = std::net::Ipv4Addr::from(
                        u32::from_be(local.sin_addr.s_addr)
                    )
                    .to_ipv6_mapped();

                    if addr.is_unspecified()
                        || addr.is_loopback()
                        || addr.is_unicast_link_local()
                    {
                        None
                    } else {
                        Some(addr)
                    }
                };

                if libc::close(ipv4_probe_fd) != 0 {
                    panic!("close() failed: {}", std::io::Error::last_os_error());
                }
                ret
            };

            let ipv6 = {
                let ipv6_probe_fd = libc::socket(
                    libc::AF_INET6,
                    libc::SOCK_DGRAM,
                    libc::IPPROTO_UDP,
                );
                if ipv6_probe_fd < 0 {
                    panic!("socket() failed: {}", std::io::Error::last_os_error());
                }

                let mut dst: libc::sockaddr_in6 = std::mem::zeroed();
                dst.sin6_family = libc::AF_INET6 as _;
                dst.sin6_port = 53u16.to_be();
                dst.sin6_addr = libc::in6_addr {
                    // 2606:4700:4700::1111
                    s6_addr: [
                        0x26, 0x06, 0x47, 0x00,
                        0x47, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00,
                        0x00, 0x00, 0x11, 0x11,
                    ],
                };

                let ret = if libc::connect(
                    ipv6_probe_fd,
                    &dst as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                ) != 0
                {
                    let err = std::io::Error::last_os_error();
                    match err.raw_os_error() {
                        Some(libc::ENETUNREACH)
                        | Some(libc::EHOSTUNREACH)
                        | Some(libc::EADDRNOTAVAIL) => None,
                        _ => panic!("ipv6 probe connect() failed: {}", err),
                    }
                } else {
                    let mut local: libc::sockaddr_in6 = std::mem::zeroed();
                    let mut local_len =
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;

                    if libc::getsockname(
                        ipv6_probe_fd,
                        &mut local as *mut _ as *mut libc::sockaddr,
                        &mut local_len as *mut _,
                    ) != 0
                    {
                        panic!(
                            "ipv6 probe getsockname() failed: {}",
                            std::io::Error::last_os_error()
                        );
                    }

                    let addr = Ipv6Addr::from(local.sin6_addr.s6_addr);
                    let seg = addr.segments();

                    if addr.is_unspecified()
                        || addr.is_loopback()
                        || addr.is_multicast()
                        || addr.is_unicast_link_local()
                        || (seg[4] == 0 && seg[5] == 0 && seg[6] == 0 && seg[7] == 0)
                    {
                        None
                    } else {
                        Some(addr)
                    }
                };

                if libc::close(ipv6_probe_fd) != 0 {
                    panic!("close() failed: {}", std::io::Error::last_os_error());
                }
                ret
            };

            (ipv4, ipv6)
        }
    }
    
}

//#[test]
fn test_network_switch() {
    let socket = setup_and_bind_udp_socket(0);
    loop {
        std::thread::yield_now();
        let time_before = monotonic_clock_ns();
        let (ipv4, ipv6) = udp_probe_source_addresses(socket);
        let time_after = monotonic_clock_ns();
        println!("{:?} {:?} took {} ns", ipv4, ipv6, time_after - time_before);
        /*  RESULTS
            Sams Laptop running manjaro: 13-18 us
            Sams m4 super mac: 100 us ish
        */
    }
}
