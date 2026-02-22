use super::*;

pub fn generate(
    data: &Arc<Tsdb>,
    _server: Option<&Arc<Tsdb>>,
    _client: Option<&Arc<Tsdb>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, sections);

    // All latency percentiles in one flat group
    let mut latency = Group::new("Latency Percentiles", "latency-percentiles");

    // Overall response latency
    latency.plot_promql(
        PlotOpts::scatter("Response", "response-latency-pct", Unit::Time).with_log_scale(true),
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], response_latency)".to_string(),
    );

    // GET latency
    latency.plot_promql(
        PlotOpts::scatter("GET", "get-latency-pct", Unit::Time).with_log_scale(true),
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], get_latency)".to_string(),
    );

    // SET latency
    latency.plot_promql(
        PlotOpts::scatter("SET", "set-latency-pct", Unit::Time).with_log_scale(true),
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], set_latency)".to_string(),
    );

    // DELETE latency
    latency.plot_promql(
        PlotOpts::scatter("DELETE", "delete-latency-pct", Unit::Time).with_log_scale(true),
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], delete_latency)".to_string(),
    );

    view.group(latency);

    view
}
