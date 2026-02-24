# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.2] - 2026-02-24

### Changed
- Update ringline dependencies to 0f76448
- Momento latency metrics now use the on_result callback pattern, matching
  redis/memcache/ping protocols

## [0.0.1] - 2026-02-21

Initial release. Extracted from the [crucible](https://github.com/brayniac/crucible)
benchmark module into a standalone tool.

### Added
- High-performance cache protocol benchmarking with io_uring via ringline
- Protocol support: Redis/Valkey (RESP), Memcache (ASCII), Momento, Ping
- Multiplexed and pipelined request modes
- Kernel SO_TIMESTAMPING for precise latency measurement
- Precheck phase to verify connectivity before warmup
- Prefill phase to populate cache before benchmarking
- Configurable workload: keyspace, value size, command mix (get/set/delete)
- Rate limiting support
- Cluster mode with CLUSTER SLOTS discovery and redirect handling
- TLS support for all protocols
- Metrics exposition via Parquet snapshots and HTTP endpoint
- Built-in results viewer with web UI
- APT and YUM package repositories
- CI: clippy, tests, Momento integration tests, Redis/Valkey TLS tests
