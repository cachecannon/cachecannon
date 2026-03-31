use metriken_query::tsdb::*;
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
    pub fn new(data: &Arc<Tsdb>, sections: Vec<Section>) -> Self {
        let interval = data.interval();
        let source = data.source().to_string();
        let version = data.version().to_string();
        let filename = data.filename().to_string();

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

    pub fn plot_promql(&mut self, opts: PlotOpts, promql_query: String) {
        self.plots.push(Plot {
            opts,
            data: Vec::new(), // Will be populated by frontend
            min_value: None,
            max_value: None,
            time_data: None,
            formatted_time_data: None,
            series_names: None,
            promql_query: Some(promql_query),
        });
    }
}

#[derive(Serialize, Clone)]
pub struct Plot {
    data: Vec<Vec<f64>>,
    opts: PlotOpts,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_data: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    formatted_time_data: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    series_names: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    promql_query: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct PlotOpts {
    title: String,
    id: String,
    style: String,
    // Unified configuration for value formatting, axis labels, etc.
    format: Option<FormatConfig>,
}

#[derive(Serialize, Clone)]
pub struct FormatConfig {
    // Axis labels
    x_axis_label: Option<String>,
    y_axis_label: Option<String>,

    // Value formatting
    unit_system: Option<String>, // e.g., "percentage", "time", "bitrate"
    precision: Option<u8>,       // Number of decimal places

    // Scale configuration
    log_scale: Option<bool>, // Whether to use log scale for y-axis
    min: Option<f64>,        // Min value for y-axis
    max: Option<f64>,        // Max value for y-axis

    // Additional customization
    value_label: Option<String>, // Label used in tooltips for the value
}

impl PlotOpts {
    // Basic constructors without formatting
    pub fn line<T: Into<String>, U: Into<String>>(title: T, id: U, unit: Unit) -> Self {
        Self {
            title: title.into(),
            id: id.into(),
            style: "line".to_string(),
            format: Some(FormatConfig::new(unit)),
        }
    }

    pub fn scatter<T: Into<String>, U: Into<String>>(title: T, id: U, unit: Unit) -> Self {
        Self {
            title: title.into(),
            id: id.into(),
            style: "scatter".to_string(),
            format: Some(FormatConfig::new(unit)),
        }
    }

    pub fn with_log_scale(mut self, log_scale: bool) -> Self {
        if let Some(ref mut format) = self.format {
            format.log_scale = Some(log_scale);
        }

        self
    }
}

impl FormatConfig {
    pub fn new(unit: Unit) -> Self {
        Self {
            x_axis_label: None,
            y_axis_label: None,
            unit_system: Some(unit.to_string()),
            precision: Some(2),
            log_scale: None,
            min: None,
            max: None,
            value_label: None,
        }
    }
}

pub enum Unit {
    Count,
    Rate,
    Time,
    Bytes,
    Datarate,
    Bitrate,
    Percentage,
}

impl std::fmt::Display for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        let s = match self {
            Self::Count => "count",
            Self::Rate => "rate",
            Self::Time => "time",
            Self::Bytes => "bytes",
            Self::Datarate => "datarate",
            Self::Bitrate => "bitrate",
            Self::Percentage => "percentage",
        };

        write!(f, "{s}")
    }
}
