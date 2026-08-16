//! Authorized filesystem access for the file tree and editor RPC surface.
//!
//! Every operation resolves a synced chat or space to a checkout owned by this
//! device before accepting a workspace-relative path.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use zeron_proto::{
    ListWorkspaceDirectoryRequest, SearchWorkspaceFilesRequest, WorkspaceDirectoryPage,
    WorkspaceEntry, WorkspaceEntryKind, WorkspaceFileSearchMatch, WorkspaceTarget,
};
use zeron_rpc::RpcError;

use crate::{Repos, WorkspaceHost};

const MAX_RELATIVE_PATH_BYTES: usize = 4096;
const MAX_RELATIVE_PATH_COMPONENTS: usize = 256;
pub const DIRECTORY_PAGE_SIZE: usize = 500;
pub const MAX_DIRECTORY_ENTRIES: usize = 50_000;
pub const MAX_SEARCH_QUERY_CHARS: usize = 256;
pub const MAX_SEARCH_RESULTS: usize = 200;
pub const WORKSPACE_FILE_RPC_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Clone)]
pub struct WorkspaceFiles {
    inner: Arc<WorkspaceFilesInner>,
}

struct WorkspaceFilesInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    write_locks: Mutex<HashMap<WorkspaceFileKey, Weak<tokio::sync::Mutex<()>>>>,
    watches: Mutex<HashMap<String, Arc<CheckoutWatch>>>,
    cancel: CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WorkspaceFileKey {
    checkout_id: String,
    path: PathBuf,
}

