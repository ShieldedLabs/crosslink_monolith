
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::os::fd::AsRawFd;

#[test]
fn bwdth_test() {
    println!("Begin the test!");
    
    do_the_test_program(29453);
}

fn do_the_test_program(port: u16) {
    let socket = setup_and_bind_udp_socket(port);
    // try a non self send in order to make sure non blocking works.
    let res = udp_send_with_congestion_and_dscp(socket, Ipv6Addr::LOCALHOST, port, b"Hello there!", Dscp::BestEffort);
    println!("res = {:?}", res);
    let mut buf = [0_u8; 1024];
    let res = udp_recv_with_congestion_and_dscp(socket, &mut buf);
    println!("res = {:?}", res);
    println!("data = {:?}", &buf[0..res.unwrap().0]);
}

#[cfg(unix)]
pub use linux::*;
#[cfg(unix)]
mod linux {
    use super::*;

    pub fn setup_and_bind_udp_socket(port: u16) -> SockHandle {
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
        }
        SockHandle::from_native(fd)
    }
    
    /// SEND one UDP packet to (dst_ip6, dst_port) on a dual-stack socket.
    /// - If `dst_ip6` is IPv4-mapped (::ffff:a.b.c.d), it sends to IPv4 using sockaddr_in
    ///   and uses IP_TOS cmsg.
    /// - Otherwise sends to IPv6 using sockaddr_in6 and IPV6_TCLASS cmsg.
    pub fn udp_send_with_congestion_and_dscp(
        udp_socket: SockHandle,
        dst_ip6: Ipv6Addr,
        dst_port: u16,
        payload: &[u8],
        dscp: Dscp,
    ) -> std::io::Result<usize> {
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
        let mut cbuf = [0u8; 128];
    
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
    
            msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as _;
    
            let n = libc::sendmsg(fd, &msg as *const _ as *mut _, 0);
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(n as usize)
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
    ) -> std::io::Result<(usize, Ipv6Addr, u16, bool, Dscp)> {
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
        msg.msg_controllen = cbuf.len();
    
        let n = unsafe { libc::recvmsg(fd, &mut msg as *mut libc::msghdr, 0) };
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
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unknown sockaddr family"));
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
                } else if cmsg.cmsg_level == libc::IPPROTO_IPV6 && cmsg.cmsg_type == libc::IPV6_TCLASS {
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
    
        Ok((n as usize, src_ip6, src_port, congested, dscp))
    }
}

#[cfg(windows)]
pub use windows::*;
#[cfg(windows)]
mod windows {
    pub fn setup_and_bind_udp_socket(port: u16) -> SockHandle {
        panic!("Not implemented");
    }
    pub fn udp_send_with_congestion_and_dscp(
        udp_socket: SockHandle,
        dst_ip6: Ipv6Addr,
        dst_port: u16,
        payload: &[u8],
        dscp: Dscp,
    ) -> std::io::Result<usize> {
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
    pub fn as_u64(self) -> u64 { self.0 }

    #[inline]
    pub fn from_u64(v: u64) -> Self { Self(v) }
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
    Af11       = 10, // Important but low priority packet. Make more effort to deliver this.
    Af21       = 18, // Important and high priority packet. High delivery effort and lower latency. QUIC control frames use this.
    Ef         = 46, // Expedited Forwarding. VoIP. Low Latency or useless. Very heavily policed, must be low bandwidth.
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