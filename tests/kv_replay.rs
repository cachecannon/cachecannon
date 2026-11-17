//! KV-cache replay (`[workload.kv_replay]`) against an in-process RESP
//! store, one trace per test (`common::kv`).
//!
//! The runner can be started once per process and its metrics are
//! process-global, so every test runs its body in a child process of this
//! binary (`common::kv::isolated`): one test binary instead of one per test,
//! because each is a separate fat-LTO link in the release test job.

mod common;

use cachecannon::metrics;
use cachecannon::{ColorMode, OutputFormat, create_formatter, run_benchmark_full};
use common::kv::{
    CHUNK_TOKENS, Hooks, RUN_BOUND, Server, key, load_config, replay, replay_file, replay_on,
    replay_with, request, temp_path, write_trace, write_unordered_trace,
};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// A Hugging Face config with the given `layer_types`.
fn model_config(layer_types: &str) -> String {
    format!(
        r#"{{
            "num_hidden_layers": 4,
            "layer_types": {layer_types},
            "hidden_size": 512,
            "num_attention_heads": 8,
            "num_key_value_heads": 2,
            "head_dim": 64,
            "linear_num_value_heads": 4,
            "linear_num_key_heads": 2,
            "linear_key_head_dim": 32,
            "linear_value_head_dim": 32,
            "torch_dtype": "bfloat16"
        }}"#
    )
}

/// KV replay against an empty server: nothing is cached until a prompt
/// repeats, and a repeat that hits every chunk of a prompt made of whole
/// chunks reports all tokens but the last as cached.
#[test]
fn kv_replay_cold_cache_then_full_prompt_repeat() {
    common::kv::isolated("kv_replay_cold_cache_then_full_prompt_repeat", || {
        let ct = CHUNK_TOKENS;
        let server = Server::default();
        let rows = replay(
            &server,
            &[
                request(1, 0, &[10, 11, 12, 13], 4 * ct),
                request(2, 1, &[20, 21, 22], 3 * ct + 5),
                request(3, 2, &[10, 11, 12, 13], 4 * ct),
            ],
            "",
        );
        assert_eq!(rows.len(), 3, "{rows:?}");

        // Closed loop has no schedule to lag.
        assert!(rows.iter().all(|r| r.lag_ns.is_none()), "{rows:?}");
        for r in &rows[..2] {
            assert_eq!(r.cached_tokens, 0, "{r:?}");
            assert_eq!(r.new_tokens, r.input_tokens, "{r:?}");
            assert_eq!(
                (r.lookup_hit_chunks, r.gets, r.sets),
                (0, 0, r.chunks),
                "{r:?}"
            );
        }
        let r = &rows[2];
        assert_eq!(r.req_id, 3);
        assert_eq!((r.hit_chunks, r.gets, r.sets), (4, 4, 0), "{r:?}");
        assert_eq!(r.cached_tokens, u64::from(4 * ct - 1), "{r:?}");
        assert_eq!(r.new_tokens, 1, "{r:?}");

        assert_eq!(metrics::KV_REQUESTS.value(), 3);
        assert_eq!(metrics::KV_CACHED_TOKENS.value(), u64::from(4 * ct - 1));
        assert_eq!(metrics::KV_FULL_HITS.value(), 1);
        assert_eq!(metrics::KV_CHUNK0_MISSES.value(), 2);
        assert_eq!(metrics::KV_EXISTS_COUNT.value(), 4 + 3 + 4);

        // Every chunk of both distinct prompts was written once, at 4 bytes per
        // token.
        let want: Vec<Vec<u8>> = [10, 11, 12, 13, 20, 21, 22].map(|c| key(c, 0)).to_vec();
        assert_eq!(server.sets(), want);
        assert_eq!(server.value_len(&key(10, 0)), Some(4 * ct as usize));
    });
}

/// KV replay of two prompts that share their first two chunks: the second
/// finds those two cached and writes only the chunks after them.
#[test]
fn kv_replay_shared_prefix_hits_and_writes_from_the_divergence() {
    common::kv::isolated(
        "kv_replay_shared_prefix_hits_and_writes_from_the_divergence",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::default();
            let rows = replay(
                &server,
                &[
                    request(1, 0, &[1, 2, 3, 4], 4 * ct + 3),
                    request(2, 1, &[1, 2, 7, 8], 4 * ct + 3),
                ],
                "",
            );
            let b = &rows[1];
            assert_eq!(b.req_id, 2);
            assert_eq!((b.lookup_hit_chunks, b.hit_chunks), (2, 2), "{b:?}");
            assert_eq!(b.cached_tokens, u64::from(2 * ct), "{b:?}");
            assert_eq!(b.new_tokens, u64::from(2 * ct + 3), "{b:?}");
            assert_eq!((b.gets, b.sets), (2, 2), "{b:?}");

            // B read chunks 1 and 2 and wrote only 7 and 8.
            assert_eq!(server.ops("GET"), vec![key(1, 0), key(2, 0)]);
            assert_eq!(server.sets()[4..], [key(7, 0), key(8, 0)]);
        },
    );
}

/// KV replay with an object deleted from the middle of a cached prefix, under
/// the default write rule: the prefix stops at the gap, and every chunk from
/// the gap on is written again although the chunks after it are present.
#[test]
fn kv_replay_gap_ends_the_prefix_and_set_from_miss_rewrites_after_it() {
    common::kv::isolated(
        "kv_replay_gap_ends_the_prefix_and_set_from_miss_rewrites_after_it",
        || {
            let ct = CHUNK_TOKENS;
            // After the first request's 4 SETs, delete its second chunk.
            let server = Server::with_hooks(Hooks {
                delete_after_sets: Some((4, vec![key(2, 0)])),
                ..Default::default()
            });
            let a = request(1, 0, &[1, 2, 3, 4], 4 * ct + 3);
            let again = request(2, 1, &[1, 2, 3, 4], 4 * ct + 3);
            let rows = replay(&server, &[a, again], "");
            let r = &rows[1];
            assert_eq!((r.lookup_hit_chunks, r.hit_chunks), (1, 1), "{r:?}");
            assert_eq!(r.cached_tokens, u64::from(ct), "{r:?}");
            assert_eq!((r.gets, r.sets), (1, 3), "{r:?}");
            assert_eq!(server.sets()[4..], [key(2, 0), key(3, 0), key(4, 0)]);
        },
    );
}

