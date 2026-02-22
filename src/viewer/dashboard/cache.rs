use super::*;

pub fn generate(
    data: &Arc<Tsdb>,
    _server: Option<&Arc<Tsdb>>,
    _client: Option<&Arc<Tsdb>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, sections);

    // Cache hit/miss rates
    let mut rates = Group::new("Cache Rates", "cache-rates");

    rates.plot_promql(
        PlotOpts::line("Hit Rate", "hit-rate", Unit::Rate),
        "irate(cache_hits[10s])".to_string(),
    );

    rates.plot_promql(
        PlotOpts::line("Miss Rate", "miss-rate", Unit::Rate),
        "irate(cache_misses[10s])".to_string(),
    );

    view.group(rates);

    // Operation breakdown
    let mut ops = Group::new("Operations", "operations");

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
