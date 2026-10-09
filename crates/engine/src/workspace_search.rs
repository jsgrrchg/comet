//! Workspace search index: one `fff-search` file picker per working folder
//! (repo, worktree or plain folder), shared by `SearchFiles` (`@`),
//! `SearchWorkspaceFiles` (file tree) and `SearchWorkspaceContent` (cmd+K).
//!
//! An index is created by the first warm or search of its root, scans in the
//! background and keeps itself current with its own watcher. Memory stays
//! bounded: an index idle for [`IDLE_EVICT`] is dropped, at most [`MAX_LIVE`]
//! are alive at once (least recently used goes first), and the content cache
//! of each one is capped at [`CACHE_BUDGET_BYTES`]. A pinned index (the
//! focused chat's, renewed by the UI heartbeat) is evicted for capacity only
//! when every live index is pinned.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use fff_search::file_picker::FilePicker;
use fff_search::{
    Casing, ContentCacheBudget, FFFMode, FilePickerOptions, FuzzySearchOptions, GrepConfig,
    GrepMode, GrepSearchOptions, MixedItemRef, MixedSearchConfig, PaginationArgs, QueryParser,
    SharedFilePicker, SharedFrecency,
};

/// An index nobody searched or warmed for this long is dropped.
pub const IDLE_EVICT: Duration = Duration::from_secs(5 * 60);
/// Live indexes at once; creating one more evicts the least recently used.
pub const MAX_LIVE: usize = 3;
/// Per-index cap on cached file contents (fff defaults to up to 512 MB).
pub const CACHE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;
/// A pin outlives the UI's 60 s warm heartbeat by a comfortable margin, so a
/// client that disappears cannot keep its index alive forever.
pub const PIN_TTL: Duration = Duration::from_secs(3 * 60);
/// A search waits this long for the initial scan before answering from
/// whatever is indexed so far (flagged `indexing`).
pub const SCAN_WAIT: Duration = Duration::from_millis(300);
/// Interactive content searches return partial results past this budget.
pub const CONTENT_TIME_BUDGET: Duration = Duration::from_secs(2);
const REAPER_INTERVAL: Duration = Duration::from_secs(30);
/// Ceiling for logging a finished scan; a scan still running then is not logged.
const SCAN_LOG_WAIT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceSearchError {
    #[error("workspace search is unavailable: {0}")]
    Unavailable(String),
}

/// Whether a root's index has finished its initial scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    Building,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKinds {
    FilesAndDirectories,
    Files,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameMatch {
    /// Root-relative, `/`-separated.
    pub path: String,
    pub is_dir: bool,
    pub score: i64,
}

#[derive(Debug, Clone, Default)]
pub struct NameSearch {
    pub matches: Vec<NameMatch>,
    /// The initial scan had not finished; results may be incomplete.
    pub indexing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentMatch {
    /// Root-relative, `/`-separated.
    pub path: String,
    /// 1-based.
    pub line: u64,
    /// 0-based byte column of the first match in the original line.
    pub column: u64,
    /// The matched line, leading whitespace trimmed and capped at 512 bytes.
    pub preview: String,
    /// Byte ranges of the matches within `preview`.
    pub ranges: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Default)]
pub struct ContentSearch {
    pub matches: Vec<ContentMatch>,
    /// More matches exist past `limit`, or the time budget ran out.
    pub truncated: bool,
    pub indexing: bool,
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Clone)]
pub struct WorkspaceSearch {
    inner: Arc<Inner>,
}

struct Inner {
    entries: Mutex<HashMap<PathBuf, Entry>>,
    clock: Clock,
    reaper_started: std::sync::Once,
}

struct Entry {
    picker: SharedFilePicker,
    last_used: Instant,
    pinned_until: Option<Instant>,
}

