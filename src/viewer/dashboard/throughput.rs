use super::*;

pub fn generate(
    data: &Arc<Tsdb>,
    _server: Option<&Arc<Tsdb>>,
    _client: Option<&Arc<Tsdb>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, sections);

    // Request/Response rates
    let mut requests = Group::new("Request/Response Rates", "request-rates");

    requests.plot_promql(
        PlotOpts::line("Responses/sec", "responses-received", Unit::Rate),
        "irate(responses_received[10s])".to_string(),
    );

    requests.plot_promql(
        PlotOpts::line("Error Rate", "error-rate", Unit::Percentage),
        "irate(request_errors[10s]) / irate(requests_sent[10s])".to_string(),
    );

    view.group(requests);

    // Bytes throughput
    let mut bytes = Group::new("Network Throughput", "bytes");

    bytes.plot_promql(
        PlotOpts::line("TX Bytes/sec", "bytes-tx", Unit::Datarate),
        "irate(bytes_tx[10s])".to_string(),
    );

    bytes.plot_promql(
        PlotOpts::line("RX Bytes/sec", "bytes-rx", Unit::Datarate),
        "irate(bytes_rx[10s])".to_string(),
    );

    view.group(bytes);

    // Operation rates
    let mut ops = Group::new("Operation Rates", "operations");

    ops.plot_promql(
        PlotOpts::line("GET/sec", "get-rate", Unit::Rate),
        "irate(get_count[10s])".to_string(),
    );

    ops.plot_promql(
        PlotOpts::line("SET/sec", "set-rate", Unit::Rate),
        "irate(set_count[10s])".to_string(),
    );

    ops.plot_promql(
        PlotOpts::line("DELETE/sec", "delete-rate", Unit::Rate),
        "irate(delete_count[10s])".to_string(),
    );

    view.group(ops);

    view
}
