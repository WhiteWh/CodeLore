//! Immutable ANN generations beside an Atlas SQLite map.
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use memmap2::{Mmap, MmapOptions};
use serde::{Deserialize, Serialize};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

#[path = "ann_builder.rs"]
mod builder;
pub(crate) use builder::run_worker;
pub(super) use builder::{schedule, status};

pub(super) const CONNECTIVITY: usize = 24;
pub(super) const EXPANSION_ADD: usize = 112;
pub(super) const EXPANSION_SEARCH: usize = 1536;
pub(super) const SHORTLIST: usize = 160;
const CACHE_SIZE: usize = 4;
const KEY_BYTES: usize = 48;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Binding {
    pub source_key: String,
    pub space: String,
    pub vector_model: String,
    pub dimension: usize,
    pub revision: String,
    pub refreshed: String,
    pub vector_generation: Option<String>,
    pub covered: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub format: u32,
    pub metric: String,
    pub connectivity: usize,
    pub expansion_add: usize,
    pub generation: String,
    pub binding: Binding,
    pub index: Stamp,
    pub keys: Stamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Stamp {
    pub bytes: u64,
    pub modified_ns: u64,
}

impl Stamp {
    pub fn read(path: &Path) -> io::Result<Self> {
        let meta = fs::metadata(path)?;
        let modified_ns = meta.modified()?.duration_since(UNIX_EPOCH).map_err(io::Error::other)?
            .as_nanos().try_into().map_err(io::Error::other)?;
        Ok(Self { bytes: meta.len(), modified_ns })
    }
}

impl Binding {
    pub fn directory(&self, db: &Path) -> PathBuf {
        db.with_file_name("ann-v1").join(super::unreal::digest(&self.space))
    }

    /// The same source and vector space. A generation built for an older
    /// revision of it still names useful candidates: search re-checks each
    /// one's chunk ID and embedding hash against SQLite before scoring it.
    pub fn compatible(&self, other: &Binding) -> bool {
        self.source_key == other.source_key && self.space == other.space
            && self.vector_model == other.vector_model && self.dimension == other.dimension
    }
}

pub(super) enum Unavailable {
    Missing,
    Stale,
    Damaged(String),
}

fn current(directory: &Path) -> Result<Manifest, Unavailable> {
    use Unavailable::{Damaged, Missing};
    let pointer = match File::open(directory.join("current.json")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(Missing),
        Err(error) => return Err(Damaged(error.to_string())),
    };
    serde_json::from_reader(pointer.take(16 * 1024)).map_err(|error| Damaged(error.to_string()))
}

fn valid_generation(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn check_files(manifest: &Manifest, index: &Path, keys: &Path) -> Result<(), Unavailable> {
    if Stamp::read(index).is_ok_and(|stamp| stamp == manifest.index)
        && Stamp::read(keys).is_ok_and(|stamp| stamp == manifest.keys)
        && manifest.binding.covered.checked_mul(KEY_BYTES).map(|bytes| bytes as u64) == Some(manifest.keys.bytes)
    {
        Ok(())
    } else {
        Err(Unavailable::Damaged("ANN generation files are missing or have changed".to_owned()))
    }
}

/// The published generation built for exactly this binding.
fn inspect(directory: &Path, binding: &Binding) -> Result<(Manifest, PathBuf, PathBuf), Unavailable> {
    inspect_where(directory, |built| built == binding)
}

/// The published generation for this source and vector space, possibly built
/// for an older revision of it.
pub(super) fn inspect_compatible(directory: &Path, binding: &Binding) -> Result<(Manifest, PathBuf, PathBuf), Unavailable> {
    inspect_where(directory, |built| built.compatible(binding))
}

fn inspect_where(directory: &Path, accept: impl Fn(&Binding) -> bool) -> Result<(Manifest, PathBuf, PathBuf), Unavailable> {
    let manifest = current(directory)?;
    if manifest.format != 1 || manifest.metric != "f16_ip" || manifest.connectivity != CONNECTIVITY
        || manifest.expansion_add != EXPANSION_ADD || !accept(&manifest.binding)
    {
        return Err(Unavailable::Stale);
    }
    if !valid_generation(&manifest.generation) {
        return Err(Unavailable::Damaged("invalid ANN generation name".to_owned()));
    }
    let index = directory.join(format!("{}.usearch", manifest.generation));
    let keys = directory.join(format!("{}.keys", manifest.generation));
    check_files(&manifest, &index, &keys)?;
    Ok((manifest, index, keys))
}

fn options(dimension: usize) -> IndexOptions {
    IndexOptions {
        dimensions: dimension, metric: MetricKind::IP, quantization: ScalarKind::F16,
        connectivity: CONNECTIVITY, expansion_add: EXPANSION_ADD, expansion_search: EXPANSION_SEARCH, multi: false,
    }
}

pub(super) struct Reader {
    index: Index,
    // USearch's view borrows these pages. Field drop order closes the index
    // first. Opening a Rust File also supports long and Unicode paths on Windows.
    _index_mapping: Mmap,
    keys: Vec<u8>,
    manifest: Manifest,
}

type Cached = (PathBuf, Arc<Reader>);
static READERS: Mutex<VecDeque<Cached>> = Mutex::new(VecDeque::new());

impl Reader {
    /// The current generation for `binding`'s source and space. It may lag
    /// the snapshot; `fresh_for` says whether it does.
    pub fn open(db: &Path, binding: &Binding) -> Result<Arc<Self>, Unavailable> {
        use Unavailable::Damaged;
        let (manifest, index, keys) = inspect_compatible(&binding.directory(db), binding)?;
        // The files describe the binding they were built for.
        let binding = &manifest.binding.clone();
        if let Ok(mut cache) = READERS.lock() {
            if let Some(pos) = cache.iter().position(|(path, reader)| *path == index && reader.manifest == manifest) {
                let entry = cache.remove(pos).expect("cached reader");
                let reader = Arc::clone(&entry.1);
                cache.push_front(entry);
                return Ok(reader);
            }
        }
        let file = File::open(&index).map_err(|error| Damaged(error.to_string()))?;
        // Published versions are immutable. A writer must publish a new path
        // instead of editing or truncating a potentially mapped generation.
        let mapping = unsafe { MmapOptions::new().map(&file) }.map_err(|error| Damaged(error.to_string()))?;
        if mapping.len() as u64 != manifest.index.bytes {
            return Err(Damaged("ANN graph length changed while opening".to_owned()));
        }
        let header = Index::metadata_from_buffer(&mapping).map_err(|error| Damaged(error.to_string()))?;
        if header.dimensions != binding.dimension as u64 || header.count_present != binding.covered as u64
            || header.count_deleted != 0 || header.multi || !matches!(header.metric, MetricKind::IP)
            || !matches!(header.quantization, ScalarKind::F16)
        {
            return Err(Damaged("ANN header disagrees with its manifest".to_owned()));
        }
        let key_data = fs::read(&keys).map_err(|error| Damaged(error.to_string()))?;
        let mut previous = 0;
        for record in key_data.chunks_exact(KEY_BYTES) {
            let id = key_id(record);
            if id <= previous || id > i64::MAX as u64 || !record[8..].iter().all(u8::is_ascii_hexdigit) {
                return Err(Damaged("invalid ANN chunk identity map".to_owned()));
            }
            previous = id;
        }
        if key_data.len() as u64 != manifest.keys.bytes {
            return Err(Damaged("invalid ANN chunk identity map length".to_owned()));
        }
        let mapped = Index::new(&options(binding.dimension)).map_err(|error| Damaged(error.to_string()))?;
        // SAFETY: This view stays inside Reader, which owns the immutable
        // mapping and drops the index before the mapped bytes.
        unsafe { mapped.view_from_buffer(&mapping) }.map_err(|error| Damaged(error.to_string()))?;
        if mapped.size() != binding.covered || mapped.dimensions() != binding.dimension || mapped.connectivity() != CONNECTIVITY
            || !matches!(mapped.metric_kind(), MetricKind::IP) || !matches!(mapped.scalar_kind(), ScalarKind::F16)
        {
            return Err(Damaged("ANN graph disagrees with its manifest".to_owned()));
        }
        mapped.change_expansion_search(EXPANSION_SEARCH);
        check_files(&manifest, &index, &keys)?;
        let reader = Arc::new(Self { index: mapped, _index_mapping: mapping, keys: key_data, manifest });
        if let Ok(mut cache) = READERS.lock() {
            cache.retain(|(path, _)| path.parent() != index.parent());
            cache.push_front((index, Arc::clone(&reader)));
            cache.truncate(CACHE_SIZE);
        }
        Ok(reader)
    }

    /// Built for exactly this snapshot and vector coverage.
    pub fn fresh_for(&self, binding: &Binding) -> bool {
        self.manifest.binding == *binding
    }

    pub fn hash(&self, id: u64) -> Option<&str> {
        // Fixed-width, ID-sorted records. Binary search without allocating a
        // second array of keys for the 820k-row UE graph.
        let (mut lo, mut hi) = (0, self.keys.len() / KEY_BYTES);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let record = &self.keys[mid * KEY_BYTES..(mid + 1) * KEY_BYTES];
            match key_id(record).cmp(&id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return std::str::from_utf8(&record[8..]).ok(),
            }
        }
        None
    }

    pub fn search(&self, query: &[f32]) -> Result<Vec<u64>, String> {
        self.index.search(query, SHORTLIST).map(|matches| matches.keys).map_err(|error| error.to_string())
    }

    pub fn filtered(&self, query: &[f32], keep: impl Fn(u64) -> bool) -> Result<Vec<u64>, String> {
        self.index.filtered_search(query, SHORTLIST, keep).map(|matches| matches.keys).map_err(|error| error.to_string())
    }
}

fn key_id(record: &[u8]) -> u64 {
    u64::from_le_bytes(record[..8].try_into().expect("fixed-width ANN key"))
}
