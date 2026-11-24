//! One event subscription per shared source/profile. The watcher lease is
//! separate from the SQLite writer lock and lasts across quiet periods.
use super::{config, scan, store, AtlasSource, LoreError, Path, Source};
use notify::event::{AccessKind, AccessMode, ModifyKind};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

const RECONCILE: Duration = Duration::from_secs(5 * 60);
const RETRY_WATCH: Duration = Duration::from_secs(30);
const RETRY_SCAN: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(in crate::atlas::dependencies) struct TreeWatches {
    owned: BTreeMap<String, TreeWatch>,
}

impl TreeWatches {
    pub fn retain(&mut self, keys: &BTreeSet<&str>) {
        self.owned.retain(|key, _| keys.contains(key.as_str()));
    }

    pub fn due(&mut self, source: &Source, profile: &AtlasSource) -> Result<Option<&mut TreeWatch>, LoreError> {
        use std::collections::btree_map::Entry;
        let watch = match self.owned.entry(source.index_key.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let dir = source.directory();
                fs::create_dir_all(&dir).map_err(|error| LoreError::new(format!("{}: {error}", dir.display())))?;
                let lease = store::lock_file(&dir, "tree-watch.lock")?;
                match fs2::FileExt::try_lock_exclusive(&lease) {
                    Ok(()) => entry.insert(TreeWatch::new(lease)),
                    Err(error) if store::is_locked(&error) => return Ok(None),
                    Err(error) => return Err(LoreError::new(format!("{}: external watcher lease: {error}", dir.display()))),
                }
            }
        };
        Ok(watch.due(&source.canonical_path, profile).then_some(watch))
    }
}

pub(in crate::atlas::dependencies) struct TreeWatch {
    watcher: Option<RecommendedWatcher>,
    _lease: File,
    dirty: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    last_scan: Option<Instant>,
    next_attach: Instant,
    next_scan: Instant,
}

impl TreeWatch {
    fn new(lease: File) -> Self {
        let now = Instant::now();
        Self {
            watcher: None, _lease: lease, dirty: Arc::new(AtomicBool::new(true)),
            lost: Arc::new(AtomicBool::new(false)), last_scan: None, next_attach: now, next_scan: now,
        }
    }

    fn due(&mut self, root: &Path, profile: &AtlasSource) -> bool {
        let now = Instant::now();
        if self.lost.swap(false, Ordering::Relaxed) && self.watcher.take().is_some() {
            self.dirty.store(true, Ordering::Relaxed);
        }
        if self.watcher.is_none() && now >= self.next_attach {
            self.watcher = attach(root, profile, &self.dirty, &self.lost).ok();
            self.next_attach = now + RETRY_WATCH;
            if self.watcher.is_some() {
                // Changes made before subscription, including during a handoff,
                // are covered by the initial reconciliation.
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
        now >= self.next_scan && (self.dirty.swap(false, Ordering::Relaxed)
            || self.last_scan.is_none_or(|last| last.elapsed() >= RECONCILE))
    }

    pub fn finished(&mut self, scanned: bool) {
        if scanned {
            self.last_scan = Some(Instant::now());
        } else {
            self.next_scan = Instant::now() + RETRY_SCAN;
            self.dirty.store(true, Ordering::Relaxed);
        }
    }
}

fn attach(root: &Path, profile: &AtlasSource, dirty: &Arc<AtomicBool>, lost: &Arc<AtomicBool>) -> notify::Result<RecommendedWatcher> {
    let cache = crate::index::cache_dir().canonicalize().ok().map(|path| config::simplify(&path));
    let watched = root.to_path_buf();
    let (exclude, dirty, lost) = (profile.exclude.clone(), Arc::clone(dirty), Arc::clone(lost));
    let mut watcher = RecommendedWatcher::new(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            lost.store(true, Ordering::Relaxed);
            return;
        };
        if event.need_rescan() {
            dirty.store(true, Ordering::Relaxed);
            return;
        }
        if matches!(event.kind, EventKind::Access(access) if access != AccessKind::Close(AccessMode::Write)) {
            return;
        }
        for path in &event.paths {
            let path = config::simplify(path);
            if cache.as_ref().is_some_and(|cache| path.starts_with(cache)) {
                continue;
            }
            let Ok(relative) = path.strip_prefix(&watched) else { continue };
            // Directory metadata notifications can follow a read on Windows.
            // Changes to directory entries have their own structural events.
            if matches!(event.kind, EventKind::Modify(kind) if !matches!(kind, ModifyKind::Name(_)))
                && fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir())
            {
                continue;
            }
            if relative.as_os_str().is_empty()
                && matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)))
            {
                // Some native watches keep following the old directory inode.
                // The parent subscription identifies changes to the root name.
                lost.store(true, Ordering::Relaxed);
            }
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !scan::default_excluded(&relative) && !scan::excluded(&relative, &exclude) {
                dirty.store(true, Ordering::Relaxed);
            }
        }
    }, notify::Config::default().with_follow_symlinks(false))?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    if let Some(parent) = root.parent() {
        let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
    }
    Ok(watcher)
}
