use tenderlink::*;

fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("panic: {info}");
        #[cfg(all(windows, debug_assertions))] unsafe { unsafe extern "system" { fn DebugBreak(); } DebugBreak(); }
        // #[cfg(all(unix, debug_assertions))]    unsafe { libc::raise(libc::SIGTRAP); }
    }));

    let args: Vec<String> = std::env::args().collect();
    // println!("Command line: {:?}", args);

    if args.len() > 2 {
        use crate::bandwidth_test::*;
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

    const P2P_PORT: u16 = 12345;

    const SEEDER_CRYPTO: u64 = bandwidth_test::CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b;

    tenderlink::run_instances(if args.contains(&"p2p".to_string()) { 0 } else { 1 });

    let seed = [7, 11, 99, 220, 101, 143, 113,   4, 242, 136,  58, 150, 223, 186, 106, 203,
               67, 18, 48,  96, 176,  69, 152, 173, 224,  46, 206, 156, 217,  31, 170, 165];

    let seeder_keypair = bandwidth_test::new_keypair_from_connect_magic1_with_seed(SEEDER_CRYPTO, seed).unwrap();

    let port: u16 = {
        if let Some(i) = args.iter().position(|x| x == "-port") {
            args.get(i + 1).map(|a| a.parse().unwrap()).unwrap_or(P2P_PORT)
        } else {
            P2P_PORT
        }
    };

    let p2p_seeder = args.len() > 1 && args[1] == "p2p";
    let use_ipv4 = !args.contains(&"-no_ipv4".to_string());
    let use_ipv6 = !args.contains(&"-no_ipv6".to_string());
    let localhost = args.contains(&"-localhost".to_string());

    if p2p_seeder {
        p2p_test::p2p(port, SEEDER_CRYPTO, Some(seeder_keypair), vec![], true, true);
        return;
    }

    let (ip4, ip6) = if localhost {
        ("::ffff:127.0.0.1", "::1")
    } else {
        ("::ffff:70.34.242.155", "2a05:f480:2400:13a9:5400:05ff:fefb:77ae")
    };
    let (ip4, ip6) = (ip4.parse().unwrap(), ip6.parse().unwrap());

    let mut peers = Vec::with_capacity(2);
    if use_ipv4 { peers.push(bandwidth_test::STPAddress::from(ip4, P2P_PORT, &seeder_keypair)); }
    if use_ipv6 { peers.push(bandwidth_test::STPAddress::from(ip6, P2P_PORT, &seeder_keypair)); }

    p2p_test::p2p(0, SEEDER_CRYPTO, None, peers, use_ipv4, use_ipv6);
}

