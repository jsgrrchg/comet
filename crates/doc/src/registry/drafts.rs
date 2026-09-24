//! Small replicated draft indexes. Immutable revision rows preserve concurrent
//! edits: every unreferenced head is recoverable, even if another writer won
//! the current-revision register. Moving a row never writes content or liveness.
use super::*;
use zeron_proto::ordering::{order_key_between, valid_order_key};
use zeron_proto::{DraftChange, DraftTarget, PromptDraft, SaveDraft, valid_draft_id};
const DRAFTS: &str = "promptDrafts";
const REVISIONS: &str = "draftRevisions";

impl RegistryDoc {
    pub fn set_draft_order(&mut self, id: &str, key: &str) -> Result<(), DocError> {
        if !valid_draft_id(id) || !valid_order_key(key) {
            return Err(DocError::Schema("Invalid draft order".into()));
        }
        self.observe_sidebar_row(DRAFTS, id);
        self.write(
            DRAFTS,
            id,
            OpKind::Upsert,
            fields([("orderKey", json!(key))]),
        );
        Ok(())
    }

    pub fn draft_closed(&self, id: &str) -> bool {
        self.overlay_row(DRAFTS, id)
            .is_some_and(|r| r.fields.get("closed").and_then(Value::as_bool) == Some(true))
    }
    pub fn publish_draft(&mut self, draft: &SaveDraft) -> Result<(), DocError> {
        if !valid_draft_id(&draft.id) || !valid_draft_id(&draft.revision) {
            return Err(DocError::Schema("Invalid draft ID".into()));
        }
        if self.draft_closed(&draft.id) || self.overlay_row(REVISIONS, &draft.revision).is_some() {
            return Ok(());
        }
        let first = self.read_drafts().first().map(|d| d.order_key.clone());
        let stamp = self.next_hlc();
        let mut index = fields([("revision", json!(draft.revision))]);
        if self.overlay_row(DRAFTS, &draft.id).is_none() {
            let order = order_key_between(None, first.as_deref(), &stamp)
                .map_err(|s| DocError::Schema(s.into()))?;
            index.insert("orderKey".into(), json!(order));
        }
        // One batch: no watcher can see an index pointing at a missing revision.
        self.enqueue_ops(vec![
            RowOp {
                kind: REVISIONS.into(),
                id: draft.revision.clone(),
                op: OpKind::Upsert,
                hlc: stamp.clone(),
                clocks: None,
                set: Some(fields([
                    ("draftId", json!(draft.id)),
                    ("baseRevision", json!(draft.base_revision)),
                    ("createdAt", json!(draft.created_at)),
                    ("preview", json!(draft.content.preview())),
                    ("target", json!(draft.content.target)),
                ])),
            },
            RowOp {
                kind: DRAFTS.into(),
                id: draft.id.clone(),
                op: OpKind::Upsert,
                hlc: stamp,
                clocks: None,
                set: Some(index),
            },
        ]);
        Ok(())
    }
    pub fn read_drafts(&self) -> Vec<PromptDraft> {
        let revisions = self.overlay_rows(REVISIONS);
        let parents: std::collections::HashSet<_> = revisions
            .iter()
            .filter_map(|r| r.fields.get("baseRevision").and_then(Value::as_str))
            .collect();
        let mut result = Vec::new();
        for row in &revisions {
            if parents.contains(row.id.as_str()) {
                continue;
            }
            let Some(root) = row.fields.get("draftId").and_then(Value::as_str) else {
                continue;
            };
            if self.draft_closed(root) {
                continue;
            }
            let index = self.overlay_row(DRAFTS, root);
            let current = index
                .as_ref()
                .and_then(|r| r.fields.get("revision"))
                .and_then(Value::as_str);
            let conflict = current != Some(row.id.as_str());
            let id = if conflict { row.id.as_str() } else { root };
            if self.draft_closed(id) {
                continue;
            }
            let order = self.overlay_row(DRAFTS, id).or(index).and_then(|r| {
                r.fields
                    .get("orderKey")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
            let Some(order_key) = order.filter(|k| valid_order_key(k)) else {
                continue;
            };
            let Some(target) = row
                .fields
                .get("target")
                .and_then(|v| serde_json::from_value::<DraftTarget>(v.clone()).ok())
            else {
                continue;
            };
            result.push(PromptDraft {
                id: id.into(),
                revision: row.id.clone(),
                base_revision: row
                    .fields
                    .get("baseRevision")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                created_at: row
                    .fields
                    .get("createdAt")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                preview: row
                    .fields
                    .get("preview")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                target,
                order_key,
                conflict,
                pending: false,
            });
        }
        let keys: Vec<_> = result.iter().map(|d| d.order_key.clone()).collect();
        for row in &mut result {
            if row.conflict && self.overlay_row(DRAFTS, &row.id).is_none() {
                let upper = keys.iter().filter(|key| *key > &row.order_key).min();
                if let Ok(key) = order_key_between(
                    Some(&row.order_key),
                    upper.map(String::as_str),
                    &row.revision,
                ) {
                    row.order_key = key;
                }
            }
        }
        result.sort_by(|a, b| a.order_key.cmp(&b.order_key).then(a.id.cmp(&b.id)));
        result
    }
    pub fn change_draft(&mut self, change: &DraftChange) -> Result<(), DocError> {
        let id = match change {
            DraftChange::Move { id, .. } | DraftChange::Discard { id } => id,
        };
        if !valid_draft_id(id) {
            return Err(DocError::Schema("Invalid draft ID".into()));
        }
        self.observe_sidebar_row(DRAFTS, id);
        if let DraftChange::Discard { .. } = change {
            self.write(
                DRAFTS,
                id,
                OpKind::Upsert,
                fields([("closed", json!(true))]),
            );
            return Ok(());
        }
        if self.draft_closed(id) {
            return Ok(());
        }
        let DraftChange::Move { after, before, .. } = change else {
            unreachable!()
        };
        let mut rows = self.read_drafts();
        let Some(at) = rows.iter().position(|d| d.id == *id) else {
            return Ok(());
        };
        rows.remove(at);
        let at = before
            .as_ref()
            .and_then(|b| rows.iter().position(|d| &d.id == b))
            .or_else(|| {
                after
                    .as_ref()
                    .and_then(|a| rows.iter().position(|d| &d.id == a).map(|i| i + 1))
            })
            .unwrap_or(rows.len());
        let stamp = self.next_hlc();
        let key = order_key_between(
            at.checked_sub(1).map(|i| rows[i].order_key.as_str()),
            rows.get(at).map(|r| r.order_key.as_str()),
            &stamp,
        )
        .map_err(|e| DocError::Schema(e.into()))?;
        self.write(
            DRAFTS,
            id,
            OpKind::Upsert,
            fields([("orderKey", json!(key))]),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn draft(id: &str, revision: &str, base: Option<&str>) -> SaveDraft {
        SaveDraft {
            id: id.into(),
            revision: revision.into(),
            base_revision: base.map(str::to_owned),
            created_at: 1,
            content: zeron_proto::DraftContent {
                prompt: revision.into(),
                ..Default::default()
            },
            assets: vec![],
        }
    }
    #[test]
    fn edits_keep_order_and_concurrent_heads_are_recoverable() {
        let mut doc = RegistryDoc::new("test");
        doc.publish_draft(&draft("a", "v1", None)).unwrap();
        doc.publish_draft(&draft("b", "v2", None)).unwrap();
        let key = doc
            .read_drafts()
            .iter()
            .find(|d| d.id == "a")
            .unwrap()
            .order_key
            .clone();
        doc.publish_draft(&draft("a", "v3", Some("v1"))).unwrap();
        doc.publish_draft(&draft("a", "v4", Some("v1"))).unwrap();
        let rows = doc.read_drafts();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.iter().find(|d| d.id == "a").unwrap().order_key, key);
        assert!(rows.iter().any(|d| d.id == "v3" && d.conflict));
        doc.publish_draft(&draft("v3", "v5", Some("v3"))).unwrap();
        assert_eq!(doc.read_drafts().len(), 3);
    }
    #[test]
    fn delayed_move_or_save_cannot_revive_discarded_draft() {
        let mut doc = RegistryDoc::new("test");
        doc.publish_draft(&draft("a", "v1", None)).unwrap();
        doc.change_draft(&DraftChange::Discard { id: "a".into() })
            .unwrap();
        doc.publish_draft(&draft("a", "v2", Some("v1"))).unwrap();
        doc.change_draft(&DraftChange::Move {
            id: "a".into(),
            after: None,
            before: None,
        })
        .unwrap();
        assert!(doc.read_drafts().is_empty());
    }
}
