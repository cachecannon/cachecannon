use super::*;

/// Generate server metrics dashboard using rezolus data
pub fn generate(
    _benchmark: &Arc<Tsdb>,
    server: Option<&Arc<Tsdb>>,
    _client: Option<&Arc<Tsdb>>,
    sections: Vec<Section>,
) -> View {
    // Use server data if available, otherwise use benchmark data for structure
    let data = server.unwrap();
    let mut view = View::new(data, sections);

    // CPU utilization
    let mut cpu = Group::new("CPU Utilization", "cpu");

    cpu.plot_promql(
        PlotOpts::line("CPU Usage %", "cpu-usage", Unit::Percentage),
        "sum(irate(cpu_usage{state=\"user|system\"}[5m])) / cpu_cores * 100".to_string(),
    );

    view.group(cpu);

    // Memory usage
    let mut memory = Group::new("Memory", "memory");

    memory.plot_promql(
        PlotOpts::line("Memory Used", "memory-used", Unit::Bytes),
        "memory_total - memory_free".to_string(),
    );

    view.group(memory);

    // Network traffic
    let mut network = Group::new("Network", "network");

    network.plot_promql(
        PlotOpts::line("TX Bytes/sec", "network-tx", Unit::Datarate),
        "sum(irate(network_bytes{direction=\"transmit\"}[5m]))".to_string(),
    );

    network.plot_promql(
        PlotOpts::line("RX Bytes/sec", "network-rx", Unit::Datarate),
        "sum(irate(network_bytes{direction=\"receive\"}[5m]))".to_string(),
    );

    view.group(network);

    // Scheduler metrics
    let mut scheduler = Group::new("Scheduler", "scheduler");

    scheduler.plot_promql(
        PlotOpts::scatter("Runqueue Latency", "runqueue-latency", Unit::Time).with_log_scale(true),
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999], scheduler_runqueue_latency)".to_string(),
    );

    view.group(scheduler);

    view
}
