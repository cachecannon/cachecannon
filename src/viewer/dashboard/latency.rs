use super::*;

pub fn generate(
    data: &Arc<ParquetReader>,
    _server: Option<&Arc<ParquetReader>>,
    _client: Option<&Arc<ParquetReader>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, sections);

    // All latency percentiles in one flat group
    let mut latency = Group::new("Latency Percentiles", "latency-percentiles");

    // Overall response latency
    latency.plot(latency_percentiles(
        "Response",
        "response-latency-pct",
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], response_latency)",
    ));

    // GET latency
    latency.plot(latency_percentiles(
        "GET",
        "get-latency-pct",
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], get_latency)",
    ));

    // SET latency
    latency.plot(latency_percentiles(
        "SET",
        "set-latency-pct",
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], set_latency)",
    ));

    // DELETE latency
    latency.plot(latency_percentiles(
        "DELETE",
        "delete-latency-pct",
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], delete_latency)",
    ));

    // Schedule slip (queueing the latency clock omits)
    latency.plot(latency_percentiles(
        "Schedule Slip",
        "schedule-slip-pct",
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], schedule_slip)",
    ));

    // Perceived (arrival-relative, CO-honest) latency
    latency.plot(latency_percentiles(
        "Perceived",
        "perceived-latency-pct",
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], perceived_latency)",
    ));

    view.group(latency);

    view
}
