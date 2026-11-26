//! The server answers no workload command, so each recv is the first on its
//! connection and arms the deadline timer; see `timeout_harness` for what is
//! measured.

mod timeout_harness;

use std::time::Duration;
use timeout_harness::{assert_closed_at_the_deadline, closes};

#[test]
fn a_request_timeout_closes_the_connection_at_the_deadline() {
    assert_closed_at_the_deadline(&closes(0, Duration::ZERO));
}
