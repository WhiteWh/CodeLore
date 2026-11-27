//! Observe the files in a recorded build plan without walking that plan at
//! every worker tick. One workspace owns the subscription per shared cache.
use super::{path_key, store, Files, LoreError, Path, Plan, Source};
use crate::config;
use notify::event::{AccessKind, AccessMode, MetadataKind, ModifyKind};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

const RECONCILE: Duration = Duration::from_secs(5 * 60);
const RETRY_WATCH: Duration = Duration::from_secs(30);
const RETRY_SCAN: Duration = Duration::from_secs(2);

pub(super) struct ListedWatches {
    owned: BTreeMap<String, ListedWatch>,
    turn: usize,
    fallback: Option<Duration>,
}

impl Default for ListedWatches {
    fn default() -> Self {
        // The stdio regression drives the same bounded fallback when native
        // notifications cannot be delivered, without depending on the OS
        // faithfully losing a particular event.
        let fallback = std::env::var("LORE_ATLAS_TEST_LISTED_FALLBACK_SECS").ok()
            .and_then(|value| value.parse::<u64>().ok()).filter(|secs| *secs > 0)
            .map(Duration::from_secs);
        Self { owned: BTreeMap::new(), turn: 0, fallback }
    }
}

impl ListedWatches {
    pub fn retain(&mut self, keys: &BTreeSet<&str>) {
        self.owned.retain(|key, _| keys.contains(key.as_str()));
    }

    pub fn order(&mut self, plans: &[Plan]) -> Vec<usize> {
        let (mut listed, named): (Vec<_>, Vec<_>) = (0..plans.len())
            .partition(|&i| matches!(plans[i].files, Files::Listed(_)));
        if !listed.is_empty() {
            let len = listed.len();
            listed.rotate_left(self.turn % len);
            self.turn = (self.turn + 1) % len;
        }
        listed.extend(named);
        listed
    }

    pub fn due(&mut self, source: &Source, files: &BTreeSet<PathBuf>) -> Result<Option<&mut ListedWatch>, LoreError> {
        use std::collections::btree_map::Entry;
        let watch = match self.owned.entry(source.index_key.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let dir = source.directory();
                fs::create_dir_all(&dir).map_err(|error| LoreError::new(format!("{}: {error}", dir.display())))?;
                let lease = store::lock_file(&dir, "listed-watch.lock")?;
                match fs2::FileExt::try_lock_exclusive(&lease) {
                    Ok(()) => entry.insert(ListedWatch::new(lease)),
                    Err(error) if store::is_locked(&error) => return Ok(None),
                    Err(error) => return Err(LoreError::new(format!("{}: listed watcher lease: {error}", dir.display()))),
                }
            }
        };
        let now = Instant::now();
        if watch.lost.swap(false, Ordering::Relaxed) {
            watch.watcher = None;
            watch.dirty.store(true, Ordering::Relaxed);
        }
        if self.fallback.is_none() && watch.watcher.is_none() && now >= watch.next_attach {
            watch.watcher = attach(&source.canonical_path, files, &watch.dirty, &watch.lost).ok();
            watch.next_attach = now + RETRY_WATCH;
            if watch.watcher.is_some() {
                // Close the gap before a subscription or after a failed watch.
                watch.dirty.store(true, Ordering::Relaxed);
            }
        }
        Ok(watch.scan_due(now, self.fallback.unwrap_or(RECONCILE)).then_some(watch))
    }
}

pub(super) struct ListedWatch {
    watcher: Option<RecommendedWatcher>,
    _lease: File,
    dirty: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    last_scan: Option<Instant>,
    next_attach: Instant,
    next_scan: Instant,
}

impl ListedWatch {
    fn new(lease: File) -> Self {
        let now = Instant::now();
        Self { watcher: None, _lease: lease, dirty: Arc::new(AtomicBool::new(true)),
            lost: Arc::new(AtomicBool::new(false)), last_scan: None, next_attach: now, next_scan: now }
    }

    fn scan_due(&self, now: Instant, interval: Duration) -> bool {
        now >= self.next_scan && (self.dirty.swap(false, Ordering::Relaxed)
            || self.last_scan.is_none_or(|last| now.saturating_duration_since(last) >= interval))
    }

    pub fn finished(&mut self, scanned: bool) {
        let now = Instant::now();
        if scanned {
            self.last_scan = Some(now);
        } else {
            self.next_scan = now + RETRY_SCAN;
            self.dirty.store(true, Ordering::Relaxed);
        }
    }
}

fn attach(root: &Path, files: &BTreeSet<PathBuf>, dirty: &Arc<AtomicBool>, lost: &Arc<AtomicBool>) -> notify::Result<RecommendedWatcher> {
    // An observed container can resolve to a whole volume. Do not attach a
    // recursive subscription to an entire disk for a finite file list.
    let parent = root.parent().ok_or_else(|| notify::Error::generic("listed source spans a volume"))?;
    let root_key = path_key(root);
    let paths: BTreeSet<_> = files.iter().map(|path| path_key(path)).collect();
    let (dirty, lost) = (Arc::clone(dirty), Arc::clone(lost));
    let mut watcher = RecommendedWatcher::new(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            lost.store(true, Ordering::Relaxed);
            return;
        };
        if event.need_rescan() {
            lost.store(true, Ordering::Relaxed);
            return;
        }
        if matches!(event.kind, EventKind::Access(kind) if kind != AccessKind::Close(AccessMode::Write))
            || matches!(event.kind, EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)))
        {
            return;
        }
        let structural = matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)));
        for path in &event.paths {
            let key = path_key(&config::simplify(path));
            if structural && key == root_key {
                lost.store(true, Ordering::Relaxed);
            }
            if paths.contains(&key) || structural && contains_below(&paths, &key) {
                dirty.store(true, Ordering::Relaxed);
            }
        }
    }, notify::Config::default().with_follow_symlinks(false))?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
    Ok(watcher)
}

fn contains_below(paths: &BTreeSet<String>, key: &str) -> bool {
    let prefix = format!("{}/", key.trim_end_matches('/'));
    paths.range(prefix.clone()..).next().is_some_and(|path| path.starts_with(&prefix))
}
