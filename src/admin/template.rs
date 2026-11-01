//! cachecannon's KPI template: the `service_queries` metadata value that the
//! Rezolus viewer renders as cachecannon's KPI section.
//!
//! The template travels in the recording. `/metrics/stream` sends it in its
//! handshake metadata, which a stream recorder (`rezolus record` to
//! `.dendro`) copies into the source's metadata, and the parquet recorder
//! (`[admin] parquet`) writes it into the file's metadata. The JSON is a
//! Rezolus `ServiceExtension` (`crates/dashboard/src/service_extension.rs`
//! in rezolus): a `service_name` and a list of KPIs, each with a `role`,
//! `title`, `query`, `type` and optional `unit_system`.
//!
//! This file is the source of truth for cachecannon's template. Rezolus keeps
//! a copy in its built-in registry (`crates/dashboard/templates/
//! cachecannon.json`) only as the fallback for recordings that carry no
//! template, such as those made before this one was sent, so the two do not
//! have to match.

/// The key the template is stored under, in both the handshake and the
/// parquet metadata.
pub const KEY_SERVICE_QUERIES: &str = "service_queries";

/// The template, verbatim from `templates/cachecannon.json`.
pub const SERVICE_QUERIES: &str = include_str!("../../templates/cachecannon.json");

#[cfg(test)]
mod tests {
    use super::*;

    /// The unit systems the Rezolus viewer can scale: rezolus's
    /// `dashboard::plot::Unit` (`crates/dashboard/src/plot.rs`). The viewer
    /// warns about any other value and renders that KPI with `count`
    /// scaling; `rezolus recording annotate` and rezolus's registry refuse
    /// it.
    const UNIT_SYSTEMS: [&str; 9] = [
        "time",
        "bytes",
        "bitrate",
        "datarate",
        "percentage",
        "frequency",
        "power",
        "count",
        "rate",
    ];

    /// The KPI types the viewer charts by name
    /// (`crates/dashboard/src/dashboard/service.rs` in rezolus): `gauge` and
    /// `histogram` have their own plots, and anything else is charted as a
    /// counter, so a type outside this list is a typo the viewer does not
    /// report.
    const KPI_TYPES: [&str; 4] = ["gauge", "histogram", "delta_counter", "counter"];

    fn template() -> serde_json::Value {
        serde_json::from_str(SERVICE_QUERIES).expect("the template is JSON")
    }

    /// The fields rezolus's `ServiceExtension` requires, with the types it
    /// reads them as.
    #[test]
    fn template_has_the_service_extension_shape() {
        let doc = template();
        assert_eq!(doc["service_name"], "cachecannon");
        let kpis = doc["kpis"].as_array().expect("kpis is an array");
        assert!(!kpis.is_empty());
        for kpi in kpis {
            for key in ["role", "title", "query", "type"] {
                assert!(kpi[key].is_string(), "KPI {kpi} has no string {key}");
            }
            let kind = kpi["type"].as_str().unwrap();
            assert!(
                KPI_TYPES.contains(&kind),
                "KPI {} has type {kind:?}, which the viewer charts as a counter",
                kpi["title"]
            );
            // rezolus takes any role: it groups the KPIs, names the group
            // (capitalized) and is part of each chart's id
            // (`kpi-<role>-<title>`), so it is held to a lowercase word.
            let role = kpi["role"].as_str().unwrap();
            assert!(
                !role.is_empty() && role.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "KPI {} has role {role:?}, which is not a lowercase word",
                kpi["title"]
            );
            if let Some(unit) = kpi.get("unit_system") {
                let unit = unit.as_str().expect("unit_system is a string");
                assert!(
                    UNIT_SYSTEMS.contains(&unit),
                    "KPI {} has unit_system {unit:?}, which the viewer does not know",
                    kpi["title"]
                );
            }
        }
    }

