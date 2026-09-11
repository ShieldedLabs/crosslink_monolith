//! Main entry point for Zebrad

use zebrad::application::{boot, APPLICATION};

/// Process entry point for `zebrad`
fn main() {
    // glibc defaults to ~8 arenas per CPU. Packet Vec churn pins RSS in each
    // one if we wait until after tokio/STP threads exist.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        #[allow(unsafe_code)]
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, 2);
            libc::malloc_trim(0);
        }
    }

    boot(&APPLICATION);
}