/// KV replay with an object deleted from the middle of a cached prefix, under
/// insert-if-absent: the prefix stops at the gap, and only the missing object
/// is written.
#[test]
fn kv_replay_gap_ends_the_prefix_and_insert_if_absent_fills_only_the_gap() {
    common::kv::isolated(
        "kv_replay_gap_ends_the_prefix_and_insert_if_absent_fills_only_the_gap",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                delete_after_sets: Some((4, vec![key(2, 0)])),
                ..Default::default()
            });
            let a = request(1, 0, &[1, 2, 3, 4], 4 * ct + 3);
            let again = request(2, 1, &[1, 2, 3, 4], 4 * ct + 3);
            let rows = replay(&server, &[a, again], r#"write_rule = "insert-if-absent""#);
            let r = &rows[1];
            assert_eq!((r.lookup_hit_chunks, r.hit_chunks), (1, 1), "{r:?}");
            assert_eq!(r.cached_tokens, u64::from(ct), "{r:?}");
            assert_eq!((r.gets, r.sets), (1, 1), "{r:?}");
            assert_eq!(server.sets()[4..], [key(2, 0)]);
        },
    );
}

/// KV replay of an MLA model: one object per chunk at the full latent size,
/// keyed as rank 0.
#[test]
fn kv_replay_mla_reads_and_writes_rank_0_only() {
    common::kv::isolated("kv_replay_mla_reads_and_writes_rank_0_only", || {
        let ct = CHUNK_TOKENS;
        let server = Server::default();
        let a = request(1, 0, &[1, 2, 3], 3 * ct + 3);
        let again = request(2, 1, &[1, 2, 3], 3 * ct + 3);
        let rows = replay(&server, &[a, again], "kv_bytes_per_token = 4\nmla = true");
        assert_eq!(rows[1].cached_tokens, u64::from(3 * ct), "{rows:?}");

        let log = server.log();
        let kv_ops: Vec<_> = log.iter().filter(|o| o.cmd != "PING").collect();
        assert_eq!(kv_ops.len(), 3 + 3 + 3 + 3, "{kv_ops:?}");
        for op in kv_ops {
            let id = u64::from_str_radix(std::str::from_utf8(&op.key[3..]).unwrap(), 16).unwrap();
            assert_eq!(kvtrace::obj_id_fields(id).2, 0, "{op:?} is not rank 0");
        }
        assert_eq!(server.sets(), vec![key(1, 0), key(2, 0), key(3, 0)]);
        // The object holds the whole latent: 4 bytes per token.
        assert_eq!(server.value_len(&key(1, 0)), Some(4 * ct as usize));
    });
}

