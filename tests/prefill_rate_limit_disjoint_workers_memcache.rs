//! `workload.prefill_rate_limit` over memcache when workers serve different
//! endpoints and run out of prefill keys at different times (zipf keys,
//! theta 1.5, `key_share` allocation; see the resp test of the same name).
//! 8000 keys at 4000 per second take about two seconds. Taking a token before
//! popping a key spent tokens on drained endpoints and took longer.

mod common;

#[test]
fn workers_whose_memcache_endpoints_drained_leave_the_rate_to_the_others() {
    let shape = common::Shape {
        endpoints: 4,
        threads: 16,
        connections: 16,
        pipeline: 1,
        keyspace: "distribution = \"zipf\"\nzipf_theta = 1.5",
        connection: "endpoint_allocation = \"key_share\"",
    };
    let times = common::prefill_times_shaped("memcache", shape, 8000, 4000);
    common::assert_paced_for_two_seconds(&times);
}