    /// Every metric a KPI query names, with or without a selector, is one
    /// this binary registers, so a renamed metric fails here rather than
    /// leaving an empty chart.
    #[test]
    fn template_queries_name_registered_metrics() {
        let registered: std::collections::HashSet<String> = metriken::metrics()
            .iter()
            .map(|m| m.name().to_string())
            .collect();
        let doc = template();
        let mut checked = 0;
        for kpi in doc["kpis"].as_array().unwrap() {
            let query = kpi["query"].as_str().unwrap();
            for name in selected_metrics(query) {
                assert!(
                    registered.contains(name),
                    "KPI {} queries {name}, which cachecannon does not register",
                    kpi["title"]
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no metric names found in the queries");
    }

    /// No metric a query names selects by `source`. Rezolus evaluates a
    /// recording's template against that recording's series only, so the
    /// matcher is not needed to keep another source's series out, and it
    /// would match nothing in a recording whose endpoint was given another
    /// `source` name.
    #[test]
    fn no_metric_selects_a_source() {
        let doc = template();
        for kpi in doc["kpis"].as_array().unwrap() {
            let query = kpi["query"].as_str().unwrap();
            for (name, matchers) in metric_references(query) {
                assert!(
                    !matchers.is_some_and(|m| {
                        m.split(',').any(|m| {
                            m.split(['=', '!'])
                                .next()
                                .is_some_and(|k| k.trim().trim_matches('"') == "source")
                        })
                    }),
                    "KPI {}: {name} selects by source",
                    kpi["title"]
                );
            }
        }
    }

    /// PromQL keywords whose `(…)` that follows is a list of label names,
    /// not an expression: `sum by (op) (…)`, `a / on (op) b`.
    const LABEL_LIST_KEYWORDS: [&str; 6] = [
        "by",
        "without",
        "on",
        "ignoring",
        "group_left",
        "group_right",
    ];

    /// PromQL keywords that are followed by an expression or a duration:
    /// `x > bool (y)` compares against `y`, which is checked.
    const OTHER_KEYWORDS: [&str; 2] = ["bool", "offset"];

    /// PromQL aggregations, which may be followed by `by`/`without` rather
    /// than `(`: `sum by (op) (…)`.
    const AGGREGATIONS: [&str; 12] = [
        "sum",
        "min",
        "max",
        "avg",
        "group",
        "stddev",
        "stdvar",
        "count",
        "count_values",
        "bottomk",
        "topk",
        "quantile",
    ];

    /// The metric names `query` refers to, each with its matchers (the text
    /// inside its `{…}`) or `None` for a bare name: every identifier outside
    /// `{…}` and `[…]` that is not a function call (followed by `(`), a
    /// keyword, an aggregation, a label list after `by` and its kin, or a
    /// number; and the `__name__` of a selector with no name before it,
    /// `{__name__="x",…}`.
    fn metric_references(query: &str) -> Vec<(&str, Option<&str>)> {
        let bytes = query.as_bytes();
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b':';
        let mut out = Vec::new();
        // Where the last identifier ended: a `{` there is that name's
        // selector, and one anywhere else names its metric in `__name__`.
        let mut ident_end = None;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'{' if ident_end != Some(i) => {
                    let end = query[i..].find('}').map_or(query.len(), |e| i + e);
                    let matchers = &query[i + 1..end];
                    let name = matchers.split(',').find_map(|m| {
                        let (key, value) = m.trim().split_once('=')?;
                        (key.trim() == "__name__").then(|| value.trim().trim_matches('"'))
                    });
                    out.push((name.unwrap_or(""), Some(matchers)));
                    i = end + 1;
                }
                b'{' | b'[' | b'"' => {
                    let close = match bytes[i] {
                        b'{' => '}',
                        b'[' => ']',
                        _ => '"',
                    };
                    i += 1 + query[i + 1..].find(close).unwrap_or(query.len() - i - 1) + 1;
                }
                b if ident(b) => {
                    let start = i;
                    while i < bytes.len() && ident(bytes[i]) {
                        i += 1;
                    }
                    ident_end = Some(i);
                    let word = &query[start..i];
                    let next = query[i..].trim_start();
                    if OTHER_KEYWORDS.contains(&word) {
                        continue;
                    }
                    if LABEL_LIST_KEYWORDS.contains(&word) {
                        // `by (op)`: the list holds label names; skip it.
                        if next.starts_with('(') {
                            let open = query.len() - next.len();
                            i = open
                                + query[open..]
                                    .find(')')
                                    .map_or(query.len() - open, |e| e + 1);
                        }
                        continue;
                    }
                    if bytes[start].is_ascii_digit()
                        || next.starts_with('(')
                        || AGGREGATIONS.contains(&word)
                    {
                        continue;
                    }
                    let matchers = next.strip_prefix('{').map(|rest| {
                        let end = rest.find('}').unwrap_or(rest.len());
                        &rest[..end]
                    });
                    out.push((word, matchers));
                }
                _ => i += 1,
            }
        }
        out
    }

    /// The metric names in `query`.
    fn selected_metrics(query: &str) -> Vec<&str> {
        metric_references(query)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn metric_references_find_every_name() {
        assert_eq!(
            metric_references(
                "sum by (op) (irate(requests_sent{source=\"x\"}[5s])) / get_count \
                 > 0.5 * histogram_quantile(0.99, set_latency{})"
            ),
            [
                ("requests_sent", Some("source=\"x\"")),
                ("get_count", None),
                ("set_latency", Some("")),
            ]
        );
        // `bool` is followed by an expression, which is checked; a selector
        // with no name names its metric in `__name__`.
        assert_eq!(
            metric_references("x{} > bool (bare) + {__name__=\"y\", source=\"s\"}"),
            [
                ("x", Some("")),
                ("bare", None),
                ("y", Some("__name__=\"y\", source=\"s\"")),
            ]
        );
    }
}
