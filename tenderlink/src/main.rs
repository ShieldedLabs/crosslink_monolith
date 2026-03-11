fn main() {
    use tenderlink::*;

    let args: Vec<String> = std::env::args().collect();

    //*
    if args.len() > 2 {
        use bandwidth_test::*;
        if args[1] == "reflector" {
            let port : u16 = args[2].parse().unwrap();
            println!("running reflector on port: {}", port);
            do_the_test_program(port, None);
            return;
        }
        if args[1] == "beamer" {
            let port : u16 = args[2].parse().unwrap();
            let other_addr : std::net::Ipv6Addr = args[3].parse().unwrap();
            let other_port : u16 = args[4].parse().unwrap();
            println!("running beamer on port: {}", port);
            println!("connecting to {} port {}", other_addr, other_port);
            do_the_test_program(port, Some((other_addr, other_port)));
            return;
        }
    }
    // */

    const P2P_PORT: u16 = 18234;

    const SEEDER_CRYPTO: u64 = bandwidth_test::CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s;

    let sk = [7, 11, 99, 220, 101, 143, 113,   4, 242, 136,  58, 150, 223, 186, 106, 203,
             67, 18, 48,  96, 176,  69, 152, 173, 224,  46, 206, 156, 217,  31, 170, 165];

    let seeder_keypair = bandwidth_test::IdentityKeyPair {
        magic1: SEEDER_CRYPTO,
        private: sk.to_vec(),
        public: x25519_dalek::x25519(sk, x25519_dalek::X25519_BASEPOINT_BYTES).to_vec(),
    };

    // let seeder_keypair = bandwidth_test::IdentityKeyPair {
    //     magic1:  bandwidth_test::CONNECT_MAGIC1_PLAIN_TEXT,
    //     ..seeder_keypair
    // };

    if args.len() > 1 {
        if args[1] == "p2p" {
            let port : u16 = args.get(2).map(|a| a.parse().unwrap()).unwrap_or(P2P_PORT);
            p2p_test::p2p(port, Some(seeder_keypair), vec![]);
            return;
        }
    }

    // let ip = "0000:0000:0000:0000:0000:ffff:4622:f29b".parse().unwrap(); // @Temporary
    let ip = "::1".parse().unwrap(); // @Temporary
    let peers = vec![bandwidth_test::STPAddress { ip, port: P2P_PORT, magic1: seeder_keypair.magic1, key: seeder_keypair.public }];
    if args.len() == 1 {
        p2p_test::p2p(0, None, peers);
        return;
    }
    let mut i: usize = usize::MAX;
    if args.len() > 1 {
        i = args[1].parse().unwrap_or(usize::MAX);
    }
    println!("Command line: {:?}", args);
    run_instances(i);
}

