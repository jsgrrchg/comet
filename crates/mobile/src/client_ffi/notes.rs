//! Session notes over FFI: read, watch and compare-and-set the user's
//! scratchpad, plus the text helpers an editor needs to merge without
//! moving the caret. Offsets that cross here are UTF-16 code units, the
//! unit of `NSRange` / `UITextView`.

use std::sync::{Arc, Mutex};

use zeron_client as zc;

use super::session::SessionHandle;
use super::types::CoreResult;

/// The notes text as one viewport last saw it; `hash` is the base of the
/// next `set_notes`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NotesSnapshot {
    pub text: String,
    pub hash: String,
}

impl From<zc::NotesSnapshot> for NotesSnapshot {
    fn from(n: zc::NotesSnapshot) -> Self {
        Self {
            text: n.text,
            hash: n.hash,
        }
    }
}

/// What a `set_notes` did.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NotesWrite {
    /// The notes now read as written; `hash` is the new base.
    Applied { hash: String },
    /// They changed since the base: rebase onto `current` and retry.
    Conflict { current: NotesSnapshot },
    /// The write would grow them past `limit` bytes; nothing was written.
    TooLarge { limit: u64 },
}

impl From<zc::NotesWrite> for NotesWrite {
    fn from(w: zc::NotesWrite) -> Self {
        match w {
            zc::NotesWrite::Applied { hash } => Self::Applied { hash },
            zc::NotesWrite::Conflict { current } => Self::Conflict {
                current: current.into(),
            },
            zc::NotesWrite::TooLarge { limit } => Self::TooLarge {
                limit: limit as u64,
            },
        }
    }
}

/// Receives the notes at once, then on every change, local or remote.
/// Called on a client thread: hop to the main thread before touching UI.
#[uniffi::export(with_foreign)]
pub trait NotesListener: Send + Sync {
    fn on_notes(&self, notes: NotesSnapshot);
}

/// A live notes subscription. Stops on `cancel()` or when released.
#[derive(uniffi::Object)]
pub struct NotesSubscription {
    watch: Mutex<Option<zc::SnapshotWatch>>,
}

#[uniffi::export]
impl NotesSubscription {
    pub fn cancel(&self) {
        self.watch.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

#[uniffi::export]
impl SessionHandle {
    /// The session's notes as last published.
    pub fn notes(&self) -> NotesSnapshot {
        self.inner.notes().into()
    }

    pub fn watch_notes(&self, listener: Arc<dyn NotesListener>) -> Arc<NotesSubscription> {
        let watch = self
            .inner
            .watch_notes(move |notes| listener.on_notes(notes.into()));
        Arc::new(NotesSubscription {
            watch: Mutex::new(Some(watch)),
        })
    }

    /// Compare-and-set the notes against `base_hash`. Works offline: an
    /// applied write is durable at once and syncs when the room is reachable.
    pub fn set_notes(&self, base_hash: String, text: String) -> CoreResult<NotesWrite> {
        Ok(self.inner.set_notes(&base_hash, &text)?.into())
    }
}

/// One contiguous replacement in UTF-16 code units: `length` units at
/// `location` become `text`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TextReplacement {
    pub location: u64,
    pub length: u64,
    pub text: String,
}

/// The single replacement turning `old` into `new` (`None` when equal). Apply
/// it to the text storage in place so the caret and scroll position survive a
/// remote change.
#[uniffi::export]
pub fn notes_text_replacement(old: String, new: String) -> Option<TextReplacement> {
    let edit = zeron_doc::single_range_diff(&old, &new)?;
    Some(TextReplacement {
        location: utf16_len(&old[..edit.range.start]),
        length: utf16_len(&old[edit.range.clone()]),
        text: edit.insert,
    })
}

/// Merge the local edit (`base` → `local`) with a remote one (`base` →
/// `remote`). Both edits survive; see `zeron_doc::rebase_notes`.
#[uniffi::export]
pub fn notes_rebase(base: String, local: String, remote: String) -> String {
    zeron_doc::rebase_notes(&base, &local, &remote)
}

/// The base hash of `text`, for writes from a viewport that only has text.
#[uniffi::export]
pub fn notes_hash(text: String) -> String {
    zeron_doc::notes_hash(&text)
}

fn utf16_len(s: &str) -> u64 {
    s.encode_utf16().count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_counts_utf16_units() {
        // 😀 is two UTF-16 units; é is one.
        let r = notes_text_replacement("a😀é b".into(), "a😀é c b".into()).unwrap();
        assert_eq!(
            r,
            TextReplacement {
                location: 5,
                length: 0,
                text: "c ".into(),
            }
        );
        let r = notes_text_replacement("x😀y".into(), "xy".into()).unwrap();
        assert_eq!((r.location, r.length, r.text.as_str()), (1, 2, ""));
        assert_eq!(notes_text_replacement("same".into(), "same".into()), None);
    }

    #[test]
    fn rebase_and_hash_match_the_doc() {
        assert_eq!(
            notes_rebase("ab".into(), "aLb".into(), "aRb".into()),
            "aRLb"
        );
        assert_eq!(notes_hash(String::new()), zeron_doc::notes_hash(""));
    }
}
