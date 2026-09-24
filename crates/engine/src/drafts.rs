//! Durable content cache + publication outbox. A revision is published only
//! after its immutable content and every referenced asset have reached edge.
use crate::{EdgeConfig, EngineError};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use zeron_proto::*;
use zeron_sync::DocsStore;
const OUTBOX: &str = "prompt-drafts-outbox-v1";

#[derive(Clone)]
pub struct DraftStore {
    store: Arc<DocsStore>,
    edge: Option<EdgeConfig>,
    org: String,
    http: reqwest::Client,
}
fn error(s: impl ToString) -> EngineError {
    EngineError::Other(s.to_string())
}
impl DraftStore {
    pub fn new(store: Arc<DocsStore>, edge: Option<EdgeConfig>, org: String) -> Self {
        Self {
            store,
            edge,
            org,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client"),
        }
    }
    fn key(id: &str) -> String {
        format!("draft-blob:{id}")
    }
    pub fn stage(&self, mut draft: SaveDraft) -> Result<SaveDraft, EngineError> {
        if !valid_draft_id(&draft.id)
            || !valid_draft_id(&draft.revision)
            || draft
                .base_revision
                .as_ref()
                .is_some_and(|id| !valid_draft_id(id))
        {
            return Err(error("Invalid draft ID"));
        }
        if draft.content.attachments.len() > 32 {
            return Err(error("Too many draft attachments"));
        }
        let bytes = serde_json::to_vec(&draft.content).map_err(error)?;
        if bytes.len() > MAX_DRAFT_CONTENT_BYTES {
            return Err(error("Draft content is too large"));
        }
        for asset in &draft.assets {
            let bytes = STANDARD.decode(&asset.data).map_err(error)?;
            if bytes.len() > MAX_DRAFT_ASSET_BYTES
                || format!("{:x}", Sha256::digest(&bytes)) != asset.blob
            {
                return Err(error("Invalid draft attachment"));
            }
            self.store.save_snapshot(&Self::key(&asset.blob), &bytes)?;
        }
        for asset in &draft.content.attachments {
            if !valid_blob(&asset.blob) || !self.store.has_snapshot(&Self::key(&asset.blob))? {
                return Err(error("Draft attachment is unavailable"));
            }
        }
        // Never silently overwrite an immutable revision on a retried request.
        if let Some(old) = self.store.load_snapshot(&Self::key(&draft.revision))? {
            if old != bytes {
                return Err(error("Draft revision already contains different content"));
            }
        }
        self.store
            .save_snapshot(&Self::key(&draft.revision), &bytes)?;
        draft.assets.clear();
        self.store.enqueue_chat_update(
            OUTBOX,
            &draft.revision,
            &serde_json::to_vec(&draft).map_err(error)?,
        )?;
        Ok(draft)
    }
    pub fn pending(&self) -> Result<Vec<SaveDraft>, EngineError> {
        self.store
            .pending_chat_updates(OUTBOX)?
            .into_iter()
            .map(|(_, b)| serde_json::from_slice(&b).map_err(error))
            .collect()
    }
    pub fn acknowledge(&self, revision: &str) -> Result<(), EngineError> {
        self.store.acknowledge_chat_update(OUTBOX, revision)?;
        Ok(())
    }
    async fn object(&self, id: &str, put: Option<Vec<u8>>) -> Result<Vec<u8>, EngineError> {
        if !valid_draft_id(id) {
            return Err(error("Invalid draft object"));
        }
        let edge = self
            .edge
            .as_ref()
            .ok_or_else(|| error("Draft content is not cached on this device"))?;
        let url = format!(
            "{}/draft-content/{}/{}",
            edge.url.trim_end_matches('/'),
            self.org,
            id
        );
        let request = if let Some(bytes) = put {
            self.http.put(url).body(bytes)
        } else {
            self.http.get(url)
        };
        let response = request
            .bearer_auth(edge.token.token().await?)
            .send()
            .await
            .map_err(error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(error(format!("Draft content: HTTP {status}")));
        }
        let bytes = response.bytes().await.map_err(error)?;
        if bytes.len() > MAX_DRAFT_ASSET_BYTES {
            return Err(error("Draft object is too large"));
        }
        Ok(bytes.to_vec())
    }
    async fn read(&self, id: &str) -> Result<Vec<u8>, EngineError> {
        if !valid_draft_id(id) {
            return Err(error("Invalid draft object"));
        }
        if let Some(bytes) = self.store.load_snapshot(&Self::key(id))? {
            return Ok(bytes);
        }
        let bytes = self.object(id, None).await?;
        if valid_blob(id) && format!("{:x}", Sha256::digest(&bytes)) != id {
            return Err(error("Draft attachment checksum mismatch"));
        }
        self.store.save_snapshot(&Self::key(id), &bytes)?;
        Ok(bytes)
    }
    pub async fn load(&self, revision: &str) -> Result<DraftBundle, EngineError> {
        let content: DraftContent =
            serde_json::from_slice(&self.read(revision).await?).map_err(error)?;
        let mut assets = Vec::new();
        for a in &content.attachments {
            assets.push(DraftAsset {
                blob: a.blob.clone(),
                data: STANDARD.encode(self.read(&a.blob).await?),
            });
        }
        Ok(DraftBundle { content, assets })
    }
    pub async fn upload(&self, draft: &SaveDraft) -> Result<(), EngineError> {
        if self.edge.is_none() {
            return Ok(());
        }
        for id in draft
            .content
            .attachments
            .iter()
            .map(|a| a.blob.as_str())
            .chain(std::iter::once(draft.revision.as_str()))
        {
            // Asset ACKs are durable too: keystrokes never upload the same image again.
            let marker = format!("draft-uploaded:{id}");
            if self.store.has_snapshot(&marker)? {
                continue;
            }
            self.object(id, Some(self.read(id).await?)).await?;
            self.store.save_snapshot(&marker, b"1")?;
        }
        Ok(())
    }
}
fn valid_blob(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn draft_outbox_and_large_content_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(dir.path()).unwrap());
        let drafts = DraftStore::new(store, None, "local".into());
        let draft = SaveDraft {
            id: "draft".into(),
            revision: "revision".into(),
            base_revision: None,
            created_at: 1,
            content: DraftContent {
                prompt: "large prompt ".repeat(4096),
                ..Default::default()
            },
            assets: vec![],
        };
        drafts.stage(draft.clone()).unwrap();
        drop(drafts);
        let drafts = DraftStore::new(
            Arc::new(DocsStore::open(dir.path()).unwrap()),
            None,
            "local".into(),
        );
        assert_eq!(drafts.pending().unwrap(), vec![draft.clone()]);
        assert_eq!(
            drafts.load("revision").await.unwrap().content,
            draft.content
        );
        let mut changed = draft;
        changed.content.prompt = "different".into();
        assert!(drafts.stage(changed).is_err());
        drafts.acknowledge("revision").unwrap();
        assert!(drafts.pending().unwrap().is_empty());
    }
}
