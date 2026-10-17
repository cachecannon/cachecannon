//! A run with `rate_limit` and no prefill starts at its rate: a fake RESP
//! server timestamps the SETs of a one-second run at 2,000 per second, and
//! the first 100 ms may carry about 200 of them. Each worker's limiter used to
//! start full and refill while connections opened, and the run opened by
//! sending about 2,200 at once.

mod start;

#[test]
fn the_run_does_not_open_with_a_burst() {
    let arrivals = start::arrivals(false);
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
    // left the cap narrowed sent about 650-700. Tokens that accrue during a
    // stall are spent after it, so a stall at the start does not lower this
    // count.
    let half = start::within(&arrivals, 500);
    assert!(
        half >= start::RATE * 4 / 10,
        "{half} requests in the first 500 ms"
    );
}
