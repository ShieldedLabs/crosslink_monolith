fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 {
        if args[1] == "reflector" {
            let port : u16 = args[2].parse().unwrap();
            println!("running reflector on port: {}", port);
            tenderlink::bandwidth_test::do_the_reflector(port);
            return;
        }
        if args[1] == "beamer" {
            let port : u16 = args[2].parse().unwrap();
            let other_addr : std::net::Ipv6Addr = args[3].parse().unwrap();
            let other_port : u16 = args[4].parse().unwrap();
            println!("running beamer on port: {}", port);
            println!("connecting to {} port {}", other_addr, other_port);
            tenderlink::bandwidth_test::do_the_test_program(port, other_addr, other_port);
            return;
        }
    }
    let mut i: usize = usize::MAX;
    if args.len() > 1 {
        i = args[1].parse().unwrap_or(usize::MAX);
    }
    // println!("Command line: {:?}", args);
    tenderlink::run_instances(i);
}
