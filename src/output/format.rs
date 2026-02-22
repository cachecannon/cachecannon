//! Number formatting utilities for benchmark output.

/// Format a rate (requests/second) with SI suffixes and 3 significant figures.
/// - < 1K: "X.XX", "XX.X", or "XXX"
/// - 1K - 999K: "X.XXK", "XX.XK", or "XXXK"
/// - >= 1M: "X.XXM", "XX.XM", or "XXXM"
pub fn format_rate(value: f64) -> String {
    if value < 1_000.0 {
        format_3sig(value, "")
    } else if value < 1_000_000.0 {
        format_3sig(value / 1_000.0, "K")
    } else {
        format_3sig(value / 1_000_000.0, "M")
    }
}

/// Format a number with 3 significant figures and an optional suffix.
fn format_3sig(value: f64, suffix: &str) -> String {
    // Account for rounding: 9.995 rounds to 10.00, 99.95 rounds to 100.0
    if value < 9.995 {
        format!("{:.2}{}", value, suffix)
    } else if value < 99.95 {
        format!("{:.1}{}", value, suffix)
    } else {
        format!("{:.0}{}", value, suffix)
    }
}

/// Format a rate with padding for table alignment.
pub fn format_rate_padded(value: f64, width: usize) -> String {
    let s = format_rate(value);
    format!("{:>width$}", s, width = width)
}

/// Format a latency value in microseconds with autoscaling and 3 significant figures.
/// - < 1000us: "XXXus" or "X.Xus" or "XX.Xus"
/// - 1ms - 999ms: "X.XXms" or "XX.Xms" or "XXXms"
/// - >= 1s: "X.XXs" or "XX.Xs" or "XXXs"
pub fn format_latency_us(us: f64) -> String {
    if us < 1_000.0 {
        // Microseconds
        if us < 10.0 {
            format!("{:.2}us", us)
        } else if us < 100.0 {
            format!("{:.1}us", us)
        } else {
            format!("{:.0}us", us)
        }
    } else if us < 1_000_000.0 {
        // Milliseconds
        let ms = us / 1_000.0;
        if ms < 10.0 {
            format!("{:.2}ms", ms)
        } else if ms < 100.0 {
            format!("{:.1}ms", ms)
        } else {
            format!("{:.0}ms", ms)
        }
    } else {
        // Seconds
        let s = us / 1_000_000.0;
        if s < 10.0 {
            format!("{:.2}s", s)
        } else if s < 100.0 {
            format!("{:.1}s", s)
        } else {
            format!("{:.0}s", s)
        }
    }
}

/// Format a latency with padding for table alignment.
pub fn format_latency_padded(us: f64, width: usize) -> String {
    let s = format_latency_us(us);
    format!("{:>width$}", s, width = width)
}

/// Format a percentage with 3 significant figures.
/// - >= 100%: no decimals (e.g., "100")
/// - >= 10%: 1 decimal (e.g., "95.2")
/// - < 10%: 2 decimals (e.g., "9.99", "0.01")
///
/// Note: Values that round to 100 are shown without decimals.
pub fn format_pct(value: f64) -> String {
    if value >= 99.95 {
        // Would round to 100.0 with 1 decimal, so show as integer
        format!("{:.0}", value)
    } else if value >= 10.0 {
        format!("{:.1}", value)
    } else {
        format!("{:.2}", value)
    }
}

/// Format a percentage with padding for table alignment.
pub fn format_pct_padded(value: f64, width: usize) -> String {
    let s = format_pct(value);
    format!("{:>width$}", s, width = width)
}

/// Format bandwidth in bits per second with SI suffixes.
/// - < 1K: "XXX bps"
/// - 1K - 999.9K: "XXX.X Kbps"
/// - 1M - 999.9M: "XXX.X Mbps"
/// - >= 1G: "X.XX Gbps"
pub fn format_bandwidth_bps(bps: f64) -> String {
    if bps < 1_000.0 {
        format!("{:.0} bps", bps)
    } else if bps < 1_000_000.0 {
        format!("{:.1} Kbps", bps / 1_000.0)
    } else if bps < 1_000_000_000.0 {
        format!("{:.1} Mbps", bps / 1_000_000.0)
    } else {
        format!("{:.2} Gbps", bps / 1_000_000_000.0)
    }
}

