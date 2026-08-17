//! Lifecycle and optimistic-concurrency state for an open workspace file.

use std::sync::Arc;

use gpui::{Entity, SharedString, Task};
use gpui_base::input::EditorState;
use zeron_proto::{
    WorkspaceFileText, WorkspaceLineEnding, WorkspaceReadOnlyReason, WorkspaceTextEncoding,
    WorkspaceWritableEncoding, WorkspaceWritableLineEnding,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct DocumentKey {
    pub chat_id: String,
    pub checkout_id: Option<String>,
    pub path: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DocumentPhase {
    Loading,
    Ready,
    Saving,
    SaveFailed(SharedString),
    ExternallyModified { disk_hash: Option<String> },
    Conflict { disk_hash: Option<String> },
    DeletedOnDisk,
    ReadOnly(WorkspaceReadOnlyReason),
    Error(SharedString),
}

#[allow(dead_code)]
pub(super) struct FileDocument {
    pub key: DocumentKey,
    pub generation: u64,
    pub phase: DocumentPhase,
    pub file: Option<WorkspaceFileText>,
    pub lines: Arc<Vec<SharedString>>,
    pub editor: Option<Entity<EditorState>>,
    pub loaded_hash: Option<String>,
    pub saved_hash: Option<String>,
    pub revision: u64,
    pub saved_revision: u64,
    pub encoding: Option<WorkspaceWritableEncoding>,
    pub line_ending: Option<WorkspaceWritableLineEnding>,
    pub read_task: Option<Task<()>>,
    pub highlight_task: Option<Task<()>>,
    pub autosave_task: Option<Task<()>>,
}

#[allow(dead_code)]
impl FileDocument {
    pub fn loading(key: DocumentKey) -> Self {
        Self {
            key,
            generation: 1,
            phase: DocumentPhase::Loading,
            file: None,
            lines: Arc::new(Vec::new()),
            editor: None,
            loaded_hash: None,
            saved_hash: None,
            revision: 0,
            saved_revision: 0,
            encoding: None,
            line_ending: None,
            read_task: None,
            highlight_task: None,
            autosave_task: None,
        }
    }

    pub fn begin_load(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.phase = DocumentPhase::Loading;
        self.read_task = None;
        self.highlight_task = None;
        self.autosave_task = None;
        self.generation
    }

    pub fn accepts(&self, key: &DocumentKey, generation: u64) -> bool {
        self.key == *key && self.generation == generation
    }

    pub fn set_loaded(&mut self, file: WorkspaceFileText) {
        let hash = file.content_hash.clone();
        self.editor = None;
        self.encoding = writable_encoding(file.encoding);
        self.line_ending = writable_line_ending(file.line_ending);
        self.lines = Arc::new(
            file.text
                .as_deref()
                .unwrap_or_default()
                .split('\n')
                .map(|line| SharedString::from(line.to_string()))
                .collect(),
        );
        self.phase = read_only_reason(&file)
            .map(DocumentPhase::ReadOnly)
            .unwrap_or(DocumentPhase::Ready);
        self.loaded_hash = hash.clone();
        self.saved_hash = hash;
        self.revision = 0;
        self.saved_revision = 0;
        self.file = Some(file);
    }

    pub fn set_error(&mut self, error: impl Into<SharedString>) {
        self.phase = DocumentPhase::Error(error.into());
    }

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub fn is_editable(&self) -> bool {
        matches!(self.phase, DocumentPhase::Ready | DocumentPhase::Saving)
            && self.file.as_ref().is_some_and(|file| file.text.is_some())
            && self.saved_hash.is_some()
            && self.encoding.is_some()
            && self.line_ending.is_some()
    }

    pub fn can_autosave(&self) -> bool {
        self.is_editable() && self.is_dirty() && !matches!(self.phase, DocumentPhase::Saving)
    }

    pub fn mark_user_edit(&mut self) {
        if self.is_editable() {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn content_hash(&self) -> Option<&str> {
        self.file.as_ref()?.content_hash.as_deref()
    }
}

fn read_only_reason(file: &WorkspaceFileText) -> Option<WorkspaceReadOnlyReason> {
    file.read_only_reason.or_else(|| {
        (file.truncated
            || file.text.is_none()
            || file.content_hash.is_none()
            || writable_encoding(file.encoding).is_none()
            || writable_line_ending(file.line_ending).is_none())
        .then_some(WorkspaceReadOnlyReason::NotRegularFile)
    })
}

fn writable_encoding(encoding: WorkspaceTextEncoding) -> Option<WorkspaceWritableEncoding> {
    match encoding {
        WorkspaceTextEncoding::Utf8 => Some(WorkspaceWritableEncoding::Utf8),
        WorkspaceTextEncoding::Utf8Bom => Some(WorkspaceWritableEncoding::Utf8Bom),
        WorkspaceTextEncoding::Binary | WorkspaceTextEncoding::Unsupported => None,
    }
}

fn writable_line_ending(
    line_ending: Option<WorkspaceLineEnding>,
) -> Option<WorkspaceWritableLineEnding> {
    match line_ending {
        Some(WorkspaceLineEnding::Lf | WorkspaceLineEnding::None) => {
            Some(WorkspaceWritableLineEnding::Lf)
        }
        Some(WorkspaceLineEnding::Crlf) => Some(WorkspaceWritableLineEnding::Crlf),
        Some(WorkspaceLineEnding::Mixed) | None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::WorkspaceTextEncoding;

    fn key(path: &str) -> DocumentKey {
        DocumentKey {
            chat_id: "chat-1".into(),
            checkout_id: Some("checkout-1".into()),
            path: path.into(),
        }
    }

    fn text_file() -> WorkspaceFileText {
        WorkspaceFileText {
            path: "src/lib.rs".into(),
            text: Some("fn main() {}".into()),
            content_hash: Some("hash-1".into()),
            size: 12,
            modified_at: None,
            encoding: WorkspaceTextEncoding::Utf8,
            line_ending: Some(WorkspaceLineEnding::Lf),
            read_only_reason: None,
            truncated: false,
        }
    }

    #[test]
    fn programmatic_load_is_clean_and_user_edits_use_revisions() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(text_file());
        assert!(!document.is_dirty());
        assert!(document.is_editable());

        document.mark_user_edit();
        assert!(document.is_dirty());
        assert_eq!(document.revision, 1);
    }

    #[test]
    fn stale_key_or_generation_is_rejected() {
        let mut document = FileDocument::loading(key("src/lib.rs"));
        let generation = document.generation;
        assert!(document.accepts(&key("src/lib.rs"), generation));
        document.begin_load();
        assert!(!document.accepts(&key("src/lib.rs"), generation));
        assert!(!document.accepts(&key("src/main.rs"), document.generation));
    }

    #[test]
    fn unsupported_and_truncated_files_are_read_only() {
        let mut unsupported = text_file();
        unsupported.encoding = WorkspaceTextEncoding::Unsupported;
        unsupported.read_only_reason = Some(WorkspaceReadOnlyReason::UnsupportedEncoding);
        let mut document = FileDocument::loading(key("src/lib.rs"));
        document.set_loaded(unsupported);
        assert!(matches!(
            document.phase,
            DocumentPhase::ReadOnly(WorkspaceReadOnlyReason::UnsupportedEncoding)
        ));
        assert!(!document.is_editable());

        let mut truncated = text_file();
        truncated.truncated = true;
        document.set_loaded(truncated);
        assert!(matches!(document.phase, DocumentPhase::ReadOnly(_)));
    }
}
