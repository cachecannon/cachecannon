use super::*;

pub fn generate(
    data: &Arc<ParquetReader>,
    _server: Option<&Arc<ParquetReader>>,
    _client: Option<&Arc<ParquetReader>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, api::BENCHMARK_ROOT, sections);

    // Connection metrics
    let mut connections = Group::new("Connections", "connections");

    connections.plot(
        Plot::gauge("Active Connections", "connections-active", Unit::Count)
            .with_style(ChartStyle::Line)
            .build("connections_active"),
    );

    connections.plot(
        Plot::counter("Connection Failures/sec", "connections-failed", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(connections_failed[10s])"),
    );

    view.group(connections);

    // Disconnect reasons
    let mut disconnects = Group::new("Disconnect Reasons", "disconnects");

    disconnects.plot(
        Plot::counter("EOF", "disconnect-eof", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(disconnects_eof[10s])"),
    );

    disconnects.plot(
        Plot::counter("Recv Error", "disconnect-recv-error", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(disconnects_recv_error[10s])"),
    );

    disconnects.plot(
        Plot::counter("Send Error", "disconnect-send-error", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(disconnects_send_error[10s])"),
    );

    disconnects.plot(
        Plot::counter("Closed Event", "disconnect-closed-event", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(disconnects_closed_event[10s])"),
    );

    disconnects.plot(
        Plot::counter("Error Event", "disconnect-error-event", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(disconnects_error_event[10s])"),
    );

    disconnects.plot(
        Plot::counter("Connect Failed", "disconnect-connect-failed", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(disconnects_connect_failed[10s])"),
    );

    view.group(disconnects);

    view
}
