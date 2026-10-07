//! `connection.disconnect_rate` over plain TCP.
//!
//! One test per file: the benchmark uses process-global metrics and a global
//! config channel, so it must not share a process with another run.

mod common;

#[test]
fn injected_disconnects_close_busy_connections_and_reconnect() {
    let (addr, counters) = common::start_stub(common::serve);
    common::run_and_check(addr, &counters, "");
}
