// Mirror `ringline`/`ringline-redis`'s backend detection so cachecannon can
// gate the io_uring-only zero-copy recv path (`recv_meta`) on the same
// condition the dependency compiled it under. cachecannon never enables
// `ringline/force-mio`, so `has_io_uring` here is simply Linux 6.0+ — matching
// ringline's default backend selection for the same cargo invocation:
//   - default Linux 6.0+ build → io_uring backend → `has_io_uring`
//   - non-Linux (macOS) / older kernel → mio backend → no cfg
//
// Cargo auto-registers cfgs set via `cargo:rustc-cfg` for check-cfg; the
// `[lints.rust.unexpected_cfgs]` stanza in Cargo.toml keeps the cfg recognized
// on targets where this build.rs does not emit it.
fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "linux" && kernel_version_sufficient() {
        println!("cargo:rustc-cfg=has_io_uring");
    }
}

/// Check that the running kernel is 6.0+ (required for the multishot-recv +
/// provided-buffer features segmented recv depends on). Kept identical to
/// `ringline-redis/build.rs`.
fn kernel_version_sufficient() -> bool {
    let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") else {
        // Not on Linux (cross-compile from macOS) or /proc not mounted.
        // Optimistically enable — `ringline` itself gates the real backend.
        return true;
    };
    let mut parts = release.trim().split('.');
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts
        .next()
        .and_then(|s| {
            let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        })
        .unwrap_or(0);
    (major, minor) >= (6, 0)
}
