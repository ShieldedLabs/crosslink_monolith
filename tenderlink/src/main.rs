fn main() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("panic: {info}");
        #[cfg(all(windows, debug_assertions))] unsafe { unsafe extern "system" { fn DebugBreak(); } DebugBreak(); }
        // #[cfg(all(unix, debug_assertions))]    unsafe { libc::raise(libc::SIGTRAP); }
    }));

    // The 0.x reflector/beamer modes lived in the STP crate and were removed in 1.0;
    // STP's own examples (lockfree_thread, lockfree_flood) cover that ground now.

    let args: Vec<String> = std::env::args().collect();
    let mut i: usize = usize::MAX;
    if args.len() > 1 {
        i = args[1].parse().unwrap_or(usize::MAX);
    }
    println!("Command line: {:?}", args);
    tenderlink::run_instances(i);
}

