use super::*;

/// Generate client metrics dashboard using rezolus data
pub fn generate(
    _benchmark: &Arc<Tsdb>,
    _server: Option<&Arc<Tsdb>>,
    client: Option<&Arc<Tsdb>>,
    sections: Vec<Section>,
) -> View {
    // Use client data
    let data = client.unwrap();
    let mut view = View::new(data, sections);

    // CPU utilization
    let mut cpu = Group::new("CPU Utilization", "cpu");

    cpu.plot_promql(
        PlotOpts::line("CPU Usage %", "cpu-usage", Unit::Percentage),
        "sum(irate(cpu_usage{state=\"user|system\"}[5m])) / cpu_cores * 100".to_string(),
    );

    view.group(cpu);

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

    view
}