/// KV replay rejects a hybrid model (linear-attention layers with a
/// recurrent state) before the run starts.
#[test]
fn kv_replay_rejects_a_hybrid_model() {
    common::kv::isolated("kv_replay_rejects_a_hybrid_model", || {
        // Control: the same config with every layer full attention is accepted.
        let dense = temp_path("dense.json");
        std::fs::write(
            &dense,
            model_config(
                r#"["full_attention", "full_attention", "full_attention", "full_attention"]"#,
            ),
        )
        .unwrap();
        let cfg = cachecannon::config::KvReplay {
            trace: "unused".into(),
            speedup: 0.0,
            model_config: Some(dense.clone()),
            kv_dtype: None,
            kv_bytes_per_token: None,
            mla: false,
            tp: 1,
            write_rule: Default::default(),
            key_prefix: "kv:".into(),
            key_format: Default::default(),
            requests_out: None,
            client: Default::default(),
            store_connections: 0,
            load_connections: 0,
            store_queue: None,
        };
        cachecannon::kvreplay::geometry(&cfg).unwrap();
        std::fs::remove_file(&dense).unwrap();

        let hybrid = temp_path("hybrid.json");
        std::fs::write(
            &hybrid,
            model_config(
                r#"["linear_attention", "full_attention", "linear_attention", "full_attention"]"#,
            ),
        )
        .unwrap();
        let config = load_config(&format!(
            r#"
            [target]
            endpoints = ["127.0.0.1:1"]

            [workload.kv_replay]
            trace = "{}"
            model_config = "{}"

            [admin]
            format = "quiet"
            "#,
            temp_path("no-trace.parquet").display(),
            hybrid.display(),
        ));
        let err = run_benchmark_full(
            config,
            None,
            create_formatter(OutputFormat::Quiet, ColorMode::Never),
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap_err()
        .to_string();
        std::fs::remove_file(&hybrid).unwrap();
        assert!(err.contains("Hybrid models are not supported"), "{err}");
    });
}

/// KV replay when an object is evicted between its EXISTS and its GET: the
/// nil GET is counted, and the cached prefix ends at its chunk. The store
/// still starts where the lookup's prefix ended, as LMCache's does, so the
/// evicted object is not written again.
#[test]
fn kv_replay_get_nil_ends_the_cached_prefix() {
    common::kv::isolated("kv_replay_get_nil_ends_the_cached_prefix", || {
        let ct = CHUNK_TOKENS;
        // The first request finds nothing, so the hook fires on the repeat's
        // EXISTS of the second chunk.
        let server = Server::with_hooks(Hooks {
            evict_after_exists: Some(key(2, 0)),
            ..Default::default()
        });
        let a = request(1, 0, &[1, 2, 3], 3 * ct + 3);
        let again = request(2, 1, &[1, 2, 3], 3 * ct + 3);
        let rows = replay(&server, &[a, again], "");
        let r = &rows[1];
        assert_eq!((r.lookup_hit_chunks, r.hit_chunks), (3, 1), "{r:?}");
        assert_eq!((r.gets, r.get_nil, r.sets), (3, 1, 0), "{r:?}");
        assert_eq!(r.cached_tokens, u64::from(ct), "{r:?}");
        assert_eq!(metrics::KV_GET_NIL.value(), 1);
        assert_eq!(server.value_len(&key(2, 0)), None);
    });
}

/// KV replay in open loop: each trace request starts at its timestamp
/// divided by `speedup`, whether or not the previous one has finished, and
/// its schedule lag is recorded.
#[test]
fn kv_replay_open_loop_follows_the_trace_clock() {
    common::kv::isolated("kv_replay_open_loop_follows_the_trace_clock", || {
        let ct = CHUNK_TOKENS;
        // The first request's SET is answered after 300 ms, so it is still in
        // flight when the second request is due.
        let server = Server::with_hooks(Hooks {
            delay_set: Some((key(1, 0), Duration::from_millis(300))),
            ..Default::default()
        });
        // Trace times 0, 600 and 1200 ms at speedup 3: due at 0, 200 and 400 ms.
        let rows = replay_on(
            &server,
            &[
                request(1, 0, &[1], ct),
                request(2, 600, &[2], ct),
                request(3, 1200, &[3], ct),
            ],
            "speedup = 3",
            2,
        );
        assert_eq!(rows.len(), 3, "{rows:?}");
        for r in &rows {
            let lag = r.lag_ns.expect("open loop records a schedule lag");
            assert!(
                lag < 50_000_000,
                "request {} started {lag} ns late",
                r.req_id
            );
        }

        // The server saw each request's lookup about 200 ms after the previous
        // one: neither back to back nor after the slow SET.
        let log = server.log();
        let lookup_at = |chunk: u64| {
            log.iter()
                .find(|o| o.cmd == "EXISTS" && o.key == key(chunk, 0))
                .unwrap()
                .at
        };
        for (a, b) in [(1, 2), (2, 3)] {
            let gap = lookup_at(b).duration_since(lookup_at(a));
            assert!(
                gap >= Duration::from_millis(180) && gap < Duration::from_millis(260),
                "lookups of requests {a} and {b} were {gap:?} apart"
            );
        }
        assert_eq!(cachecannon::metrics::KV_REQUESTS.value(), 3);
    });
}

/// KV replay of a trace row whose `input_tokens` is 0 although it has full
/// chunks (kvtrace keeps the source's prompt length, which can disagree with
/// the chunks): the repeat finds both chunks, counts 0 cached and 0 new
/// tokens, and the run finishes.
#[test]
fn kv_replay_zero_input_tokens_with_chunks() {
    common::kv::isolated("kv_replay_zero_input_tokens_with_chunks", || {
        let server = Server::default();
        let rows = replay(
            &server,
            &[request(1, 0, &[1, 2], 0), request(2, 1, &[1, 2], 0)],
            "",
        );
        assert_eq!(rows.len(), 2, "{rows:?}");
        let r = &rows[1];
        assert_eq!((r.hit_chunks, r.gets), (2, 2), "{r:?}");
        assert_eq!((r.cached_tokens, r.new_tokens), (0, 0), "{r:?}");
        assert_eq!(metrics::KV_NEW_TOKENS.value(), 0);
        assert_eq!(metrics::KV_FULL_HITS.value(), 0);
    });
}

/// KV replay when the first chunk's object is evicted between its EXISTS and
/// its GET: the request has a load but no cached tokens, so it adds no sample
/// to the load time per 1,000 cached tokens.
#[test]
fn kv_replay_get_nil_at_the_first_chunk_has_no_per_token_load_time() {
    common::kv::isolated(
        "kv_replay_get_nil_at_the_first_chunk_has_no_per_token_load_time",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                evict_after_exists: Some(key(1, 0)),
                ..Default::default()
            });
            let a = request(1, 0, &[1, 2], 2 * ct + 3);
            let again = request(2, 1, &[1, 2], 2 * ct + 3);
            let rows = replay(&server, &[a, again], "");
            let r = &rows[1];
            assert_eq!((r.lookup_hit_chunks, r.hit_chunks), (2, 0), "{r:?}");
            assert_eq!((r.gets, r.get_nil, r.cached_tokens), (2, 1, 0), "{r:?}");
            assert_eq!(metrics::KV_LOADS.value(), 1);
            assert_eq!(metrics::KV_LOADS_WITH_CACHED.value(), 0);
            assert!(
                metrics::KV_LOAD_TIME_PER_1K_TOKENS
                    .load()
                    .is_none_or(|h| h.into_iter().all(|b| b.count() == 0)),
                "a per-token load time was recorded"
            );
        },
    );
}

/// KV replay when the server closes a connection while it waits for its next
/// request (as a server with an idle timeout does): the request handed to it
/// finds it closed before anything was sent, goes back to the dispatcher, and
/// runs after the reconnect. It is not counted as failed, and nothing is
/// counted as a send-pool failure.
#[test]
fn kv_replay_closed_idle_connection_reconnects_without_losing_a_request() {
    common::kv::isolated(
        "kv_replay_closed_idle_connection_reconnects_without_losing_a_request",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_after_sets: Some((2, Duration::from_millis(100))),
                ..Default::default()
            });
            let rows = replay(
                &server,
                // The server closes 100 ms after request 1's last SET
                // reply; request 2 is due at 300 ms, so the connection is
                // idle, waiting for it, when it closes.
                &[
                    request(1, 0, &[1, 2], 2 * ct + 3),
                    request(2, 300, &[3, 4], 2 * ct + 3),
                ],
                "speedup = 1",
            );
            assert_eq!(
                rows.iter().map(|r| r.req_id).collect::<Vec<_>>(),
                [1, 2],
                "{rows:?}"
            );
            assert_eq!(rows[1].sets, 2, "{rows:?}");
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 0);
            assert_eq!(metrics::REQUESTS_SEND_FAILED.value(), 0);
            assert_eq!(server.sets(), [key(1, 0), key(2, 0), key(3, 0), key(4, 0)]);
        },
    );
}

/// KV replay when the server closes every idle connection at once, as an idle
/// timeout does: the request handed to the first closed connection goes back
/// without using up its retry, may land on the next closed one, and runs once
/// a connection has reconnected. No request is lost.
#[test]
fn kv_replay_closing_every_idle_connection_loses_no_request() {
    common::kv::isolated(
        "kv_replay_closing_every_idle_connection_loses_no_request",
        || {
            let ct = CHUNK_TOKENS;
            // All three connections are shut 100 ms after request 1's SET;
            // request 2 is due at 300 ms.
            let server = Server::with_hooks(Hooks {
                close_all_after_sets: Some((1, Duration::from_millis(100))),
                ..Default::default()
            });
            let rows = replay_on(
                &server,
                &[request(1, 0, &[1], ct + 3), request(2, 300, &[2], ct + 3)],
                "speedup = 1",
                3,
            );
            assert_eq!(
                rows.iter().map(|r| r.req_id).collect::<Vec<_>>(),
                [1, 2],
                "{rows:?}"
            );
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 0);
        },
    );
}

