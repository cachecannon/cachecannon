use super::AppState;
use super::plot::*;
use metriken_query::tsdb::*;
use std::sync::Arc;

mod cache;
mod client;
mod connections;
mod latency;
mod overview;
mod server;
mod throughput;

type Generator = fn(&Arc<Tsdb>, Option<&Arc<Tsdb>>, Option<&Arc<Tsdb>>, Vec<Section>) -> View;

/// Generate dashboards based on available data sources
pub fn generate(
    benchmark: Arc<Tsdb>,
    server: Option<Arc<Tsdb>>,
    client: Option<Arc<Tsdb>>,
) -> AppState {
    let mut state = AppState::new(benchmark.clone());

    // Build section metadata based on available data
    let mut section_meta: Vec<(&str, &str, Generator)> = vec![
        ("Overview", "/overview", overview::generate),
        ("Throughput", "/throughput", throughput::generate),
        ("Latency", "/latency", latency::generate),
        ("Cache", "/cache", cache::generate),
        ("Connections", "/connections", connections::generate),
    ];

    // Add server section if server rezolus data is available
    if server.is_some() {
        section_meta.push(("Server Metrics", "/server", server::generate));
    }

    // Add client section if client rezolus data is available
    if client.is_some() {
        section_meta.push(("Client Metrics", "/client", client::generate));
    }

    let sections: Vec<Section> = section_meta
        .iter()
        .map(|(name, route, _)| Section {
            name: (*name).to_string(),
            route: (*route).to_string(),
        })
        .collect();

    for (_, route, generator) in &section_meta {
        let key = format!("{}.json", &route[1..]);
        let view = generator(
            &benchmark,
            server.as_ref(),
            client.as_ref(),
            sections.clone(),
        );
        state
            .sections
            .insert(key, serde_json::to_string(&view).unwrap());
    }

    state
}
