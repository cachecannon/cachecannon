use super::*;

/// Generate server metrics dashboard using rezolus data
pub fn generate(
    _benchmark: &Arc<ParquetReader>,
    server: Option<&Arc<ParquetReader>>,
    _client: Option<&Arc<ParquetReader>>,
    sections: Vec<Section>,
) -> View {
    // Use server data if available, otherwise use benchmark data for structure
    let data = server.expect("server::generate called without server data");
    let mut view = View::new(data, sections);

    // CPU utilization
    let mut cpu = Group::new("CPU Utilization", "cpu");

    cpu.plot(
        Plot::counter("CPU Usage %", "cpu-usage", Unit::Percentage)
            .with_style(ChartStyle::Line)
            .build("sum(irate(cpu_usage{state=\"user|system\"}[5m])) / cpu_cores * 100"),
    );

    view.group(cpu);

    // Memory usage
    let mut memory = Group::new("Memory", "memory");

    memory.plot(
        Plot::gauge("Memory Used", "memory-used", Unit::Bytes)
            .with_style(ChartStyle::Line)
            .build("memory_total - memory_free"),
    );

    view.group(memory);

    // Network traffic
    let mut network = Group::new("Network", "network");

    network.plot(
        Plot::counter("TX Bytes/sec", "network-tx", Unit::Datarate)
            .with_style(ChartStyle::Line)
            .build("sum(irate(network_bytes{direction=\"transmit\"}[5m]))"),
    );

    network.plot(
        Plot::counter("RX Bytes/sec", "network-rx", Unit::Datarate)
            .with_style(ChartStyle::Line)
            .build("sum(irate(network_bytes{direction=\"receive\"}[5m]))"),
    );

    view.group(network);

    // Scheduler metrics
    let mut scheduler = Group::new("Scheduler", "scheduler");

    scheduler.plot(latency_percentiles(
        "Runqueue Latency",
        "runqueue-latency",
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999], scheduler_runqueue_latency)",
    ));

    view.group(scheduler);

    view
}