/// KV replay: a request handed back while the dispatcher waits for a later
/// request's due time runs at once, before that later request.
#[test]
fn kv_replay_handed_back_request_runs_before_later_ones() {
    common::kv::isolated(
        "kv_replay_handed_back_request_runs_before_later_ones",
        || {
            let ct = CHUNK_TOKENS;
            // The connection that ran request 1 is closed 100 ms after its
            // second SET. Requests 2 and 3 are due at 300 and 600 ms, request 4
            // at 3,000 ms; request 3 reaches the closed connection.
            let server = Server::with_hooks(Hooks {
                close_after_sets: Some((2, Duration::from_millis(100))),
                ..Default::default()
            });
            let rows = replay_on(
                &server,
                &[
                    request(1, 0, &[1, 2], 2 * ct + 3),
                    request(2, 300, &[3], ct + 3),
                    request(3, 600, &[4], ct + 3),
                    request(4, 3000, &[5], ct + 3),
                ],
                "speedup = 1",
                2,
            );
            assert_eq!(
                rows.iter().map(|r| r.req_id).collect::<Vec<_>>(),
                [1, 2, 3, 4],
                "{rows:?}"
            );
            for r in &rows {
                let lag = r.lag_ns.unwrap();
                assert!(
                    lag < 100_000_000,
                    "request {} started {lag} ns late",
                    r.req_id
                );
            }
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 0);
        },
    );
}

/// KV replay when no connection can reconnect: a request handed back while
/// none of the worker's connections is connected is counted as failed once a
/// reconnect attempt fails, and the run ends instead of waiting for
/// `duration`.
#[test]
fn kv_replay_handed_back_request_fails_when_nothing_reconnects() {
    common::kv::isolated(
        "kv_replay_handed_back_request_fails_when_nothing_reconnects",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_all_after_sets: Some((1, Duration::from_millis(100))),
                refuse_after_close_all: true,
                ..Default::default()
            });
            let started = std::time::Instant::now();
            let rows = replay_on(
                &server,
                &[request(1, 0, &[1], ct + 3), request(2, 300, &[2], ct + 3)],
                "speedup = 1",
                1,
            );
            let took = started.elapsed();
            assert_eq!(
                rows.iter().map(|r| r.req_id).collect::<Vec<_>>(),
                [1],
                "{rows:?}"
            );
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 1);
            assert!(took < RUN_BOUND, "the run took {took:?}");
        },
    );
}

/// KV replay when every reconnect is accepted and closed at once: each
/// reconnect counts as a failed one, so a handed-back request is counted as
/// failed and the run ends instead of waiting for `duration`.
#[test]
fn kv_replay_handed_back_request_fails_when_reconnects_close_at_once() {
    common::kv::isolated(
        "kv_replay_handed_back_request_fails_when_reconnects_close_at_once",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_all_after_sets: Some((1, Duration::from_millis(100))),
                drop_new_after_close_all: true,
                ..Default::default()
            });
            let started = std::time::Instant::now();
            let rows = replay_on(
                &server,
                &[request(1, 0, &[1], ct + 3), request(2, 300, &[2], ct + 3)],
                "speedup = 1",
                1,
            );
            let took = started.elapsed();
            assert_eq!(
                rows.iter().map(|r| r.req_id).collect::<Vec<_>>(),
                [1],
                "{rows:?}"
            );
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 1);
            assert!(took < RUN_BOUND, "the run took {took:?}");
        },
    );
}

/// KV replay when every connection closes while a claimed request waits for
/// its start time and nothing reconnects: once due, the held request is
/// failed like a handed-back one, and the run ends instead of waiting for
/// `duration`. Request 2 takes the one idle connection's hand-back; request 3
/// is the held one.
#[test]
fn kv_replay_held_request_fails_when_nothing_reconnects() {
    common::kv::isolated(
        "kv_replay_held_request_fails_when_nothing_reconnects",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_all_after_sets: Some((1, Duration::from_millis(100))),
                refuse_after_close_all: true,
                ..Default::default()
            });
            let started = std::time::Instant::now();
            let rows = replay_on(
                &server,
                &[
                    request(1, 0, &[1], ct + 3),
                    request(2, 300, &[2], ct + 3),
                    request(3, 310, &[3], ct + 3),
                ],
                "speedup = 1",
                2,
            );
            let took = started.elapsed();
            assert_eq!(
                rows.iter().map(|r| r.req_id).collect::<Vec<_>>(),
                [1],
                "{rows:?}"
            );
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 2);
            assert!(took < RUN_BOUND, "the run took {took:?}");
        },
    );
}

/// KV replay over RESP3 when every reconnect is accepted and closed at once:
/// the failed `HELLO 3` counts as a failed reconnect, so the handed-back
/// request fails and the run ends instead of waiting for `duration`.
#[test]
fn kv_replay_resp3_handed_back_request_fails_when_reconnects_close_at_once() {
    common::kv::isolated(
        "kv_replay_resp3_handed_back_request_fails_when_reconnects_close_at_once",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_all_after_sets: Some((1, Duration::from_millis(100))),
                drop_new_after_close_all: true,
                ..Default::default()
            });
            let started = std::time::Instant::now();
            let rows = replay_with(
                &server,
                &[request(1, 0, &[1], ct + 3), request(2, 300, &[2], ct + 3)],
                "speedup = 1",
                1,
                "resp3",
            );
            let took = started.elapsed();
            assert_eq!(
                rows.iter().map(|r| r.req_id).collect::<Vec<_>>(),
                [1],
                "{rows:?}"
            );
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 1);
            assert!(took < RUN_BOUND, "the run took {took:?}");
        },
    );
}