struct CheckoutWatch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedWorkspace {
    pub checkout_id: String,
    pub root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceFilesError {
    #[error("{0}")]
    BadParams(String),
    #[error("{0}")]
    Authorization(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Io(String),
}

impl From<WorkspaceFilesError> for RpcError {
    fn from(error: WorkspaceFilesError) -> Self {
        match error {
            WorkspaceFilesError::BadParams(message) => RpcError::BadParams(message),
            WorkspaceFilesError::Authorization(message)
            | WorkspaceFilesError::NotFound(message)
            | WorkspaceFilesError::Unsupported(message)
            | WorkspaceFilesError::Io(message) => RpcError::Failed(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceRelativePath(PathBuf);

impl WorkspaceRelativePath {
    pub fn directory(path: &str) -> Result<Self, WorkspaceFilesError> {
        Self::parse(path, true)
    }

    pub fn file(path: &str) -> Result<Self, WorkspaceFilesError> {
        Self::parse(path, false)
    }

    fn parse(path: &str, allow_root: bool) -> Result<Self, WorkspaceFilesError> {
        if path.is_empty() {
            return allow_root
                .then(|| Self(PathBuf::new()))
                .ok_or_else(|| bad_path("path must not be empty"));
        }
        if path.len() > MAX_RELATIVE_PATH_BYTES {
            return Err(bad_path("path is too long"));
        }
        if path.contains(['\0', '\\']) {
            return Err(bad_path("path contains an invalid character"));
        }
        if path.starts_with('/') || path.starts_with("//") || path.as_bytes().get(1) == Some(&b':')
        {
            return Err(bad_path("path must be workspace-relative"));
        }

        let parsed = Path::new(path);
        let mut count = 0usize;
        for component in parsed.components() {
            count += 1;
            if count > MAX_RELATIVE_PATH_COMPONENTS {
                return Err(bad_path("path has too many components"));
            }
            match component {
                Component::Normal(value) => {
                    let value = value
                        .to_str()
                        .ok_or_else(|| bad_path("path must be UTF-8"))?;
                    if value.eq_ignore_ascii_case(".git") {
                        return Err(bad_path(".git paths are not accessible"));
                    }
                }
                Component::CurDir | Component::ParentDir => {
                    return Err(bad_path("path must not contain . or .."));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(bad_path("path must be workspace-relative"));
                }
            }
        }
        Ok(Self(parsed.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn wire_path(&self) -> String {
        self.0
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => value.to_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/")
    }
}

fn bad_path(message: &str) -> WorkspaceFilesError {
    WorkspaceFilesError::BadParams(message.to_string())
}

impl WorkspaceFiles {
    pub fn new(repos: Repos, workspace: WorkspaceHost, device_id: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(WorkspaceFilesInner {
                repos,
                workspace,
                device_id: device_id.into(),
                write_locks: Mutex::new(HashMap::new()),
                watches: Mutex::new(HashMap::new()),
                cancel: CancellationToken::new(),
            }),
        }
    }

    pub(crate) async fn resolve_target(
        &self,
        target: &WorkspaceTarget,
    ) -> Result<ResolvedWorkspace, WorkspaceFilesError> {
        let root = match (&target.chat_id, &target.space_id) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(WorkspaceFilesError::BadParams(
                    "workspace target needs exactly one of chatId or spaceId".into(),
                ));
            }
            (Some(chat_id), None) => {
                if target.checkout_path.is_some() {
                    return Err(WorkspaceFilesError::BadParams(
                        "checkoutPath applies only to a space target".into(),
                    ));
                }
                let chat = self
                    .inner
                    .workspace
                    .chat(chat_id)
                    .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?
                    .ok_or_else(|| WorkspaceFilesError::NotFound("chat not found".into()))?;
                if chat.device_id != self.inner.device_id {
                    return Err(WorkspaceFilesError::Authorization(
                        "chat belongs to another device".into(),
                    ));
                }
                let cwd = chat.cwd.map(PathBuf::from).ok_or_else(|| {
                    WorkspaceFilesError::NotFound("chat has no workspace folder".into())
                })?;
                let space_id = chat.space_id.ok_or_else(|| {
                    WorkspaceFilesError::NotFound("chat has no workspace space".into())
                })?;
                let space = self
                    .inner
                    .workspace
                    .space(&space_id)
                    .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?
                    .ok_or_else(|| {
                        WorkspaceFilesError::NotFound("chat workspace space not found".into())
                    })?;
                if space.device_id != self.inner.device_id {
                    return Err(WorkspaceFilesError::Authorization(
                        "chat space belongs to another device".into(),
                    ));
                }
                self.inner
                    .repos
                    .workspace_checkout(Path::new(&space.path), &cwd)
                    .await
                    .ok_or_else(|| {
                        WorkspaceFilesError::Authorization(
                            "chat folder is not a workspace checkout".into(),
                        )
                    })?
            }
            (None, Some(space_id)) => {
                let space = self
                    .inner
                    .workspace
                    .space(space_id)
                    .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?
                    .ok_or_else(|| WorkspaceFilesError::NotFound("space not found".into()))?;
                if space.device_id != self.inner.device_id {
                    return Err(WorkspaceFilesError::Authorization(
                        "space belongs to another device".into(),
                    ));
                }
                let space_path = PathBuf::from(&space.path);
                let requested = target
                    .checkout_path
                    .as_deref()
                    .map_or_else(|| space_path.clone(), PathBuf::from);
                self.inner
                    .repos
                    .workspace_checkout(&space_path, &requested)
                    .await
                    .ok_or_else(|| {
                        WorkspaceFilesError::BadParams(
                            "checkoutPath is not a workspace checkout".into(),
                        )
                    })?
            }
        };

        let identity = self
            .inner
            .repos
            .checkout_identity(&root)
            .await
            .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
        Ok(ResolvedWorkspace {
            checkout_id: identity.id,
            root: identity.root,
        })
    }

    pub async fn list_directory(
        &self,
        request: ListWorkspaceDirectoryRequest,
    ) -> Result<WorkspaceDirectoryPage, WorkspaceFilesError> {
        let workspace = self.resolve_target(&request.target).await?;
        let directory = WorkspaceRelativePath::directory(&request.directory)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_on_drop = CancelOnDrop::new(cancel.clone());
        let root = workspace.root;
        let result = tokio::task::spawn_blocking(move || {
            list_directory_blocking(
                &root,
                &directory,
                request.include_ignored,
                request.cursor.as_deref(),
                &cancel,
            )
        })
        .await
        .map_err(|error| WorkspaceFilesError::Io(format!("directory worker failed: {error}")))?;
        cancel_on_drop.disarm();
        result
    }

    pub async fn search(
        &self,
        request: SearchWorkspaceFilesRequest,
    ) -> Result<Vec<WorkspaceFileSearchMatch>, WorkspaceFilesError> {
        if request.query.chars().count() > MAX_SEARCH_QUERY_CHARS {
            return Err(WorkspaceFilesError::BadParams(format!(
                "query must not exceed {MAX_SEARCH_QUERY_CHARS} characters"
            )));
        }
        let workspace = self.resolve_target(&request.target).await?;
        let limit =
            usize::from(request.limit.unwrap_or(MAX_SEARCH_RESULTS as u16)).min(MAX_SEARCH_RESULTS);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_on_drop = CancelOnDrop::new(cancel.clone());
        let result = tokio::task::spawn_blocking(move || {
            search_workspace_blocking(
                &workspace.root,
                &request.query,
                request.include_ignored,
                limit,
                &cancel,
            )
        })
        .await
        .map_err(|error| WorkspaceFilesError::Io(format!("search worker failed: {error}")))?;
        cancel_on_drop.disarm();
        result
    }

    /// Cancel all service-owned work. This operation is idempotent.
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        lock(&self.inner.watches).clear();
        lock(&self.inner.write_locks).clear();
    }
}

struct CancelOnDrop(Option<Arc<AtomicBool>>);

impl CancelOnDrop {
    fn new(cancel: Arc<AtomicBool>) -> Self {
        Self(Some(cancel))
    }

    fn disarm(mut self) {
        self.0.take();
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirectoryCursor {
    version: u8,
    directory: String,
    include_ignored: bool,
    offset: usize,
    fingerprint: String,
}

fn list_directory_blocking(
    root: &Path,
    directory: &WorkspaceRelativePath,
    include_ignored: bool,
    cursor: Option<&str>,
    cancel: &AtomicBool,
) -> Result<WorkspaceDirectoryPage, WorkspaceFilesError> {
    let target = checked_directory(root, directory)?;
    let visible_paths = include_ignored.then(|| filtered_directory_paths(root, &target));
    let mut builder = ignore::WalkBuilder::new(&target);
    builder.max_depth(Some(1)).follow_links(false).hidden(false);
    if include_ignored {
        builder.standard_filters(false);
    }

    let mut entries = Vec::new();
    let mut hard_truncated = false;
    for result in builder.build() {
        if cancel.load(Ordering::Relaxed) {
            return Err(WorkspaceFilesError::Io(
                "directory listing cancelled".into(),
            ));
        }
        let entry = match result {
            Ok(entry) => entry,
            Err(error) if entries.is_empty() => {
                return Err(WorkspaceFilesError::Io(error.to_string()));
            }
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| WorkspaceFilesError::Authorization("entry escaped workspace".into()))?;
        if contains_git_component(relative) {
            continue;
        }
        if entries.len() == MAX_DIRECTORY_ENTRIES {
            hard_truncated = true;
            break;
        }
        let metadata = match std::fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(WorkspaceFilesError::Io(error.to_string())),
        };
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            WorkspaceEntryKind::Symlink
        } else if file_type.is_dir() {
            WorkspaceEntryKind::Directory
        } else {
            WorkspaceEntryKind::File
        };
        let path = path_to_wire(relative)?;
        let ignored = visible_paths
            .as_ref()
            .is_some_and(|visible| !visible.contains(&path));
        entries.push(WorkspaceEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path,
            kind,
            size: file_type.is_file().then_some(metadata.len()),
            modified_at: metadata.modified().ok().map(chrono::DateTime::from),
            ignored,
            read_only: file_type.is_symlink() || !file_type.is_file(),
        });
    }

    entries.sort_by(|left, right| {
        entry_group(left.kind)
            .cmp(&entry_group(right.kind))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
    let fingerprint = directory_fingerprint(&entries);
    let offset = if let Some(cursor) = cursor {
        let cursor = decode_cursor(cursor)?;
        if cursor.version != 1
            || cursor.directory != directory.wire_path()
            || cursor.include_ignored != include_ignored
        {
            return Err(WorkspaceFilesError::BadParams(
                "directory cursor does not match this request; restart listing".into(),
            ));
        }
        if cursor.fingerprint != fingerprint {
            return Err(WorkspaceFilesError::BadParams(
                "directory changed between pages; restart listing".into(),
            ));
        }
        cursor.offset
    } else {
        0
    };
    if offset > entries.len() {
        return Err(WorkspaceFilesError::BadParams(
            "directory cursor is out of range; restart listing".into(),
        ));
    }
    let end = (offset + DIRECTORY_PAGE_SIZE).min(entries.len());
    let page_entries = entries[offset..end].to_vec();
    let next_cursor = (end < entries.len()).then(|| {
        encode_cursor(&DirectoryCursor {
            version: 1,
            directory: directory.wire_path(),
            include_ignored,
            offset: end,
            fingerprint,
        })
    });
    Ok(WorkspaceDirectoryPage {
        directory: directory.wire_path(),
        entries: page_entries,
        next_cursor,
        truncated: hard_truncated,
    })
}

fn filtered_directory_paths(root: &Path, target: &Path) -> HashSet<String> {
    let mut builder = ignore::WalkBuilder::new(target);
    builder.max_depth(Some(1)).follow_links(false).hidden(false);
    builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() == 1)
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .ok()
                .and_then(|path| path_to_wire(path).ok())
        })
        .collect()
}

fn search_workspace_blocking(
    root: &Path,
    query: &str,
    include_ignored: bool,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<Vec<WorkspaceFileSearchMatch>, WorkspaceFilesError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut builder = ignore::WalkBuilder::new(root);
    builder.follow_links(false).hidden(false);
    if include_ignored {
        builder.standard_filters(false);
    }
    let query_lower = query.to_lowercase();
    let mut matches = Vec::new();
    for result in builder.build() {
        if cancel.load(Ordering::Relaxed) {
            return Err(WorkspaceFilesError::Io("workspace search cancelled".into()));
        }
        let entry = match result {
            Ok(entry) => entry,
            Err(error) if matches.is_empty() => {
                return Err(WorkspaceFilesError::Io(error.to_string()));
            }
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue;
        }
        let relative = match entry.path().strip_prefix(root) {
            Ok(relative) if !contains_git_component(relative) => relative,
            _ => continue,
        };
        let file_type = match entry.file_type() {
            Some(file_type) => file_type,
            None => continue,
        };
        let kind = if file_type.is_symlink() {
            WorkspaceEntryKind::Symlink
        } else if file_type.is_dir() {
            WorkspaceEntryKind::Directory
        } else if file_type.is_file() {
            WorkspaceEntryKind::File
        } else {
            continue;
        };
        let path = path_to_wire(relative)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(score) = workspace_search_score(&name, &path, &query_lower) else {
            continue;
        };
        matches.push(WorkspaceFileSearchMatch {
            path,
            name,
            kind,
            score,
        });
    }
    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.to_lowercase().cmp(&right.path.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
    matches.truncate(limit);
    Ok(matches)
}

fn checked_directory(
    root: &Path,
    directory: &WorkspaceRelativePath,
) -> Result<PathBuf, WorkspaceFilesError> {
    let mut current = root.to_path_buf();
    for component in directory.as_path().components() {
        let Component::Normal(component) = component else {
            return Err(bad_path("invalid directory component"));
        };
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(WorkspaceFilesError::Unsupported(
                "symlink directories cannot be traversed".into(),
            ));
        }
        if !metadata.is_dir() {
            return Err(WorkspaceFilesError::Unsupported(
                "directory path is not a directory".into(),
            ));
        }
    }
    let canonical = std::fs::canonicalize(&current)
        .map_err(|error| WorkspaceFilesError::Io(error.to_string()))?;
    if !canonical.starts_with(root) {
        return Err(WorkspaceFilesError::Authorization(
            "directory escaped workspace".into(),
        ));
    }
    Ok(canonical)
}

fn entry_group(kind: WorkspaceEntryKind) -> u8 {
    match kind {
        WorkspaceEntryKind::Directory => 0,
        WorkspaceEntryKind::File => 1,
        WorkspaceEntryKind::Symlink => 2,
    }
}

fn contains_git_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, Component::Normal(value) if value.to_string_lossy().eq_ignore_ascii_case(".git"))
    })
}