/// Format a count with SI suffixes for display.
/// - < 1K: raw number
/// - 1K - 999.9K: "XXX.XK"
/// - >= 1M: "X.XM"
pub fn format_count(value: u64) -> String {
    let v = value as f64;
    if v < 1_000.0 {
        format!("{}", value)
    } else if v < 1_000_000.0 {
        format!("{:.1}K", v / 1_000.0)
    } else if v < 1_000_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else {
        format!("{:.1}B", v / 1_000_000_000.0)
    }
}

/// Format bytes with SI suffixes.
pub fn format_bytes(bytes: u64) -> String {
    let v = bytes as f64;
    if v < 1_024.0 {
        format!("{}B", bytes)
    } else if v < 1_024.0 * 1_024.0 {
        format!("{:.1}KB", v / 1_024.0)
    } else if v < 1_024.0 * 1_024.0 * 1_024.0 {
        format!("{:.1}MB", v / (1_024.0 * 1_024.0))
    } else {
        format!("{:.2}GB", v / (1_024.0 * 1_024.0 * 1_024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_rate() {
        // 3 significant figures without suffix
        assert_eq!(format_rate(1.23), "1.23");
        assert_eq!(format_rate(12.3), "12.3");
        assert_eq!(format_rate(500.0), "500");
        // 3 significant figures with K suffix
        assert_eq!(format_rate(1_230.0), "1.23K");
        assert_eq!(format_rate(12_300.0), "12.3K");
        assert_eq!(format_rate(234_000.0), "234K");
        // 3 significant figures with M suffix
        assert_eq!(format_rate(1_230_000.0), "1.23M");
        assert_eq!(format_rate(12_300_000.0), "12.3M");
        assert_eq!(format_rate(234_000_000.0), "234M");
        // Edge cases: values that round up at boundaries
        assert_eq!(format_rate(99_950.0), "100K"); // not "100.0K"
        assert_eq!(format_rate(100_000.0), "100K");
        assert_eq!(format_rate(9_995.0), "10.0K"); // not "10.00K"
    }

    #[test]
    fn test_format_latency() {
        // Microseconds - 3 significant figures
        assert_eq!(format_latency_us(1.23), "1.23us");
        assert_eq!(format_latency_us(42.5), "42.5us");
        assert_eq!(format_latency_us(999.0), "999us");
        // Milliseconds - 3 significant figures
        assert_eq!(format_latency_us(1_230.0), "1.23ms");
        assert_eq!(format_latency_us(42_500.0), "42.5ms");
        assert_eq!(format_latency_us(200_000.0), "200ms");
        // Seconds - 3 significant figures
        assert_eq!(format_latency_us(1_230_000.0), "1.23s");
        assert_eq!(format_latency_us(42_500_000.0), "42.5s");
        assert_eq!(format_latency_us(200_000_000.0), "200s");
    }

    #[test]
    fn test_format_pct() {
        // 3 significant figures
        assert_eq!(format_pct(100.0), "100");
        assert_eq!(format_pct(95.23), "95.2");
        assert_eq!(format_pct(9.99), "9.99");
        assert_eq!(format_pct(0.01), "0.01");
        assert_eq!(format_pct(0.0), "0.00");
    }

    #[test]
    fn test_format_bandwidth() {
        assert_eq!(format_bandwidth_bps(500.0), "500 bps");
        assert_eq!(format_bandwidth_bps(1_500.0), "1.5 Kbps");
        assert_eq!(format_bandwidth_bps(892_000_000.0), "892.0 Mbps");
        assert_eq!(format_bandwidth_bps(1_240_000_000.0), "1.24 Gbps");
    }
}