/// KV replay of a trace longer than the reader thread's read-ahead (16,384
/// requests): every request is replayed exactly once, and the run ends when
/// the last one finishes rather than at `duration`.
#[test]
fn kv_replay_streams_a_trace_longer_than_the_read_ahead() {
    common::kv::isolated(
        "kv_replay_streams_a_trace_longer_than_the_read_ahead",
        || {
            const N: u64 = 20_000;
            let server = Server::default();
            // One chunk each, ids repeating every 100 requests so later ones hit.
            let requests: Vec<_> = (0..N)
                .map(|i| request(i, 0, &[i % 100], CHUNK_TOKENS))
                .collect();
            let started = std::time::Instant::now();
            let (result, rows) = replay_file(&server, write_trace(&requests), "", 8, "resp");
            result.unwrap();
            assert!(
                started.elapsed() < RUN_BOUND,
                "ran to duration: {:?}",
                started.elapsed()
            );
            let mut ids: Vec<u64> = rows.iter().map(|r| r.req_id).collect();
            ids.sort_unstable();
            assert_eq!(ids, (0..N).collect::<Vec<_>>());
            assert_eq!(metrics::KV_REQUESTS.value(), N);
        },
    );
}

/// KV replay of a file with a row out of timestamp order among its first
/// requests: the reader finds it before the replay starts, so no trace
/// request runs, and the run fails naming the row.
#[test]
fn kv_replay_read_error_before_the_start_runs_nothing() {
    common::kv::isolated("kv_replay_read_error_before_the_start_runs_nothing", || {
        let ct = CHUNK_TOKENS;
        let server = Server::default();
        let trace = write_unordered_trace(&[
            request(1, 0, &[1], ct),
            request(2, 2, &[2], ct),
            request(3, 1, &[3], ct),
            request(4, 3, &[4], ct),
        ]);
        let (result, rows) = replay_file(&server, trace, "", 1, "resp");
        let err = result.unwrap_err();
        assert!(
            err.contains("row 2 (req_id 3)") && err.contains("before the previous row's"),
            "{err}"
        );
        assert!(rows.is_empty(), "{rows:?}");
        assert!(server.sets().is_empty());
        assert_eq!(metrics::KV_REQUESTS.value(), 0);
    });
}

/// KV replay of a file whose out-of-order row is past what the reader can
/// read before the replay starts (`READ_AHEAD`, 16,384): the rows before it
/// are replayed, the run ends when they finish rather than at `duration`,
/// and then fails naming the row.
#[test]
fn kv_replay_read_error_after_the_start_fails_the_run_after_the_rows_before_it() {
    common::kv::isolated(
        "kv_replay_read_error_after_the_start_fails_the_run_after_the_rows_before_it",
        || {
            const N: u64 = 20_000;
            let ct = CHUNK_TOKENS;
            let server = Server::default();
            let mut requests: Vec<_> = (0..N).map(|i| request(i, 1, &[i % 100], ct)).collect();
            requests.push(request(N, 0, &[1], ct));
            let trace = write_unordered_trace(&requests);
            let started = std::time::Instant::now();
            let (result, rows) = replay_file(&server, trace, "", 8, "resp");
            let took = started.elapsed();
            assert!(took < RUN_BOUND, "ran to duration: {took:?}");
            let err = result.unwrap_err();
            assert!(
                err.contains(&format!("row {N} (req_id {N})"))
                    && err.contains("before the previous row's"),
                "{err}"
            );
            let mut ids: Vec<u64> = rows.iter().map(|r| r.req_id).collect();
            ids.sort_unstable();
            assert_eq!(ids, (0..N).collect::<Vec<_>>());
        },
    );
}

/// KV replay whose trace finishes during warmup: nothing is recorded, and the
/// run fails rather than reporting the warmup traffic as its result.
#[test]
fn kv_replay_trace_finished_during_warmup_fails_the_run() {
    common::kv::isolated(
        "kv_replay_trace_finished_during_warmup_fails_the_run",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::default();
            let trace = write_trace(&[request(1, 0, &[1], ct), request(2, 0, &[1], ct)]);
            let (result, rows) = replay_file(&server, trace, "general.warmup = \"3s\"", 1, "resp");
            let err = result.unwrap_err();
            assert!(err.contains("finished before recording started"), "{err}");
            assert_eq!(rows.len(), 2, "{rows:?}");
        },
    );
}

/// Where the server read `cmd` for `chunk`'s rank-0 object first.
fn first_at(server: &Server, cmd: &str, chunk: u64) -> std::time::Instant {
    server
        .log()
        .into_iter()
        .find(|o| o.cmd == cmd && o.key == key(chunk, 0))
        .unwrap_or_else(|| panic!("no {cmd} of chunk {chunk}"))
        .at
}

/// KV replay with `store_connections`: a request's SETs go to a store
/// connection, so the next request's lookup does not wait for them, and a
/// store queued behind a slow one records the wait as queue time.
#[test]
fn kv_replay_store_connections_take_stores_off_the_lookup_path() {
    common::kv::isolated(
        "kv_replay_store_connections_take_stores_off_the_lookup_path",
        || {
            let ct = CHUNK_TOKENS;
            let hold = Duration::from_millis(500);
            let server = Server::with_hooks(Hooks {
                delay_set: Some((key(1, 0), hold)),
                ..Default::default()
            });
            let rows = replay_on(
                &server,
                &[request(1, 0, &[1], ct), request(2, 0, &[2], ct)],
                "store_connections = 1",
                2,
            );
            assert_eq!(rows.len(), 2, "{rows:?}");
            // The run ends only once every queued store has finished.
            assert_eq!(server.sets(), vec![key(1, 0), key(2, 0)]);

            // Request 2's lookup reached the server while request 1's SET was
            // held.
            let gap = first_at(&server, "EXISTS", 2).duration_since(first_at(&server, "SET", 1));
            assert!(gap < hold / 2, "lookup 2 came {gap:?} after SET 1");

            let row = |id: u64| rows.iter().find(|r| r.req_id == id).unwrap();
            let (r1, r2) = (row(1), row(2));
            assert_eq!((r1.sets, r2.sets), (1, 1));
            assert!(!r1.store_dropped && !r2.store_dropped);
            // Store time runs from queued to the last SET reply, so request
            // 1's includes the hold, and request 2's includes waiting for
            // the writer behind it.
            assert!(r1.store_ns >= hold.as_nanos() as u64, "{r1:?}");
            let q2 = r2
                .store_queue_ns
                .expect("queue time with store_connections");
            assert!(q2 >= (hold / 2).as_nanos() as u64, "{r2:?}");
            assert!(r2.store_ns >= q2, "{r2:?}");
            // Ready is the end of the load, before any store.
            for r in [r1, r2] {
                assert!(r.ready_ns >= r.lookup_ns + r.load_ns, "{r:?}");
                assert!(r.ready_ns < (hold / 2).as_nanos() as u64, "{r:?}");
            }
            assert_eq!(metrics::KV_REQUESTS.value(), 2);
            assert_eq!(metrics::KV_STORES.value(), 2);
        },
    );
}

