//! A run with `rate_limit` after a prefill starts at its rate: the limiters
//! refill while prefill runs and must be reset when warmup begins. A fake RESP
//! server timestamps the GETs of a one-second run at 2,000 per second after a
//! 1,000-key prefill; the first 100 ms may carry about 200 of them. Without
//! the reset at the end of prefill, about 1,400 arrived at once.

mod start;

#[test]
fn the_run_does_not_open_with_a_burst() {
    let early = start::first_100ms(true);
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
