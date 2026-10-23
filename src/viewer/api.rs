//! The PromQL HTTP API the frontend reads (`/api/v1/...`).
//!
//! metriken-query 0.9 served these routes itself (`promql::routes`, feature
//! `http`); 0.36 has no HTTP layer, so they live here. Paths, parameters and
//! response bodies are 0.9's, except that `/api/v1/metadata` also reports the
//! snapshot interval, and a range query that is a single
//! `histogram_percentiles` or `histogram_quantiles` call with a step of 1.5
//! snapshot intervals or more gets a stride derived from the step
//! (`with_histogram_stride`), which changes its results.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Json;
use axum::routing::get;
use metriken_query::{ParquetReader, QueryError, QueryResult};
use serde::{Deserialize, Serialize};

type Source = Arc<ParquetReader>;

#[derive(Debug, Deserialize)]
struct QueryParams {
    query: String,
    time: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RangeQueryParams {
    query: String,
    start: f64,
    end: f64,
    step: f64,
}

#[derive(Debug, Serialize)]
struct ApiResponse<T> {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "errorType")]
    error_type: Option<String>,
}

impl<T> ApiResponse<T> {
    fn success(data: T) -> Self {
        Self {
            status: "success".to_string(),
            data: Some(data),
            error: None,
            error_type: None,
        }
    }

    fn error(error: String, error_type: &str) -> Self {
        Self {
            status: "error".to_string(),
            data: None,
            error: Some(error),
            error_type: Some(error_type.to_string()),
        }
    }
}

/// A query's outcome as a response body. An error is answered with 200 and
/// `status: "error"`, as 0.9 did.
fn respond(result: Result<QueryResult, QueryError>) -> Json<ApiResponse<QueryResult>> {
    match result {
        Ok(result) => Json(ApiResponse::success(result)),
        Err(e) => {
            let error_type = match e {
                QueryError::ParseError(_) => "bad_data",
                QueryError::EvaluationError(_) => "execution",
                QueryError::Unsupported(_) => "unsupported",
                QueryError::MetricNotFound(_) => "not_found",
            };
            Json(ApiResponse::error(e.to_string(), error_type))
        }
    }
}

pub fn routes(source: Source) -> Router {
    Router::new()
        .route("/api/v1/query", get(instant_query))
        .route("/api/v1/query_range", get(range_query))
        .route("/api/v1/labels", get(label_names))
        .route("/api/v1/label/{name}/values", get(label_values))
        .route("/api/v1/metadata", get(metadata))
        .with_state(source)
}

async fn instant_query(
    Query(params): Query<QueryParams>,
    State(source): State<Source>,
) -> Json<ApiResponse<QueryResult>> {
    respond(source.query(&params.query, params.time))
}

async fn range_query(
    Query(params): Query<RangeQueryParams>,
    State(source): State<Source>,
) -> Json<ApiResponse<QueryResult>> {
    let query = with_histogram_stride(&params.query, params.step, source.interval());
    respond(source.query_range(&query, params.start, params.end, params.step))
}

/// metriken-query ignores `step` for `histogram_percentiles` and
/// `histogram_quantiles` and returns one point per snapshot that recorded
/// latency, so a chart of a four-hour run at 1 s snapshots gets about 14,000
/// points per quantile. Their optional third argument is a stride in seconds:
/// a window closes at the first snapshot at least a stride after the previous
/// window closed, and its quantile is taken over the histograms merged across
/// it.
/// Every snapshot is counted in some point, but a latency excursion confined
/// to one snapshot is diluted by the others in its window: on a four-hour
/// recording at 1 s, where the step rounds to four snapshots per window, a
/// 9.0 s per-snapshot p99 reads 55 ms at p99 and 9.0 s at p99.9.
///
/// The stride is `step` rounded to a whole number k of snapshot `interval`s,
/// less half an interval. Rounding is needed because a stride slightly over k
/// intervals closes each window on snapshot k + 1: a stride of 1.01 s over
/// snapshots 1.0015 s apart gives two-snapshot windows and half the points.
/// The half interval is subtracted so a window still closes on snapshot k
/// when its k intervals sum to slightly less than k intervals. A window
/// holds fewer snapshots when one of its first k − 1 is missing or its
/// (k − 1)th is half an interval or more late, and k + 1 when its kth is
/// more than half an interval early, both relative to the snapshot that
/// closed the previous window.
///
/// The query is returned unchanged when k is 1 or less (a step under 1.5
/// intervals), when it already has a stride, and when it is not a single call
/// of either function. A bracket inside a label value can also leave it
/// unchanged.
fn with_histogram_stride(query: &str, step: f64, interval: f64) -> String {
    let trimmed = query.trim();
    let Some(open) = ["histogram_percentiles(", "histogram_quantiles("]
        .iter()
        .find(|prefix| trimmed.starts_with(*prefix))
        .map(|prefix| prefix.len() - 1)
    else {
        return query.to_string();
    };
    if step.is_nan() || step <= 0.0 {
        return query.to_string();
    }
    // A recording at under 1 ms reports an interval of 0; use the step as is.
    let stride = if interval > 0.0 {
        let k = (step / interval).round();
        if k <= 1.0 {
            return query.to_string();
        }
        (k - 0.5) * interval
    } else {
        step
    };

    // The call's opening parenthesis must close at the last character, and
    // its argument list must hold exactly two top-level arguments.
    let mut depth = 0i32;
    let mut commas = 0;
    for (i, ch) in trimmed.char_indices().skip(open) {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 && i != trimmed.len() - 1 {
                    return query.to_string();
                }
            }
            ',' if depth == 1 => commas += 1,
            _ => {}
        }
    }
    if depth != 0 || commas != 1 {
        return query.to_string();
    }

    format!("{}, {stride})", &trimmed[..trimmed.len() - 1])
}

