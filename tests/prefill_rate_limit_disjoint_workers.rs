//! `workload.prefill_rate_limit` when workers serve different endpoints and
//! run out of prefill keys at different times. With zipf keys (theta 1.5) and
//! `key_share` allocation, the endpoint owning the hottest keys gets most of
//! the connections but only about a quarter of the prefill keys, so the
//! workers serving it drain first. The others must be able to use the whole
//! rate: 8000 keys at 4000 per second take about two seconds. With the rate
//! split evenly per worker it took 3 to 9 s. The bounds, and how they allow
//! for a busy test host, are in `common::assert_paced_for_two_seconds`.

mod common;

#[test]
fn workers_whose_endpoints_drained_leave_the_rate_to_the_others() {
    let shape = common::Shape {
        endpoints: 4,
        threads: 16,
        connections: 16,
        pipeline: 1,
        keyspace: "distribution = \"zipf\"\nzipf_theta = 1.5",
        connection: "endpoint_allocation = \"key_share\"",
    };
    let times = common::prefill_times_shaped("resp", shape, 8000, 4000);
    common::assert_paced_for_two_seconds(&times);
}
