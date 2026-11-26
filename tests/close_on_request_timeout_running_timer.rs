//! The server answers the first command on each connection 50 ms late, so
//! the recv for the next request runs under the deadline timer armed for the
//! first, and that timer fires at least 50 ms before the next request's
//! deadline; see
//! `timeout_harness` for what is measured.

mod timeout_harness;

use std::time::Duration;
use timeout_harness::{assert_closed_at_the_deadline, closes};

/// The timer left running by the answered request fires first. The client must
/// check the next request's deadline then, re-arm for the time that remains
/// (at least 50 ms), and close at that deadline.
#[test]
fn a_timer_left_running_by_an_answered_request_closes_at_the_next_deadline() {
    assert_closed_at_the_deadline(&closes(1, Duration::from_millis(50)));
}