impl Default for WorkspaceSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceSearch {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }

    fn with_clock(clock: Clock) -> Self {
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(HashMap::new()),
                clock,
                reaper_started: std::sync::Once::new(),
            }),
        }
    }

    /// Start (or keep alive) the index for `root`. `pin` protects it from
    /// capacity eviction for [`PIN_TTL`]; `pin: false` releases a pin.
    pub fn warm(&self, root: &Path, pin: bool) -> Result<IndexState, WorkspaceSearchError> {
        let picker = self.acquire(root, Some(pin))?;
        Ok(if picker.wait_for_scan(Duration::ZERO) {
            IndexState::Ready
        } else {
            IndexState::Building
        })
    }

    /// Fuzzy file (and directory) names. An empty query lists files by
    /// recency (git and modification time).
    pub fn search_names(
        &self,
        root: &Path,
        query: &str,
        limit: usize,
        kinds: NameKinds,
    ) -> Result<NameSearch, WorkspaceSearchError> {
        let picker = self.acquire(root, None)?;
        let indexing = !picker.wait_for_scan(SCAN_WAIT);
        if limit == 0 {
            return Ok(NameSearch {
                matches: Vec::new(),
                indexing,
            });
        }
        let guard = picker.read().map_err(unavailable)?;
        let Some(fff) = guard.as_ref() else {
            return Ok(NameSearch::default());
        };
        let options = FuzzySearchOptions {
            pagination: PaginationArgs { offset: 0, limit },
            ..Default::default()
        };
        let mut matches = Vec::with_capacity(limit);
        if kinds == NameKinds::Files || query.trim().is_empty() {
            let parsed = QueryParser::default().parse(query);
            let result = fff.fuzzy_search(&parsed, None, options);
            for (item, score) in result.items.into_iter().zip(result.scores) {
                matches.push(NameMatch {
                    path: item.relative_path(fff),
                    is_dir: false,
                    score: i64::from(score.total),
                });
            }
        } else {
            let parsed = QueryParser::new(MixedSearchConfig).parse(query);
            let result = fff.fuzzy_search_mixed(&parsed, None, options);
            for (item, score) in result.items.into_iter().zip(result.scores) {
                let (path, is_dir) = match item {
                    MixedItemRef::File(file) => (file.relative_path(fff), false),
                    MixedItemRef::Dir(dir) => {
                        let path = dir.relative_path(fff);
                        (path.trim_end_matches('/').to_owned(), true)
                    }
                };
                if path.is_empty() {
                    continue;
                }
                matches.push(NameMatch {
                    path,
                    is_dir,
                    score: i64::from(score.total),
                });
            }
        }
        matches.truncate(limit);
        Ok(NameSearch { matches, indexing })
    }

    /// Plain-text content search with smart case. Binary and oversized files
    /// are skipped; at most `per_file` matches come from any one file.
    pub fn search_content(
        &self,
        root: &Path,
        query: &str,
        limit: usize,
        per_file: usize,
    ) -> Result<ContentSearch, WorkspaceSearchError> {
        let picker = self.acquire(root, None)?;
        let indexing = !picker.wait_for_scan(SCAN_WAIT);
        if limit == 0 || per_file == 0 || query.trim().is_empty() {
            return Ok(ContentSearch {
                indexing,
                ..Default::default()
            });
        }
        let guard = picker.read().map_err(unavailable)?;
        let Some(fff) = guard.as_ref() else {
            return Ok(ContentSearch::default());
        };
        let parsed = QueryParser::new(GrepConfig).parse(query);
        let options = GrepSearchOptions {
            max_matches_per_file: per_file,
            casing: Some(Casing::Smart),
            page_limit: limit,
            mode: GrepMode::PlainText,
            time_budget_ms: CONTENT_TIME_BUDGET.as_millis() as u64,
            enforce_time_budget: true,
            ..Default::default()
        };
        let result = fff.grep(&parsed, &options);
        let mut truncated = result.next_file_offset != 0 || result.matches.len() > limit;
        let mut matches = Vec::with_capacity(result.matches.len().min(limit));
        for found in result.matches {
            if matches.len() == limit {
                truncated = true;
                break;
            }
            let Some(file) = result.files.get(found.file_index) else {
                continue;
            };
            let indent = found.line_content.len() - found.line_content.trim_start().len();
            let shift = indent as u32;
            matches.push(ContentMatch {
                path: file.relative_path(fff),
                line: found.line_number,
                column: found.col as u64,
                preview: found.line_content[indent..].to_owned(),
                ranges: found
                    .match_byte_offsets
                    .iter()
                    .map(|(start, end)| (start.saturating_sub(shift), end.saturating_sub(shift)))
                    .collect(),
            });
        }
        Ok(ContentSearch {
            matches,
            truncated,
            indexing,
        })
    }

    /// Number of live indexes.
    pub fn live(&self) -> usize {
        lock(&self.inner.entries).len()
    }

    /// Drop every index idle past [`IDLE_EVICT`] (the reaper runs this every
    /// [`REAPER_INTERVAL`]).
    pub fn evict_idle(&self) {
        let now = (self.inner.clock)();
        let expired: Vec<(PathBuf, Entry)> = {
            let mut entries = lock(&self.inner.entries);
            let roots: Vec<PathBuf> = entries
                .iter()
                .filter(|(_, entry)| now.saturating_duration_since(entry.last_used) >= IDLE_EVICT)
                .map(|(root, _)| root.clone())
                .collect();
            roots
                .into_iter()
                .filter_map(|root| entries.remove(&root).map(|entry| (root, entry)))
                .collect()
        };
        for (root, entry) in expired {
            release(&root, entry, "idle");
        }
    }

    fn acquire(
        &self,
        root: &Path,
        pin: Option<bool>,
    ) -> Result<SharedFilePicker, WorkspaceSearchError> {
        self.start_reaper();
        let now = (self.inner.clock)();
        let mut evicted = None;
        let picker = {
            let mut entries = lock(&self.inner.entries);
            if let Some(entry) = entries.get_mut(root) {
                entry.last_used = now;
                match pin {
                    Some(true) => entry.pinned_until = Some(now + PIN_TTL),
                    Some(false) => entry.pinned_until = None,
                    None => {}
                }
                entry.picker.clone()
            } else {
                if entries.len() >= MAX_LIVE {
                    evicted = lru_victim(&entries, now)
                        .and_then(|victim| entries.remove(&victim).map(|entry| (victim, entry)));
                }
                let picker = create_picker(root)?;
                entries.insert(
                    root.to_path_buf(),
                    Entry {
                        picker: picker.clone(),
                        last_used: now,
                        pinned_until: (pin == Some(true)).then(|| now + PIN_TTL),
                    },
                );
                picker
            }
        };
        if let Some((victim, entry)) = evicted {
            release(&victim, entry, "capacity");
        }
        Ok(picker)
    }

    fn start_reaper(&self) {
        let weak = Arc::downgrade(&self.inner);
        self.inner.reaper_started.call_once(move || {
            let spawned = std::thread::Builder::new()
                .name("workspace-search-reaper".into())
                .spawn(move || reap(weak));
            if let Err(error) = spawned {
                tracing::warn!(%error, "workspace search reaper did not start");
            }
        });
    }
}

