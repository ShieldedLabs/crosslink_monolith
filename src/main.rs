fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut i: usize = usize::MAX;
    if args.len() > 1 {
        i = args[1].parse().unwrap_or(usize::MAX);
    }
    // println!("Command line: {:?}", args);
    tenderlink::run_instances(i);
}