/// The control for `kv_replay_store_connections_take_stores_off_the_lookup_path`:
/// on one connection without `store_connections`, the next request's lookup
/// waits for the held SET's reply, and no queue time is recorded.
#[test]
fn kv_replay_without_store_connections_a_lookup_waits_for_the_store() {
    common::kv::isolated(
        "kv_replay_without_store_connections_a_lookup_waits_for_the_store",
        || {
            let ct = CHUNK_TOKENS;
            let hold = Duration::from_millis(500);
            let server = Server::with_hooks(Hooks {
                delay_set: Some((key(1, 0), hold)),
                ..Default::default()
            });
            let rows = replay(
                &server,
                &[request(1, 0, &[1], ct), request(2, 0, &[2], ct)],
                "",
            );
            assert_eq!(rows.len(), 2, "{rows:?}");
            let gap = first_at(&server, "EXISTS", 2).duration_since(first_at(&server, "SET", 1));
            assert!(gap >= hold, "lookup 2 came {gap:?} after SET 1");
            for r in &rows {
                assert_eq!(r.store_queue_ns, None, "{r:?}");
                assert!(!r.store_dropped, "{r:?}");
                assert!(r.ready_ns >= r.lookup_ns + r.load_ns, "{r:?}");
            }
        },
    );
}

/// KV replay with `store_queue`: a store that finds the worker's queue full
/// is dropped whole, its request still completes, and nothing of it is sent.
#[test]
fn kv_replay_a_full_store_queue_drops_the_whole_store() {
    common::kv::isolated("kv_replay_a_full_store_queue_drops_the_whole_store", || {
        let ct = CHUNK_TOKENS;
        let server = Server::with_hooks(Hooks {
            delay_set: Some((key(1, 0), Duration::from_millis(600))),
            ..Default::default()
        });
        // Due at 0, 100 and 200 ms. Request 1's store holds the writer;
        // request 2's waits in the queue, which then is full for request 3's.
        let rows = replay_on(
            &server,
            &[
                request(1, 0, &[1], ct),
                request(2, 100, &[2], ct),
                request(3, 200, &[3, 4], 2 * ct),
            ],
            "speedup = 1\nstore_connections = 1\nstore_queue = 1",
            2,
        );
        assert_eq!(rows.len(), 3, "{rows:?}");
        let row = |id: u64| rows.iter().find(|r| r.req_id == id).unwrap();
        assert!(!row(1).store_dropped && !row(2).store_dropped, "{rows:?}");
        let r3 = row(3);
        assert!(r3.store_dropped, "{r3:?}");
        assert_eq!((r3.sets, r3.store_ns, r3.store_queue_ns), (0, 0, None));
        assert_eq!(server.sets(), vec![key(1, 0), key(2, 0)]);
        assert_eq!(metrics::KV_STORES_DROPPED.value(), 1);
        assert_eq!(metrics::KV_REQUESTS.value(), 3);
        assert_eq!(metrics::KV_STORES.value(), 2);
    });
}

/// KV replay with `store_connections` when the server closes the idle store
/// connection: the next store reaches it closed with nothing sent, goes back
/// on the queue, and runs once the store connection has reconnected.
#[test]
fn kv_replay_store_on_a_closed_store_connection_runs_after_it_reconnects() {
    common::kv::isolated(
        "kv_replay_store_on_a_closed_store_connection_runs_after_it_reconnects",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_after_sets: Some((1, Duration::from_millis(50))),
                ..Default::default()
            });
            let rows = replay_on(
                &server,
                &[request(1, 0, &[1], ct), request(2, 300, &[2], ct)],
                "speedup = 1\nstore_connections = 1",
                2,
            );
            let mut ids: Vec<u64> = rows.iter().map(|r| r.req_id).collect();
            ids.sort();
            assert_eq!(ids, [1, 2], "{rows:?}");
            assert_eq!(server.sets(), vec![key(1, 0), key(2, 0)]);
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 0);
        },
    );
}

/// KV replay with `store_connections` when the store connection closes and
/// cannot reconnect while the lookup connection stays open: the queued store
/// is failed once a reconnect has failed, and the run ends instead of waiting
/// for `duration`.
#[test]
fn kv_replay_store_fails_when_no_store_connection_reconnects() {
    common::kv::isolated(
        "kv_replay_store_fails_when_no_store_connection_reconnects",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_after_sets: Some((1, Duration::from_millis(50))),
                refuse_after_close: true,
                ..Default::default()
            });
            let started = std::time::Instant::now();
            let rows = replay_on(
                &server,
                &[request(1, 0, &[1], ct), request(2, 300, &[2], ct)],
                "speedup = 1\nstore_connections = 1",
                2,
            );
            let took = started.elapsed();
            assert_eq!(
                rows.iter().map(|r| r.req_id).collect::<Vec<_>>(),
                [1],
                "{rows:?}"
            );
            // Request 2's lookup ran on the connection that stayed open.
            assert!(server.ops("EXISTS").contains(&key(2, 0)));
            assert_eq!(server.sets(), vec![key(1, 0)]);
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 1);
            assert!(took < RUN_BOUND, "the run took {took:?}");
        },
    );
}

/// The first `cmd` of chunk `chunk` (rank 0) the server received.
fn first_op(server: &Server, cmd: &str, chunk: u64) -> common::kv::Op {
    server
        .log()
        .into_iter()
        .find(|o| o.cmd == cmd && o.key == key(chunk, 0))
        .unwrap_or_else(|| panic!("no {cmd} of chunk {chunk}"))
}

