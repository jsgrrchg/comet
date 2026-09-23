//! Structural mutations execute on the workspace host, serialized against saves.
use super::*;
use zeron_proto::{
    MoveWorkspaceEntryRequest, WorkspaceMutationOutcome, WorkspaceMutationRejection as Reason,
};

type MutationResult<T> = Result<T, (Reason, String)>;

impl WorkspaceFiles {
    pub(super) fn mutation_gate(&self, checkout: &str) -> Arc<tokio::sync::RwLock<()>> {
        let mut gates = lock(&self.inner.mutation_gates);
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(checkout).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::RwLock::new(()));
        gates.insert(checkout.into(), Arc::downgrade(&gate));
        gate
    }

    pub async fn move_entry(
        &self,
        request: MoveWorkspaceEntryRequest,
    ) -> Result<WorkspaceMutationOutcome, WorkspaceFilesError> {
        let workspace = self.resolve_target(&request.target).await?;
        let gate = self
            .mutation_gate(&workspace.checkout_id)
            .write_owned()
            .await;
        let current = self.resolve_target(&request.target).await?;
        if request.expected_checkout_id.is_empty()
            || current != workspace
            || request.expected_checkout_id != workspace.checkout_id
        {
            return Ok(rejected(
                request.operation_id,
                Reason::WorkspaceChanged,
                "Workspace changed; refresh before moving",
            ));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_on_drop = CancelOnDrop::new(cancel.clone());
        let result = tokio::task::spawn_blocking(move || {
            let _gate = gate;
            match move_blocking(&workspace, &request, &cancel) {
                Ok(entry) => WorkspaceMutationOutcome::Applied {
                    operation_id: request.operation_id.clone(),
                    checkout_id: workspace.checkout_id,
                    change: WorkspaceFileChange {
                        operation_id: Some(request.operation_id),
                        kind: WorkspaceFileChangeKind::Renamed,
                        path: entry.path.clone(),
                        old_path: Some(request.source_path),
                    },
                    entry: Some(entry),
                },
                Err((reason, message)) => rejected(request.operation_id, reason, &message),
            }
        })
        .await
        .map_err(|e| WorkspaceFilesError::Io(format!("move worker failed: {e}")))?;
        cancel_on_drop.disarm();
        Ok(result)
    }
}

fn rejected(operation_id: String, reason: Reason, message: &str) -> WorkspaceMutationOutcome {
    WorkspaceMutationOutcome::Rejected {
        operation_id,
        reason,
        message: message.into(),
    }
}

fn domain_error(error: WorkspaceFilesError) -> (Reason, String) {
    let reason = match &error {
        WorkspaceFilesError::BadParams(_) => Reason::InvalidPath,
        WorkspaceFilesError::Authorization(_) => Reason::WorkspaceChanged,
        WorkspaceFilesError::NotFound(_) => Reason::SourceMissing,
        WorkspaceFilesError::Unsupported(_) => Reason::Unsupported,
        WorkspaceFilesError::Io(_) => Reason::InvalidDestination,
    };
    (reason, error.to_string())
}

fn io_error(error: std::io::Error) -> (Reason, String) {
    use std::io::ErrorKind;
    (
        match error.kind() {
            ErrorKind::AlreadyExists => Reason::DestinationExists,
            ErrorKind::NotFound => Reason::SourceMissing,
            ErrorKind::PermissionDenied => Reason::PermissionDenied,
            ErrorKind::Unsupported | ErrorKind::CrossesDevices => Reason::Unsupported,
            _ => Reason::InvalidDestination,
        },
        error.to_string(),
    )
}

