use super::*;

pub fn generate(
    data: &Arc<ParquetReader>,
    _server: Option<&Arc<ParquetReader>>,
    _client: Option<&Arc<ParquetReader>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, sections);

    // Request/Response rates
    let mut requests = Group::new("Request/Response Rates", "request-rates");

    requests.plot(
        Plot::counter("Responses/sec", "responses-received", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(responses_received[10s])"),
    );

    requests.plot(
        Plot::counter("Error Rate", "error-rate", Unit::Percentage)
            .percentage_range()
            .with_min_y_span(0.001)
            .with_style(ChartStyle::Line)
            .build("irate(request_errors[10s]) / irate(requests_sent[10s])"),
    );

    view.group(requests);

    // Bytes throughput
    let mut bytes = Group::new("Network Throughput", "bytes");

    bytes.plot(
        Plot::counter("TX Bytes/sec", "bytes-tx", Unit::Datarate)
            .with_style(ChartStyle::Line)
            .build("irate(bytes_tx[10s])"),
    );

    bytes.plot(
        Plot::counter("RX Bytes/sec", "bytes-rx", Unit::Datarate)
            .with_style(ChartStyle::Line)
            .build("irate(bytes_rx[10s])"),
    );

    view.group(bytes);

    // Operation rates
    let mut ops = Group::new("Operation Rates", "operations");

    ops.plot(
        Plot::counter("GET/sec", "get-rate", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(get_count[10s])"),
    );

    ops.plot(
        Plot::counter("SET/sec", "set-rate", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(set_count[10s])"),
    );

    ops.plot(
        Plot::counter("DELETE/sec", "delete-rate", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(delete_count[10s])"),
    );

    view.group(ops);

    view
}
