//! A run with `rate_limit` after a prefill starts at its rate: the limiters
//! refill while prefill runs and must be reset when warmup begins. A fake RESP
//! server timestamps the GETs of a one-second run at 2,000 per second after a
//! 1,000-key prefill, which it slows to at most about 1,000 SETs per second by
//! holding each reply (`start::PREFILL_SET_DELAY`); the first 100 ms may carry
//! about 200 of the GETs. The prefill lasts at least a second, so a reset made
//! at the start of prefill instead of its end releases a second of tokens.
//! Without the reset at the end of prefill, about 2,200 arrived at once.

mod start;

#[test]
fn the_run_does_not_open_with_a_burst() {
    let arrivals = start::arrivals(true);
    // A run that opens with the burst puts about 2,200 in the first 100 ms.
    // The ceiling is 250 ms of the rate (500). Arrivals are timed from the
    // first request, and tokens accrue from the limiter restart, so a stall
    // between the two adds about 20 requests per 10 ms; a 120 ms stall
    // passes.
    let early = start::within(&arrivals, 100);
    assert!(
        early <= start::RATE / 4,
        "{early} requests in the first 100 ms"
    );
    // The floor is 80% of the first 500 ms (1,000 requests). A restart that
    // leaves the cap narrowed sends fewer. Tokens that accrue during a stall
    // are spent after it, so a stall at the start does not lower this count.
    let half = start::within(&arrivals, 500);
    assert!(
        half >= start::RATE * 4 / 10,
        "{half} requests in the first 500 ms"
    );
}