/// Metadata revision is deliberately cheap; directory revisions are not recursive snapshots.
pub(super) fn revision(metadata: &std::fs::Metadata) -> String {
    let mut hash = Sha256::new();
    hash.update(format!(
        "{:?}:{:?}:{:?}:{}:{}",
        metadata.file_type(),
        metadata.modified().ok(),
        metadata.created().ok(),
        metadata.len(),
        metadata.permissions().readonly()
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        hash.update(format!(
            ":{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ));
    }
    hex(&hash.finalize())
}

pub(super) fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return metadata.file_attributes() & 0x400 != 0; // FILE_ATTRIBUTE_REPARSE_POINT, includes junctions.
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn source(
    root: &Path,
    path: &str,
    expected: &str,
    kind: WorkspaceEntryKind,
) -> MutationResult<(WorkspaceRelativePath, std::fs::Metadata)> {
    let relative = WorkspaceRelativePath::file(path).map_err(domain_error)?;
    // Require canonical wire spelling so identity/deduplication is unambiguous.
    if relative.wire_path() != path {
        return Err((
            Reason::InvalidPath,
            "Path must use canonical workspace-relative spelling".into(),
        ));
    }
    let parent = WorkspaceRelativePath(relative.as_path().parent().unwrap_or(Path::new("")).into());
    checked_directory(root, &parent).map_err(domain_error)?;
    let metadata = std::fs::symlink_metadata(root.join(relative.as_path())).map_err(io_error)?;
    if is_link(&metadata) || !(metadata.is_file() || metadata.is_dir()) {
        return Err((
            Reason::Unsupported,
            "Links and special files cannot be mutated".into(),
        ));
    }
    let actual_kind = if metadata.is_dir() {
        WorkspaceEntryKind::Directory
    } else {
        WorkspaceEntryKind::File
    };
    if actual_kind != kind || expected.is_empty() || revision(&metadata) != expected {
        return Err((
            Reason::SourceChanged,
            "Entry changed; refresh before trying again".into(),
        ));
    }
    Ok((relative, metadata))
}

fn move_blocking(
    workspace: &ResolvedWorkspace,
    request: &MoveWorkspaceEntryRequest,
    cancel: &AtomicBool,
) -> MutationResult<WorkspaceEntry> {
    if request.operation_id.is_empty() || request.operation_id.len() > 128 {
        return Err((Reason::InvalidPath, "Invalid operation identity".into()));
    }
    let (source, metadata) = source(
        &workspace.root,
        &request.source_path,
        &request.expected_source_revision,
        request.expected_kind,
    )?;
    let destination =
        WorkspaceRelativePath::file(&request.destination_path).map_err(domain_error)?;
    if destination.wire_path() != request.destination_path {
        return Err((Reason::InvalidPath, "Invalid destination spelling".into()));
    }
    if destination == source
        || (metadata.is_dir() && destination.as_path().starts_with(source.as_path()))
    {
        return Err((
            Reason::InvalidDestination,
            "Cannot move an entry into itself".into(),
        ));
    }
    let parent = WorkspaceRelativePath(
        destination
            .as_path()
            .parent()
            .unwrap_or(Path::new(""))
            .into(),
    );
    checked_directory(&workspace.root, &parent).map_err(domain_error)?;
    if cancel.load(Ordering::Acquire) {
        return Err((Reason::Busy, "Move cancelled before execution".into()));
    }
    move_no_replace(&workspace.root, source.as_path(), destination.as_path()).map_err(io_error)?;
    let target = workspace.root.join(destination.as_path());
    let updated = std::fs::symlink_metadata(&target).unwrap_or(metadata);
    Ok(WorkspaceEntry {
        path: destination.wire_path(),
        name: target.file_name().unwrap().to_string_lossy().into_owned(),
        kind: request.expected_kind,
        size: updated.is_file().then_some(updated.len()),
        modified_at: updated.modified().ok().map(chrono::DateTime::from),
        ignored: false,
        read_only: !updated.is_file(),
        mutation_revision: Some(revision(&updated)),
    })
}

/// Open every ancestor without following links, and anchor the native rename to those handles.
#[cfg(unix)]
fn open_parent(root: &Path, relative: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };
    let mut directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    for component in relative.parent().unwrap_or(Path::new("")).components() {
        let Component::Normal(name) = component else {
            return Err(std::io::ErrorKind::InvalidInput.into());
        };
        let name = std::ffi::CString::new(name.as_bytes())?;
        // SAFETY: a valid directory descriptor and NUL-terminated single path component.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat returned a newly owned descriptor.
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn move_no_replace(root: &Path, source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
    let from = open_parent(root, source)?;
    let to = open_parent(root, destination)?;
    let source = std::ffi::CString::new(
        source
            .file_name()
            .ok_or(std::io::ErrorKind::InvalidInput)?
            .as_bytes(),
    )?;
    let destination = std::ffi::CString::new(
        destination
            .file_name()
            .ok_or(std::io::ErrorKind::InvalidInput)?
            .as_bytes(),
    )?;
    // SAFETY: live parent handles and NUL-terminated basename buffers; replacement is disabled.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn move_no_replace(root: &Path, source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let from: Vec<u16> = root
        .join(source)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let to: Vec<u16> = root
        .join(destination)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: NUL-terminated paths; neither REPLACE_EXISTING nor COPY_ALLOWED is enabled.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn move_no_replace(_: &Path, _: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(root: &Path, path: &str, to: &str) -> MoveWorkspaceEntryRequest {
        let metadata = std::fs::symlink_metadata(root.join(path)).unwrap();
        MoveWorkspaceEntryRequest {
            target: WorkspaceTarget {
                chat_id: Some("chat".into()),
                space_id: None,
                checkout_path: None,
            },
            operation_id: "op".into(),
            expected_checkout_id: "checkout".into(),
            source_path: path.into(),
            destination_path: to.into(),
            expected_source_revision: revision(&metadata),
            expected_kind: if metadata.is_dir() {
                WorkspaceEntryKind::Directory
            } else {
                WorkspaceEntryKind::File
            },
        }
    }
    #[test]
    fn moves_subtrees_without_replacement_or_prefix_confusion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/child")).unwrap();
        std::fs::create_dir(root.join("ab")).unwrap();
        std::fs::write(root.join("a/child/á.txt"), "keep").unwrap();
        let workspace = ResolvedWorkspace {
            checkout_id: "checkout".into(),
            root: root.into(),
        };
        let cancel = AtomicBool::new(false);
        assert_eq!(
            move_blocking(&workspace, &request(root, "a", "a/child/new"), &cancel)
                .unwrap_err()
                .0,
            Reason::InvalidDestination
        );
        assert_eq!(
            move_blocking(&workspace, &request(root, "a", "ab"), &cancel)
                .unwrap_err()
                .0,
            Reason::DestinationExists
        );
        move_blocking(&workspace, &request(root, "a", "ab/a"), &cancel).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("ab/a/child/á.txt")).unwrap(),
            "keep"
        );
    }
    #[test]
    fn stale_sources_invalid_paths_and_cancel_do_not_mutate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a"), "old").unwrap();
        let workspace = ResolvedWorkspace {
            checkout_id: "checkout".into(),
            root: root.into(),
        };
        let req = request(root, "a", "b");
        std::fs::write(root.join("a"), "changed").unwrap();
        assert_eq!(
            move_blocking(&workspace, &req, &AtomicBool::new(false))
                .unwrap_err()
                .0,
            Reason::SourceChanged
        );
        for to in ["../b", ".git/file", "", "/tmp/outside"] {
            assert!(
                move_blocking(&workspace, &request(root, "a", to), &AtomicBool::new(false))
                    .is_err()
            );
        }
        assert_eq!(
            move_blocking(&workspace, &request(root, "a", "b"), &AtomicBool::new(true))
                .unwrap_err()
                .0,
            Reason::Busy
        );
        assert!(root.join("a").exists());
        assert!(!root.join("b").exists());
    }
    #[cfg(unix)]
    #[test]
    fn links_cannot_be_overwritten_or_traversed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.path().join("missing"), root.join("b")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        let ws = ResolvedWorkspace {
            checkout_id: "checkout".into(),
            root: root.into(),
        };
        assert_eq!(
            move_blocking(&ws, &request(root, "a", "b"), &AtomicBool::new(false))
                .unwrap_err()
                .0,
            Reason::DestinationExists
        );
        assert!(
            move_blocking(&ws, &request(root, "a", "link/a"), &AtomicBool::new(false)).is_err()
        );
        assert!(root.join("a").exists());
        assert!(!outside.path().join("a").exists());
    }
}