fn reap(inner: Weak<Inner>) {
    loop {
        std::thread::sleep(REAPER_INTERVAL);
        let Some(inner) = inner.upgrade() else {
            return;
        };
        WorkspaceSearch { inner }.evict_idle();
    }
}

/// Least recently used unpinned index; the least recently used overall when
/// every index is pinned (the live cap holds regardless).
fn lru_victim(entries: &HashMap<PathBuf, Entry>, now: Instant) -> Option<PathBuf> {
    let pinned = |entry: &Entry| entry.pinned_until.is_some_and(|until| until > now);
    entries
        .iter()
        .filter(|(_, entry)| !pinned(entry))
        .min_by_key(|(_, entry)| entry.last_used)
        .or_else(|| entries.iter().min_by_key(|(_, entry)| entry.last_used))
        .map(|(root, _)| root.clone())
}

fn create_picker(root: &Path) -> Result<SharedFilePicker, WorkspaceSearchError> {
    let home = std::fs::canonicalize(crate::repos::home_dir()).is_ok_and(|home| home == root);
    let picker = SharedFilePicker::default();
    let started = Instant::now();
    FilePicker::new_with_shared_state(
        picker.clone(),
        SharedFrecency::noop(),
        FilePickerOptions {
            base_path: root.to_string_lossy().into_owned(),
            enable_mmap_cache: false,
            // The bigram index makes content search fast; a whole home folder
            // is too large to be worth it, so it is searched unindexed there.
            enable_content_indexing: !home,
            mode: FFFMode::Neovim,
            cache_budget: ContentCacheBudget::from_overrides(0, CACHE_BUDGET_BYTES, 0),
            watch: true,
            follow_symlinks: false,
            enable_fs_root_scanning: false,
            // Projectless chats live in the home folder; fff skips its dotfiles
            // and machine-state folders.
            enable_home_dir_scanning: true,
            ..Default::default()
        },
    )
    .map_err(unavailable)?;
    tracing::info!(root = %root.display(), "workspace search index created");
    let logged = picker.clone();
    let root_for_log = root.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("workspace-search-scan-log".into())
        .spawn(move || {
            if !logged.wait_for_scan(SCAN_LOG_WAIT) {
                return;
            }
            let Ok(guard) = logged.read() else {
                return;
            };
            if let Some(fff) = guard.as_ref() {
                tracing::info!(
                    root = %root_for_log.display(),
                    files = fff.live_file_count(),
                    scan_ms = started.elapsed().as_millis() as u64,
                    arena_bytes = fff.arena_bytes().0,
                    "workspace search index scanned"
                );
            }
        });
    Ok(picker)
}

