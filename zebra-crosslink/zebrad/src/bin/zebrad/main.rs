//! Main entry point for Zebrad

use zebrad::application::{boot, APPLICATION};

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// #[global_allocator]
// static ALLOC: std::alloc::System = std::alloc::System;


/// Process entry point for `zebrad`
fn main() {
    boot(&APPLICATION);
}