/// Request 1 stores chunks 1 to 4; request 2, 300 ms later, loads them.
fn store_then_load() -> Vec<kvtrace::Request> {
    let ct = CHUNK_TOKENS;
    vec![
        request(1, 0, &[1, 2, 3, 4], 4 * ct),
        request(2, 300, &[1, 2, 3, 4], 4 * ct),
    ]
}

/// KV replay with `client = "vllm"`: a load's GETs are tasks that any idle
/// load- or store-priority connection takes, one at a time each. With chunk
/// 1's GET held on one connection, the store-priority connection, finding no
/// store queued, sends the other three GETs before the held one is answered.
#[test]
fn kv_replay_vllm_client_spreads_a_load_over_the_priority_connections() {
    common::kv::isolated(
        "kv_replay_vllm_client_spreads_a_load_over_the_priority_connections",
        || {
            let hold = Duration::from_millis(500);
            let server = Server::with_hooks(Hooks {
                delay_get: Some((key(1, 0), hold)),
                ..Default::default()
            });
            let rows = replay_on(
                &server,
                &store_then_load(),
                "speedup = 1\nclient = \"vllm\"\nload_connections = 1\nstore_connections = 1",
                3,
            );
            assert_eq!(rows.len(), 2, "{rows:?}");
            let held = first_op(&server, "GET", 1);
            for chunk in [2, 3, 4] {
                let get = first_op(&server, "GET", chunk);
                assert_ne!(
                    get.conn, held.conn,
                    "GET {chunk} queued behind the held GET"
                );
                let after = get.at.saturating_duration_since(held.at);
                assert!(after < hold / 2, "GET {chunk} came {after:?} after GET 1");
            }
            assert_eq!(server.ops("GET").len(), 4);
            let r2 = rows.iter().find(|r| r.req_id == 2).unwrap();
            assert_eq!((r2.hit_chunks, r2.gets, r2.sets), (4, 4, 0), "{r2:?}");
            // The load ends with its last GET reply, the held one.
            assert!(r2.load_ns >= hold.as_nanos() as u64, "{r2:?}");
            assert!(r2.ready_ns >= r2.lookup_ns + r2.load_ns, "{r2:?}");
            let r1 = rows.iter().find(|r| r.req_id == 1).unwrap();
            assert_eq!(r1.sets, 4, "{r1:?}");
            assert!(r1.store_queue_ns.is_some(), "{r1:?}");
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 0);
        },
    );
}

/// The control for `kv_replay_vllm_client_spreads_a_load_over_the_priority_connections`:
/// without `client = "vllm"` the load is pipelined on the connection that
/// looked up, so the GETs after the held one are read only once it is
/// answered.
#[test]
fn kv_replay_lmcache_client_pipelines_a_load_on_one_connection() {
    common::kv::isolated(
        "kv_replay_lmcache_client_pipelines_a_load_on_one_connection",
        || {
            let hold = Duration::from_millis(500);
            let server = Server::with_hooks(Hooks {
                delay_get: Some((key(1, 0), hold)),
                ..Default::default()
            });
            let rows = replay_on(&server, &store_then_load(), "speedup = 1", 3);
            assert_eq!(rows.len(), 2, "{rows:?}");
            let held = first_op(&server, "GET", 1);
            for chunk in [2, 3, 4] {
                let get = first_op(&server, "GET", chunk);
                assert_eq!(get.conn, held.conn, "GET {chunk}");
                let after = get.at.saturating_duration_since(held.at);
                assert!(after >= hold, "GET {chunk} came {after:?} after GET 1");
            }
        },
    );
}

/// KV replay with `client = "vllm"`: a lookup connection waits for its
/// request's load but not for its store, so the next request's lookup
/// reaches the server while the first request's SET is held, and the store
/// records its queue time.
#[test]
fn kv_replay_vllm_client_lookup_does_not_wait_for_the_store() {
    common::kv::isolated(
        "kv_replay_vllm_client_lookup_does_not_wait_for_the_store",
        || {
            let ct = CHUNK_TOKENS;
            let hold = Duration::from_millis(500);
            let server = Server::with_hooks(Hooks {
                delay_set: Some((key(1, 0), hold)),
                ..Default::default()
            });
            let rows = replay_on(
                &server,
                &[request(1, 0, &[1], ct), request(2, 0, &[2], ct)],
                "client = \"vllm\"\nload_connections = 1",
                2,
            );
            assert_eq!(rows.len(), 2, "{rows:?}");
            assert_eq!(server.sets(), vec![key(1, 0), key(2, 0)]);
            let gap = first_at(&server, "EXISTS", 2).duration_since(first_at(&server, "SET", 1));
            assert!(gap < hold / 2, "lookup 2 came {gap:?} after SET 1");
            let row = |id: u64| rows.iter().find(|r| r.req_id == id).unwrap();
            let (r1, r2) = (row(1), row(2));
            assert_eq!((r1.sets, r2.sets), (1, 1));
            assert!(r1.store_ns >= hold.as_nanos() as u64, "{r1:?}");
            // Request 2's SET waits for the one load-priority connection.
            let q2 = r2.store_queue_ns.expect("queue time with client = vllm");
            assert!(q2 >= (hold / 2).as_nanos() as u64, "{r2:?}");
            assert!(r2.store_ns >= q2, "{r2:?}");
            assert_eq!(metrics::KV_STORES.value(), 2);
        },
    );
}