/// The fixed list 0.9 returned; the frontend does not call this route.
async fn label_names() -> Json<ApiResponse<Vec<String>>> {
    let labels = [
        "__name__",
        "direction",
        "op",
        "state",
        "reason",
        "id",
        "name",
        "sampler",
    ];
    Json(ApiResponse::success(
        labels.iter().map(|l| l.to_string()).collect(),
    ))
}

/// The fixed lists 0.9 returned; the frontend does not call this route.
async fn label_values(Path(name): Path<String>) -> Json<ApiResponse<Vec<String>>> {
    let values: &[&str] = match name.as_str() {
        "direction" => &["transmit", "receive", "to", "from"],
        "op" => &["read", "write"],
        "state" => &["user", "system"],
        _ => &[],
    };
    Json(ApiResponse::success(
        values.iter().map(|v| v.to_string()).collect(),
    ))
}

/// The recording's time range and snapshot interval in seconds. With no
/// data, the range is the last hour, as 0.9's `get_time_range` gave.
/// `interval` is the declared sampling interval (`sampling_interval_ms` in
/// the parquet footer, in whole milliseconds), which 0.9 did not report;
/// `script.js` uses it as the smallest step, or 1 s when it is 0.
async fn metadata(State(source): State<Source>) -> Json<ApiResponse<serde_json::Value>> {
    let (min, max) = source.time_range().unwrap_or_else(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        (now - 3600.0, now)
    });
    Json(ApiResponse::success(serde_json::json!({
        "minTime": min,
        "maxTime": max,
        "interval": source.interval(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_the_0_9_error_types() {
        let body = respond(Err(QueryError::MetricNotFound("x".into())));
        let json = serde_json::to_value(&body.0).unwrap();
        assert_eq!(json["status"], "error");
        assert_eq!(json["errorType"], "not_found");
        assert!(json.get("data").is_none());
    }

    #[test]
    fn histogram_quantiles_get_a_stride_from_the_step() {
        assert_eq!(
            with_histogram_stride(
                "histogram_percentiles([0.5, 0.99], response_latency)",
                14.0,
                1.0
            ),
            "histogram_percentiles([0.5, 0.99], response_latency, 13.5)"
        );
        assert_eq!(
            with_histogram_stride(
                "histogram_quantiles([0.5], x{op=\"get\", a=\"b\"})",
                2.5,
                1.0
            ),
            "histogram_quantiles([0.5], x{op=\"get\", a=\"b\"}, 2.5)"
        );
    }

    /// A step that rounds to one snapshot interval or less adds no stride, nor
    /// does a NaN or zero step. This checks the query text only.
    #[test]
    fn steps_rounding_to_one_interval_add_no_stride() {
        let query = "histogram_percentiles([0.5], response_latency)";
        for (step, interval) in [
            (0.1, 0.1),
            (1.0, 1.0),
            (1.4, 1.0),
            (0.5, 1.0),
            (f64::NAN, 1.0),
            (0.0, 0.0),
        ] {
            assert_eq!(with_histogram_stride(query, step, interval), query);
        }
    }

    /// The stride is a whole number of snapshot intervals less half of one.
    /// With an interval of 0, the stride is the step. This checks the
    /// formatted query only; the windowing is metriken-query's.
    #[test]
    fn stride_is_whole_snapshots_less_half_an_interval() {
        let query = "histogram_percentiles([0.5], response_latency)";
        for (step, interval, stride) in [
            (1.6, 1.0, "1.5"),
            (3.6, 1.0, "3.5"),
            (1.0, 0.25, "0.875"),
            (2.0, 0.0, "2"),
        ] {
            assert_eq!(
                with_histogram_stride(query, step, interval),
                format!("histogram_percentiles([0.5], response_latency, {stride})")
            );
        }
    }

    #[test]
    fn other_queries_keep_their_text() {
        for query in [
            "histogram_percentiles([0.5], response_latency, 30)",
            "histogram_percentiles([0.5], a) / histogram_percentiles([0.5], b)",
            "histogram_percentiles([0.5], a) * 1000",
            "irate(responses_received[10s])",
        ] {
            assert_eq!(with_histogram_stride(query, 14.0, 1.0), query);
        }
    }
}
