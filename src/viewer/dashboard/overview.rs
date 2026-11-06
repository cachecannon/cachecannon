use super::*;

pub fn generate(
    data: &Arc<ParquetReader>,
    _server: Option<&Arc<ParquetReader>>,
    _client: Option<&Arc<ParquetReader>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, api::BENCHMARK_ROOT, sections);

    // Key metrics - single flat section
    let mut metrics = Group::new("Key Metrics", "key-metrics");

    // Throughput
    metrics.plot(
        Plot::counter("Throughput", "throughput", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(responses_received[10s])"),
    );

    // Hit rate
    metrics.plot(
        Plot::counter("Hit Rate", "hit-rate", Unit::Percentage)
            .percentage_range()
            .with_min_y_span(0.01)
            .with_style(ChartStyle::Line)
            .build("irate(cache_hits[10s]) / (irate(cache_hits[10s]) + irate(cache_misses[10s]))"),
    );

    // Latency percentiles
    metrics.plot(latency_percentiles(
        "Latency",
        "latency",
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], response_latency)",
    ));

    // Error rate
    metrics.plot(
        Plot::counter("Error Rate", "error-rate", Unit::Percentage)
            .percentage_range()
            .with_min_y_span(0.001)
            .with_style(ChartStyle::Line)
            .build("irate(request_errors[10s]) / irate(requests_sent[10s])"),
    );

    // Bandwidth TX (convert bytes to bits)
    metrics.plot(
        Plot::counter("TX Bandwidth", "bandwidth-tx", Unit::Bitrate)
            .with_style(ChartStyle::Line)
            .build("irate(bytes_tx[10s]) * 8"),
    );

    // Bandwidth RX (convert bytes to bits)
    metrics.plot(
        Plot::counter("RX Bandwidth", "bandwidth-rx", Unit::Bitrate)
            .with_style(ChartStyle::Line)
            .build("irate(bytes_rx[10s]) * 8"),
    );

    view.group(metrics);

    view
}
