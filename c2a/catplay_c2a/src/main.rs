#![recursion_limit = "256"]

use std::process::exit;

use catplay_c2a::Main;
use catplay_tracing::logger::{AsyncLogger, setup_prod_logger};
use catplay_tracing::tracer::SessionTracer;
use log::{error, info, warn};

#[cfg(all(feature = "jemalloc", feature = "mimalloc"))]
compile_error!("features `jemalloc` and `mimalloc` are mutually exclusive");

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(all(feature = "jemalloc", not(target_arch = "riscv32")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "jemalloc", not(target_arch = "riscv32")))]
use tikv_jemalloc_sys as _;

#[cfg(all(feature = "jemalloc", not(target_arch = "riscv32")))]
fn disable_jemalloc_thread_cache() {
    let mut enabled = false;
    let result = unsafe {
        tikv_jemalloc_sys::mallctl(
            c"thread.tcache.enabled".as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut enabled as *mut _ as *mut libc::c_void,
            std::mem::size_of_val(&enabled),
        )
    };
    if result != 0 {
        eprintln!("failed to disable jemalloc thread cache: mallctl returned {result}");
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    #[cfg(all(feature = "jemalloc", not(target_arch = "riscv32")))]
    disable_jemalloc_thread_cache();

    println!("Booting!!");
    setup_prod_logger();
    AsyncLogger::set_async(true);
    catplay_util::timeline::mark(catplay_util::timeline::Stage::AppStart);
    let b = AsyncLogger::create_barrier();
    if std::env::var("CATPLAY_TRACING").as_deref() == Ok("1") {
        warn!("Tracing is enabled (overhead)");
        SessionTracer::enable_globally();
    }

    info!("Calling start()");

    let mut main = Main::start().await;
    match main {
        Err(err) => {
            error!("Failed to start: {}", err);
        }
        Ok(ref mut main) => {
            info!("Started");

            if let Err(err) = main.do_loop().await {
                error!("Error during reconcile: {}", err);
            }
        }
    }

    drop(b);
    exit(1);
}
