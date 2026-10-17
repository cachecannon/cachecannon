//! A run with `rate_limit` and no prefill starts at its rate: a fake RESP
//! server timestamps the SETs of a one-second run at 2,000 per second, and
//! the first 100 ms may carry about 200 of them. Each worker's limiter used to
//! start full and refill while connections opened, and the run opened by
//! sending about 2,200 at once.

mod start;

#[test]
fn the_run_does_not_open_with_a_burst() {
    let early = start::first_100ms(false);
    // 100 ms at 2,000/s is 200; allow one batch per connection on top. The
    // floor is 80% of it: a restart that left the cap narrowed sent 131-141.
    assert!(
        early <= start::RATE / 10 + 16 * 4,
        "{early} requests in the first 100 ms"
    );
    assert!(
        early >= start::RATE * 8 / 100,
        "{early} requests in the first 100 ms"
    );
}
