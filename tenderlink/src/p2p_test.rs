
use std::net::{Ipv6Addr};

const PACKET_TYPE_HELLO:     u8 = 0;
const PACKET_TYPE_HELLO_ACK: u8 = 1;
const PACKET_TYPE_PEER_LIST: u8 = 2;

pub struct IpAddress(pub Ipv6Addr, pub u16);

pub fn p2p(port: u16, peers: &mut Vec<IpAddress>) {
    socket_setup();
    monotonic_clock_setup();

    let socket = setup_and_bind_udp_socket(port);
}

use crate::native_sockets::*;