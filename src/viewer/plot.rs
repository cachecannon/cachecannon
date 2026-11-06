use metriken_charts::HistogramView;
pub use metriken_charts::{ChartStyle, Plot, Unit};
use metriken_query::ParquetReader;
use serde::Serialize;
use std::sync::Arc;

#[derive(Default, Serialize)]
pub struct View {
    // interval between consecutive datapoints as fractional seconds
    interval: f64,
    source: String,
    version: String,
    filename: String,
    groups: Vec<Group>,
    sections: Vec<Section>,
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    #[serde(default)]
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
}

impl View {
    pub fn new(data: &Arc<ParquetReader>, sections: Vec<Section>) -> Self {
        let interval = data.interval();
        let source = data.source();
        let version = data.version();
        let filename = data.filename().unwrap_or_default().to_string();

        Self {
            interval,
            source,
            version,
            filename,
            groups: Vec::new(),
            sections,
            metadata: std::collections::HashMap::new(),
        }
    }

    pub fn group(&mut self, group: Group) -> &Self {
        self.groups.push(group);
        self
    }
}

#[derive(Clone, Serialize)]
pub struct Section {
    pub(crate) name: String,
    pub(crate) route: String,
}

#[derive(Serialize)]
pub struct Group {
    name: String,
    id: String,
    plots: Vec<Plot>,
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    #[serde(default)]
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
}

impl Group {
    pub fn new<T: Into<String>, U: Into<String>>(name: T, id: U) -> Self {
        Self {
            name: name.into(),
            id: id.into(),
            plots: Vec::new(),
            metadata: std::collections::HashMap::new(),
        }
    }

    pub fn plot(&mut self, plot: Plot) {
        self.plots.push(plot);
    }
}

/// A chart of `query`, a complete `histogram_percentiles(...)` call, on a log
/// axis. `Plot::histogram_latency` is not used because its 0 to 100 s range
/// draws a slower value in the out-of-bounds band.
pub fn latency_percentiles(title: &str, id: &str, query: &str) -> Plot {
    Plot::histogram(title, id, Unit::Time, HistogramView::Percentiles)
        .with_log_scale(true)
        .with_style(ChartStyle::Scatter)
        .build(query)
}