/// Stop the index's scan and watcher and free it. The write lock waits for
/// in-flight searches, so this runs off the caller's thread.
fn release(root: &Path, entry: Entry, reason: &'static str) {
    let root = root.to_path_buf();
    let picker = entry.picker;
    picker.cancel();
    let _ = std::thread::Builder::new()
        .name("workspace-search-release".into())
        .spawn(move || {
            let Ok(mut guard) = picker.write() else {
                return;
            };
            if let Some(mut fff) = guard.take() {
                let files = fff.live_file_count();
                let arena_bytes = fff.arena_bytes().0;
                fff.stop_background_monitor();
                drop(fff);
                tracing::info!(
                    root = %root.display(),
                    reason,
                    files,
                    arena_bytes,
                    "workspace search index evicted"
                );
            }
        });
}

fn unavailable(error: impl std::fmt::Display) -> WorkspaceSearchError {
    WorkspaceSearchError::Unavailable(error.to_string())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ManualClock(Mutex<Instant>);

    impl ManualClock {
        fn advance(&self, by: Duration) {
            *lock(&self.0) += by;
        }
    }

    fn manual() -> (WorkspaceSearch, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock(Mutex::new(Instant::now())));
        let reader = clock.clone();
        (
            WorkspaceSearch::with_clock(Arc::new(move || *lock(&reader.0))),
            clock,
        )
    }

    fn folder(files: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let root = std::fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    fn ready(search: &WorkspaceSearch, root: &Path) -> SharedFilePicker {
        search.warm(root, false).unwrap();
        let picker = lock(&search.inner.entries)[root].picker.clone();
        assert!(picker.wait_for_indexing_complete(Duration::from_secs(20)));
        picker
    }

    fn names(search: &WorkspaceSearch, root: &Path, query: &str) -> Vec<String> {
        search
            .search_names(root, query, 20, NameKinds::FilesAndDirectories)
            .unwrap()
            .matches
            .into_iter()
            .map(|found| found.path)
            .collect()
    }

    #[test]
    fn creates_an_index_and_finds_files_and_directories() {
        let (_dir, root) = folder(&[("src/composer.rs", b""), ("README.md", b"")]);
        let search = WorkspaceSearch::new();
        ready(&search, &root);

        let found = search
            .search_names(&root, "composer", 20, NameKinds::FilesAndDirectories)
            .unwrap();
        assert!(!found.indexing);
        assert_eq!(found.matches[0].path, "src/composer.rs");
        assert!(!found.matches[0].is_dir);
        let dirs = search
            .search_names(&root, "src", 20, NameKinds::FilesAndDirectories)
            .unwrap();
        assert!(dirs.matches.iter().any(|m| m.path == "src" && m.is_dir));
        let files = search
            .search_names(&root, "src", 20, NameKinds::Files)
            .unwrap();
        assert!(files.matches.iter().all(|m| !m.is_dir));
        assert_eq!(search.live(), 1);
    }

    #[test]
    fn the_watcher_picks_up_new_files() {
        let (_dir, root) = folder(&[("alpha.rs", b"")]);
        let search = WorkspaceSearch::new();
        let picker = ready(&search, &root);
        assert!(picker.wait_for_watcher(Duration::from_secs(20)));
        assert!(!names(&search, &root, "beta").contains(&"beta.rs".to_owned()));

        std::fs::write(root.join("beta.rs"), b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !names(&search, &root, "beta").contains(&"beta.rs".to_owned()) {
            assert!(Instant::now() < deadline, "watcher never indexed beta.rs");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn idle_indexes_are_evicted() {
        let (_a, a) = folder(&[("a.rs", b"")]);
        let (_b, b) = folder(&[("b.rs", b"")]);
        let (search, clock) = manual();
        search.warm(&a, false).unwrap();
        clock.advance(IDLE_EVICT / 2);
        search.warm(&b, false).unwrap();
        clock.advance(IDLE_EVICT / 2);

        search.evict_idle();

        let entries = lock(&search.inner.entries);
        assert!(!entries.contains_key(&a));
        assert!(entries.contains_key(&b));
    }

    #[test]
    fn creating_past_the_cap_evicts_the_least_recently_used() {
        let folders: Vec<_> = (0..4).map(|_| folder(&[("x.rs", b"")])).collect();
        let (search, clock) = manual();
        for (_, root) in &folders[..3] {
            search.warm(root, false).unwrap();
            clock.advance(Duration::from_secs(1));
        }
        // Touching the oldest makes the second the least recently used.
        search.warm(&folders[0].1, false).unwrap();
        clock.advance(Duration::from_secs(1));

        search.warm(&folders[3].1, false).unwrap();

        let entries = lock(&search.inner.entries);
        assert_eq!(entries.len(), MAX_LIVE);
        assert!(!entries.contains_key(&folders[1].1));
    }

    #[test]
    fn a_pinned_index_survives_capacity_eviction() {
        let folders: Vec<_> = (0..4).map(|_| folder(&[("x.rs", b"")])).collect();
        let (search, clock) = manual();
        search.warm(&folders[0].1, true).unwrap();
        for (_, root) in &folders[1..] {
            clock.advance(Duration::from_secs(1));
            search.warm(root, false).unwrap();
        }

        let entries = lock(&search.inner.entries);
        assert_eq!(entries.len(), MAX_LIVE);
        assert!(entries.contains_key(&folders[0].1));
        assert!(!entries.contains_key(&folders[1].1));
    }

    #[test]
    fn content_search_skips_binaries_and_caps_matches_per_file() {
        let (_dir, root) = folder(&[
            (
                "notes.md",
                b"needle one\nneedle two\nneedle three\nneedle four\n",
            ),
            ("other.txt", b"  indented needle\n"),
            ("blob.bin", b"needle\0\0\0binary"),
        ]);
        let search = WorkspaceSearch::new();
        ready(&search, &root);

        let found = search.search_content(&root, "needle", 50, 3).unwrap();
        assert!(!found.matches.iter().any(|m| m.path == "blob.bin"));
        assert_eq!(
            found
                .matches
                .iter()
                .filter(|m| m.path == "notes.md")
                .count(),
            3
        );
        let other = found
            .matches
            .iter()
            .find(|m| m.path == "other.txt")
            .unwrap();
        assert_eq!(other.line, 1);
        assert_eq!(other.preview, "indented needle");
        assert_eq!(other.ranges, vec![(9, 15)]);
        assert_eq!(other.column, 11);
    }

    #[test]
    fn content_search_rejects_blank_queries_without_searching() {
        let (_dir, root) = folder(&[("notes.md", b"   \n")]);
        let search = WorkspaceSearch::new();
        ready(&search, &root);
        assert!(
            search
                .search_content(&root, "  ", 50, 3)
                .unwrap()
                .matches
                .is_empty()
        );
    }
}
