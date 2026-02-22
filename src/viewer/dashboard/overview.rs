use super::*;

pub fn generate(
    data: &Arc<Tsdb>,
    _server: Option<&Arc<Tsdb>>,
    _client: Option<&Arc<Tsdb>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, sections);

    // Key metrics - single flat section
    let mut metrics = Group::new("Key Metrics", "key-metrics");

    // Throughput
    metrics.plot_promql(
        PlotOpts::line("Throughput", "throughput", Unit::Rate),
        "irate(responses_received[10s])".to_string(),
    );

    // Hit rate
    metrics.plot_promql(
        PlotOpts::line("Hit Rate", "hit-rate", Unit::Percentage),
        "irate(cache_hits[10s]) / (irate(cache_hits[10s]) + irate(cache_misses[10s]))".to_string(),
    );

    // Latency percentiles
    metrics.plot_promql(
        PlotOpts::scatter("Latency", "latency", Unit::Time).with_log_scale(true),
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], response_latency)".to_string(),
    );

    // Error rate
    metrics.plot_promql(
        PlotOpts::line("Error Rate", "error-rate", Unit::Percentage),
        "irate(request_errors[10s]) / irate(requests_sent[10s])".to_string(),
    );

    // Bandwidth TX (convert bytes to bits)
    metrics.plot_promql(
        PlotOpts::line("TX Bandwidth", "bandwidth-tx", Unit::Bitrate),
        "irate(bytes_tx[10s]) * 8".to_string(),
    );

    // Bandwidth RX (convert bytes to bits)
    metrics.plot_promql(
        PlotOpts::line("RX Bandwidth", "bandwidth-rx", Unit::Bitrate),
        "irate(bytes_rx[10s]) * 8".to_string(),
    );

    view.group(metrics);

    view
}
