use super::*;

/// Generate client metrics dashboard using rezolus data
pub fn generate(
    _benchmark: &Arc<ParquetReader>,
    _server: Option<&Arc<ParquetReader>>,
    client: Option<&Arc<ParquetReader>>,
    sections: Vec<Section>,
) -> View {
    // Use client data
    let data = client.expect("client::generate called without client data");
    let mut view = View::new(data, sections);

    // CPU utilization
    let mut cpu = Group::new("CPU Utilization", "cpu");

    cpu.plot(
        Plot::counter("CPU Usage %", "cpu-usage", Unit::Percentage)
            .with_style(ChartStyle::Line)
            .build("sum(irate(cpu_usage{state=\"user|system\"}[5m])) / cpu_cores * 100"),
    );

    view.group(cpu);

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

    view
}
