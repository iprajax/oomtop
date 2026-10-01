// macOS: jemalloc returns freed pages to the kernel at once, so the TUI's RSS tracks what is in use instead
// of the system allocator's high-water mark. libmalloc keeps freed MALLOC_SMALL pages resident as
// "reusable" (out of the footprint, still in RSS), which put the TUI at ~45 MB RSS against SPEC §14's 40 MB
// with only ~19 MB footprint. Measured over 690 s on this Mac (SPEC §21): TUI RSS 45.0 → 28.8 MB, footprint
// 19.3 → 19.0 MB, CPU unchanged; headless RSS 30.0 → 23.0 MB. Linux keeps the system allocator.
#[cfg(target_os = "macos")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// jemalloc options, read at its first allocation (`_RJEM_MALLOC_CONF` in the environment overrides them):
/// one arena and no thread caches (a monitor is not allocation-bound), and freed pages purged immediately
/// instead of after jemalloc's default 10 s decay — a lazily purged page still counts as resident on macOS.
#[cfg(target_os = "macos")]
#[allow(non_upper_case_globals)]
#[export_name = "_rjem_malloc_conf"]
pub static malloc_conf: &[u8] = b"narenas:1,tcache:false,dirty_decay_ms:0,muzzy_decay_ms:0\0";

fn main() {
    std::process::exit(oomtop_cli::run(std::env::args_os()));
}
