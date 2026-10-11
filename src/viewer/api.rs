//! The PromQL HTTP API the frontend reads (`/api/v1/...`).
//!
//! metriken-query 0.9 served these routes itself (`promql::routes`, feature
//! `http`); 0.36 has no HTTP layer, so they live here. Paths, parameters and
//! response bodies are 0.9's, so `assets/lib/script.js` is unchanged.

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
    respond(source.query_range(&params.query, params.start, params.end, params.step))
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

/// The recording's time range in seconds. With no data, the last hour, as
/// 0.9's `get_time_range` gave.
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
}
