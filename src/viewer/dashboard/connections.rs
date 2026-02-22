use super::*;

pub fn generate(
    data: &Arc<Tsdb>,
    _server: Option<&Arc<Tsdb>>,
    _client: Option<&Arc<Tsdb>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, sections);

    // Connection metrics
    let mut connections = Group::new("Connections", "connections");

    connections.plot_promql(
        PlotOpts::line("Active Connections", "connections-active", Unit::Count),
        "connections_active".to_string(),
    );

    connections.plot_promql(
        PlotOpts::line("Connection Failures/sec", "connections-failed", Unit::Rate),
        "irate(connections_failed[10s])".to_string(),
    );

    view.group(connections);

    // Disconnect reasons
    let mut disconnects = Group::new("Disconnect Reasons", "disconnects");

    disconnects.plot_promql(
        PlotOpts::line("EOF", "disconnect-eof", Unit::Rate),
        "irate(disconnects_eof[10s])".to_string(),
    );

    disconnects.plot_promql(
        PlotOpts::line("Recv Error", "disconnect-recv-error", Unit::Rate),
        "irate(disconnects_recv_error[10s])".to_string(),
    );

    disconnects.plot_promql(
        PlotOpts::line("Send Error", "disconnect-send-error", Unit::Rate),
        "irate(disconnects_send_error[10s])".to_string(),
    );

    disconnects.plot_promql(
        PlotOpts::line("Closed Event", "disconnect-closed-event", Unit::Rate),
        "irate(disconnects_closed_event[10s])".to_string(),
    );

    disconnects.plot_promql(
        PlotOpts::line("Error Event", "disconnect-error-event", Unit::Rate),
        "irate(disconnects_error_event[10s])".to_string(),
    );

    disconnects.plot_promql(
        PlotOpts::line("Connect Failed", "disconnect-connect-failed", Unit::Rate),
        "irate(disconnects_connect_failed[10s])".to_string(),
    );

    view.group(disconnects);

    view
}
