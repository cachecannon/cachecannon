//! ann-benchmarks HDF5 dataset loading.
//!
//! The convention (https://github.com/erikbern/ann-benchmarks): `train` is the
//! N x D float array loaded into the store, `test` is the Q x D query set, and
//! `neighbors` is the Q x k ground-truth array where row i holds the indices
//! (into `train`) of the true nearest neighbors of `test[i]`. The `distance`
//! file attribute names the metric.

use std::error::Error;
use std::path::Path;

use ndarray::Array2;

pub struct Dataset {
    pub train: Array2<f32>,
    pub test: Array2<f32>,
    pub neighbors: Array2<i32>,
    /// Metric as recorded in the file: `euclidean` or `angular`.
    pub metric: String,
}

impl Dataset {
    pub fn dim(&self) -> usize {
        self.train.ncols()
    }

    /// Ground-truth depth: the maximum k recall can be scored at.
    pub fn neighbors_k(&self) -> usize {
        self.neighbors.ncols()
    }

    /// The metric mapped to Valkey Search's `DISTANCE_METRIC` argument.
    /// ann-benchmarks "angular" datasets are unit-normalized, so cosine
    /// distance gives the same ranking.
    pub fn distance_metric_arg(&self) -> Result<&'static [u8], String> {
        match self.metric.as_str() {
            "euclidean" => Ok(b"L2"),
            "angular" | "cosine" => Ok(b"COSINE"),
            other => Err(format!(
                "unsupported dataset metric {other:?} (expected euclidean or angular)"
            )),
        }
    }

    /// Row `i` of `train`/`test` as the little-endian FLOAT32 blob Valkey
    /// Search expects. Endianness is a correctness requirement: a mismatch
    /// silently wrecks recall rather than erroring.
    pub fn f32_le_blob(row: ndarray::ArrayView1<'_, f32>) -> Vec<u8> {
        let mut blob = Vec::with_capacity(row.len() * 4);
        for v in row {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        blob
    }
}

pub fn load(path: &Path) -> Result<Dataset, Box<dyn Error>> {
    let file = hdf5_metno::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;

    let train = file
        .dataset("train")
        .and_then(|d| d.read_2d::<f32>())
        .map_err(|e| format!("read `train`: {e}"))?;
    let test = file
        .dataset("test")
        .and_then(|d| d.read_2d::<f32>())
        .map_err(|e| format!("read `test`: {e}"))?;
    let neighbors = file
        .dataset("neighbors")
        .and_then(|d| d.read_2d::<i32>())
        .map_err(|e| format!("read `neighbors`: {e}"))?;

    if train.ncols() != test.ncols() {
        return Err(format!("train dim {} != test dim {}", train.ncols(), test.ncols()).into());
    }
    if neighbors.nrows() != test.nrows() {
        return Err(format!(
            "neighbors rows {} != test rows {}",
            neighbors.nrows(),
            test.nrows()
        )
        .into());
    }

    // Silently guessing the metric would benchmark the wrong distance
    // function; ann-benchmarks files always carry the `distance` attribute.
    let metric = file
        .attr("distance")
        .and_then(|a| a.read_scalar::<hdf5_metno::types::VarLenUnicode>())
        .map(|s| s.to_string())
        .map_err(|e| format!("read `distance` attribute: {e}"))?;

    Ok(Dataset {
        train,
        test,
        neighbors,
        metric,
    })
}
