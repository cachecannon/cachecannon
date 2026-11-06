use super::*;

pub fn generate(
    data: &Arc<ParquetReader>,
    _server: Option<&Arc<ParquetReader>>,
    _client: Option<&Arc<ParquetReader>>,
    sections: Vec<Section>,
) -> View {
    let mut view = View::new(data, api::BENCHMARK_ROOT, sections);

    // Cache hit/miss rates
    let mut rates = Group::new("Cache Rates", "cache-rates");

    rates.plot(
        Plot::counter("Hit Rate", "hit-rate", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(cache_hits[10s])"),
    );

    rates.plot(
        Plot::counter("Miss Rate", "miss-rate", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(cache_misses[10s])"),
    );

    view.group(rates);

    // Operation breakdown
    let mut ops = Group::new("Operations", "operations");

    ops.plot(
        Plot::counter("GET/sec", "get-rate", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(get_count[10s])"),
    );

    ops.plot(
        Plot::counter("SET/sec", "set-rate", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(set_count[10s])"),
    );

    ops.plot(
        Plot::counter("DELETE/sec", "delete-rate", Unit::Rate)
            .with_style(ChartStyle::Line)
            .build("irate(delete_count[10s])"),
    );

    view.group(ops);

    view
}
