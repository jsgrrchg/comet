//! Authorized filesystem access for the file tree and editor RPC surface.
//!
//! Every operation resolves a synced chat or space to a checkout owned by this
//! device before accepting a workspace-relative path.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use tokio_util::sync::CancellationToken;
use zeron_proto::WorkspaceTarget;
use zeron_rpc::RpcError;

use crate::{Repos, WorkspaceHost};

const MAX_RELATIVE_PATH_BYTES: usize = 4096;
const MAX_RELATIVE_PATH_COMPONENTS: usize = 256;

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

    /// Cancel all service-owned work. This operation is idempotent.
    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        lock(&self.inner.watches).clear();
        lock(&self.inner.write_locks).clear();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