/// KV replay with `client = "vllm"` and `store_queue`: a store that finds
/// the worker's queue full is dropped whole.
#[test]
fn kv_replay_vllm_client_full_store_queue_drops_the_whole_store() {
    common::kv::isolated(
        "kv_replay_vllm_client_full_store_queue_drops_the_whole_store",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                delay_set: Some((key(1, 0), Duration::from_millis(600))),
                ..Default::default()
            });
            // Request 1's SET holds the one load-priority connection; request 2's store
            // waits in the queue, which then is full for request 3's.
            let rows = replay_on(
                &server,
                &[
                    request(1, 0, &[1], ct),
                    request(2, 100, &[2], ct),
                    request(3, 200, &[3, 4], 2 * ct),
                ],
                "speedup = 1\nclient = \"vllm\"\nload_connections = 1\nstore_queue = 1",
                2,
            );
            assert_eq!(rows.len(), 3, "{rows:?}");
            let row = |id: u64| rows.iter().find(|r| r.req_id == id).unwrap();
            assert!(!row(1).store_dropped && !row(2).store_dropped, "{rows:?}");
            let r3 = row(3);
            assert!(r3.store_dropped, "{r3:?}");
            assert_eq!((r3.sets, r3.store_ns, r3.store_queue_ns), (0, 0, None));
            assert_eq!(server.sets(), vec![key(1, 0), key(2, 0)]);
            assert_eq!(metrics::KV_STORES_DROPPED.value(), 1);
        },
    );
}

/// KV replay with `client = "vllm"` when the server closes the idle
/// load-priority connection: the next task reaches it closed with nothing sent, goes back
/// on its queue, and runs once the connection has reconnected.
#[test]
fn kv_replay_vllm_client_task_on_a_closed_connection_runs_after_it_reconnects() {
    common::kv::isolated(
        "kv_replay_vllm_client_task_on_a_closed_connection_runs_after_it_reconnects",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_after_sets: Some((1, Duration::from_millis(50))),
                ..Default::default()
            });
            let rows = replay_on(
                &server,
                &[request(1, 0, &[1], ct), request(2, 300, &[1, 2], 2 * ct)],
                "speedup = 1\nclient = \"vllm\"\nload_connections = 1",
                2,
            );
            let mut ids: Vec<u64> = rows.iter().map(|r| r.req_id).collect();
            ids.sort();
            assert_eq!(ids, [1, 2], "{rows:?}");
            let r2 = rows.iter().find(|r| r.req_id == 2).unwrap();
            assert_eq!((r2.hit_chunks, r2.gets, r2.sets), (1, 1, 1), "{r2:?}");
            assert_eq!(server.sets(), vec![key(1, 0), key(2, 0)]);
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 0);
        },
    );
}

/// KV replay with `client = "vllm"` when the one load-priority connection closes and
/// cannot reconnect while the lookup connection stays open: a queued load
/// and a queued store are each failed once a reconnect has failed, and the
/// run ends instead of waiting for `duration`.
#[test]
fn kv_replay_vllm_client_tasks_fail_when_no_priority_connection_reconnects() {
    common::kv::isolated(
        "kv_replay_vllm_client_tasks_fail_when_no_priority_connection_reconnects",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_after_sets: Some((1, Duration::from_millis(50))),
                refuse_after_close: true,
                ..Default::default()
            });
            let started = std::time::Instant::now();
            // Request 2 hits chunk 1 and must load it; request 3 misses and
            // must store.
            let rows = replay_on(
                &server,
                &[
                    request(1, 0, &[1], ct),
                    request(2, 300, &[1], ct),
                    request(3, 600, &[3], ct),
                ],
                "speedup = 1\nclient = \"vllm\"\nload_connections = 1",
                2,
            );
            let took = started.elapsed();
            assert_eq!(
                rows.iter().map(|r| r.req_id).collect::<Vec<_>>(),
                [1],
                "{rows:?}"
            );
            let exists = server.ops("EXISTS");
            assert!(exists.contains(&key(3, 0)), "{exists:?}");
            assert!(server.ops("GET").is_empty());
            assert_eq!(server.sets(), vec![key(1, 0)]);
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 2);
            assert!(took < RUN_BOUND, "the run took {took:?}");
        },
    );
}

/// KV replay with `client = "vllm"`: a GET that returns nil ends the cached
/// prefix at its chunk, as with the default client.
#[test]
fn kv_replay_vllm_client_get_nil_ends_the_cached_prefix() {
    common::kv::isolated(
        "kv_replay_vllm_client_get_nil_ends_the_cached_prefix",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                evict_after_exists: Some(key(2, 0)),
                ..Default::default()
            });
            let rows = replay_on(
                &server,
                &[
                    request(1, 0, &[1, 2, 3], 3 * ct + 3),
                    request(2, 300, &[1, 2, 3], 3 * ct + 3),
                ],
                "speedup = 1\nclient = \"vllm\"\nload_connections = 1\nstore_connections = 1",
                3,
            );
            let r = rows.iter().find(|r| r.req_id == 2).unwrap();
            assert_eq!((r.lookup_hit_chunks, r.hit_chunks), (3, 1), "{r:?}");
            assert_eq!((r.gets, r.get_nil, r.sets), (3, 1, 0), "{r:?}");
            assert_eq!(r.cached_tokens, u64::from(ct), "{r:?}");
            assert_eq!(metrics::KV_GET_NIL.value(), 1);
        },
    );
}

/// KV replay with `client = "vllm"`: a GET whose connection closes after it
/// was sent fails its request, the lookup connection waiting for the load
/// moves on, and a later request runs once the connection has reconnected.
#[test]
fn kv_replay_vllm_client_get_on_a_failed_connection_fails_its_request() {
    common::kv::isolated(
        "kv_replay_vllm_client_get_on_a_failed_connection_fails_its_request",
        || {
            let ct = CHUNK_TOKENS;
            let server = Server::with_hooks(Hooks {
                close_on_get: Some(key(1, 0)),
                ..Default::default()
            });
            let started = std::time::Instant::now();
            let rows = replay_on(
                &server,
                &[
                    request(1, 0, &[1], ct),
                    request(2, 300, &[1], ct),
                    request(3, 600, &[3], ct),
                ],
                "speedup = 1\nclient = \"vllm\"\nload_connections = 1",
                2,
            );
            let took = started.elapsed();
            let mut ids: Vec<u64> = rows.iter().map(|r| r.req_id).collect();
            ids.sort();
            assert_eq!(ids, [1, 3], "{rows:?}");
            assert_eq!(server.ops("GET"), vec![key(1, 0)]);
            assert_eq!(server.sets(), vec![key(1, 0), key(3, 0)]);
            assert_eq!(metrics::KV_REQUESTS_FAILED.value(), 1);
            assert!(took < RUN_BOUND, "the run took {took:?}");
        },
    );
}
