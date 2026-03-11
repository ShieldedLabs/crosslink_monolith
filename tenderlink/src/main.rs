fn main() {
    let args: Vec<String> = std::env::args().collect();

    //*
    if args.len() > 2 {
        if args[1] == "reflector" {
            let port : u16 = args[2].parse().unwrap();
            println!("running reflector on port: {}", port);
            tenderlink::bandwidth_test::do_the_test_program(port, None);
            return;
        }
        if args[1] == "beamer" {
            let port : u16 = args[2].parse().unwrap();
            let other_addr : std::net::Ipv6Addr = args[3].parse().unwrap();
            let other_port : u16 = args[4].parse().unwrap();
            println!("running beamer on port: {}", port);
            println!("connecting to {} port {}", other_addr, other_port);
            tenderlink::bandwidth_test::do_the_test_program(port, Some((other_addr, other_port)));
            return;
        }
    }
    // */

    const P2P_PORT: u16 = 18234;

    if args.len() > 1 {
        if args[1] == "p2p" {
            let port : u16 = args.get(2).map(|a| a.parse().unwrap()).unwrap_or(P2P_PORT);
            tenderlink::p2p_test::p2p(port, vec![]);
            return;
        }
    }

    // let peers = vec![tenderlink::p2p_test::IpAddress("0000:0000:0000:0000:0000:ffff:4622:f29b".parse().unwrap(), P2P_PORT)]; // @Temporary
    let peers = vec![tenderlink::bandwidth_test::STPAddress { ip: "::1".parse().unwrap(), port: P2P_PORT, magic1: tenderlink::bandwidth_test::CONNECT_MAGIC1_PLAIN_TEXT, key: Vec::from(&[0x00u8; 32]) }];
    if args.len() == 1 {
        tenderlink::p2p_test::p2p(0, peers);
        return;
    }
    let mut i: usize = usize::MAX;
    if args.len() > 1 {
        i = args[1].parse().unwrap_or(usize::MAX);
    }
    println!("Command line: {:?}", args);
    tenderlink::run_instances(i);
}