fn path_to_wire(path: &Path) -> Result<String, WorkspaceFilesError> {
    path.components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| WorkspaceFilesError::Unsupported("path is not UTF-8".into())),
            _ => Err(bad_path("invalid relative path")),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|components| components.join("/"))
}

fn directory_fingerprint(entries: &[WorkspaceEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.path.as_bytes());
        hasher.update([0]);
        hasher.update([entry_group(entry.kind)]);
        hasher.update(entry.size.unwrap_or_default().to_le_bytes());
        hasher.update(
            entry
                .modified_at
                .map(|time| time.timestamp_nanos_opt().unwrap_or_default())
                .unwrap_or_default()
                .to_le_bytes(),
        );
    }
    hex(&hasher.finalize())
}

fn encode_cursor(cursor: &DirectoryCursor) -> String {
    let json = serde_json::to_vec(cursor).expect("directory cursor is serializable");
    hex(&json)
}

fn decode_cursor(cursor: &str) -> Result<DirectoryCursor, WorkspaceFilesError> {
    let bytes = decode_hex(cursor)
        .ok_or_else(|| WorkspaceFilesError::BadParams("invalid directory cursor".into()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| WorkspaceFilesError::BadParams("invalid directory cursor".into()))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

fn workspace_search_score(name: &str, path: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let name = name.to_lowercase();
    let path = path.to_lowercase();
    if name == query {
        return Some(10_000);
    }
    if name.starts_with(query) {
        return Some(8_000 - name.len() as i64);
    }
    if let Some(index) = name.find(query) {
        return Some(6_000 - index as i64 - name.len() as i64);
    }
    if let Some(index) = path.find(query) {
        return Some(4_000 - index as i64 - path.len() as i64);
    }
    let mut query_chars = query.chars();
    let mut wanted = query_chars.next()?;
    let mut gaps = 0i64;
    for character in path.chars() {
        if character == wanted {
            match query_chars.next() {
                Some(next) => wanted = next,
                None => return Some(2_000 - gaps - path.len() as i64),
            }
        } else {
            gaps += 1;
        }
    }
    None
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn relative_paths_preserve_normal_and_unicode_components() {
        let path = WorkspaceRelativePath::file("src/日本語/emoji-🛰️.rs").unwrap();
        assert_eq!(path.as_path(), Path::new("src/日本語/emoji-🛰️.rs"));
        assert_eq!(path.wire_path(), "src/日本語/emoji-🛰️.rs");
        assert_eq!(
            WorkspaceRelativePath::directory("").unwrap().as_path(),
            Path::new("")
        );
    }

    #[test]
    fn relative_paths_reject_unsafe_shapes() {
        for path in [
            "",
            "/tmp/file",
            "./file",
            "src/../file",
            "src\\file",
            "src\0file",
            "C:/file",
            "//server/share",
            ".git/config",
            "src/.GIT/config",
        ] {
            assert!(
                WorkspaceRelativePath::file(path).is_err(),
                "unsafe path accepted: {path:?}"
            );
        }
    }

    #[test]
    fn list_orders_directories_pages_and_detects_stale_cursors() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("Zoo")).unwrap();
        std::fs::create_dir(root.path().join("alpha")).unwrap();
        for index in 0..501 {
            std::fs::write(root.path().join(format!("file-{index:03}.txt")), b"x").unwrap();
        }
        let root = std::fs::canonicalize(root.path()).unwrap();
        let first = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            false,
            None,
            &no_cancel(),
        )
        .unwrap();
        assert_eq!(first.entries.len(), DIRECTORY_PAGE_SIZE);
        assert_eq!(first.entries[0].path, "alpha");
        assert_eq!(first.entries[1].path, "Zoo");
        let cursor = first.next_cursor.unwrap();

        let second = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            false,
            Some(&cursor),
            &no_cancel(),
        )
        .unwrap();
        assert_eq!(second.entries.len(), 3);
        assert!(second.next_cursor.is_none());

        std::fs::write(root.join("new.txt"), b"new").unwrap();
        let stale = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            false,
            Some(&cursor),
            &no_cancel(),
        )
        .unwrap_err();
        assert!(stale.to_string().contains("restart listing"));
    }

    #[test]
    fn list_honors_ignore_rules_but_never_exposes_git() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.path().join(".hidden"), b"hidden").unwrap();
        std::fs::write(root.path().join("ignored.txt"), b"ignored").unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/config"), b"secret").unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();

        let filtered = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            false,
            None,
            &no_cancel(),
        )
        .unwrap();
        assert!(filtered.entries.iter().any(|entry| entry.path == ".hidden"));
        assert!(
            !filtered
                .entries
                .iter()
                .any(|entry| entry.path == "ignored.txt")
        );

        let all = list_directory_blocking(
            &root,
            &WorkspaceRelativePath::directory("").unwrap(),
            true,
            None,
            &no_cancel(),
        )
        .unwrap();
        assert!(
            all.entries
                .iter()
                .any(|entry| entry.path == "ignored.txt" && entry.ignored)
        );
        assert!(
            !all.entries
                .iter()
                .any(|entry| entry.path == ".git" || entry.path.starts_with(".git/"))
        );
    }

    #[test]
    fn search_ranks_filename_and_nested_path_matches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/deep")).unwrap();
        std::fs::write(root.path().join("src/deep/config.rs"), b"").unwrap();
        std::fs::write(root.path().join("src/configuration.rs"), b"").unwrap();
        std::fs::write(root.path().join("README.md"), b"").unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();

        let matches = search_workspace_blocking(&root, "config", false, 200, &no_cancel()).unwrap();
        assert_eq!(matches[0].path, "src/deep/config.rs");
        assert!(
            matches
                .iter()
                .any(|entry| entry.path == "src/configuration.rs")
        );
        assert!(matches.len() <= MAX_SEARCH_RESULTS);
    }
}
