#!/usr/bin/env python3
# Copyright 2026 Cachecannon Authors
# SPDX-License-Identifier: Apache-2.0
"""Generate a small ann-benchmarks-format HDF5 dataset for `valkey-lab search`.

Produces the same layout as the datasets at https://github.com/erikbern/ann-benchmarks
(`train`, `test`, `neighbors`, `distances`), with brute-force exact ground truth
computed offline, so the harness can be exercised end to end in seconds without
downloading a real corpus.

The default size (10k train / 100 test / dim 32) builds and queries in seconds
while still being large enough that a misconfigured index shows up as bad recall.

Usage:
    python3 scripts/make-search-test-dataset.py --out /tmp/search-test.hdf5
    python3 scripts/make-search-test-dataset.py --train 50000 --dim 64 \
        --metric angular --out /tmp/glove-ish.hdf5
"""

import argparse

import h5py
import numpy as np


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--train", type=int, default=10_000, help="train vectors (N)")
    ap.add_argument("--test", type=int, default=100, help="query vectors (Q)")
    ap.add_argument("--dim", type=int, default=32, help="dimensions (D)")
    ap.add_argument("--k", type=int, default=100, help="ground-truth neighbors per query")
    ap.add_argument(
        "--metric",
        choices=["euclidean", "angular"],
        default="euclidean",
        help="distance metric recorded in the file and used for ground truth",
    )
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--out", required=True, help="output .hdf5 path")
    args = ap.parse_args()

    rng = np.random.default_rng(args.seed)
    # Clustered rather than uniform: draw cluster centers, then points around
    # them, so HNSW recall behaves like it does on real data (uniform random
    # vectors make every neighbor list look the same and hide ranking bugs).
    n_clusters = max(8, args.train // 500)
    centers = rng.standard_normal((n_clusters, args.dim)).astype(np.float32) * 4.0
    assign = rng.integers(0, n_clusters, size=args.train)
    train = centers[assign] + rng.standard_normal((args.train, args.dim)).astype(np.float32)
    tassign = rng.integers(0, n_clusters, size=args.test)
    test = centers[tassign] + rng.standard_normal((args.test, args.dim)).astype(np.float32)

    if args.metric == "angular":
        # ann-benchmarks angular = cosine on unit-normalized vectors.
        train /= np.linalg.norm(train, axis=1, keepdims=True)
        test /= np.linalg.norm(test, axis=1, keepdims=True)

    # Brute-force exact ground truth (O(N*Q*D), fine at this size).
    # For both metrics, squared-L2 ordering on (normalized) vectors gives the
    # correct neighbor ranking.
    d2 = (
        np.sum(test**2, axis=1, keepdims=True)
        - 2.0 * test @ train.T
        + np.sum(train**2, axis=1)
    )
    order = np.argsort(d2, axis=1, kind="stable")[:, : args.k]
    d2_top = np.maximum(np.take_along_axis(d2, order, axis=1), 0.0)
    if args.metric == "angular":
        # On unit vectors d2 = 2 - 2*cos, so cosine distance = d2 / 2.
        dist = d2_top / 2.0
    else:
        dist = np.sqrt(d2_top)

    with h5py.File(args.out, "w") as f:
        f.attrs["distance"] = args.metric
        f.attrs["dimension"] = args.dim
        f.attrs["point_type"] = "float"
        f.create_dataset("train", data=train.astype(np.float32))
        f.create_dataset("test", data=test.astype(np.float32))
        f.create_dataset("neighbors", data=order.astype(np.int32))
        f.create_dataset("distances", data=dist.astype(np.float32))

    print(
        f"wrote {args.out}: train {train.shape}, test {test.shape}, "
        f"neighbors {order.shape}, metric={args.metric}"
    )


if __name__ == "__main__":
    main()
