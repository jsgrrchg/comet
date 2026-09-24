//! The new-session canvas owns a draft identity, independent of chat routing.
//! Debounced writes are serialized; navigation flushes the captured old target.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use zeron_proto::{DraftAsset, DraftAttachment, DraftBundle, DraftContent, PromptDraft, SaveDraft};

#[derive(Clone)]
pub(super) struct EditingDraft {
    pub id: String,
    pub revision: Option<String>,
    pub created_at: i64,
    pub saved: Option<DraftContent>,
    pub snapshot: DraftBundle,
}
impl Composer {
    pub(crate) fn active_prompt_draft(&self) -> Option<&str> {
        self.prompt_draft.as_ref().map(|d| d.id.as_str())
    }

    pub(super) fn capture_prompt_draft(&mut self, cx: &mut Context<Self>) {
        if !self.current_key.is_empty() || self.sending || self.prompt_draft_loading {
            return;
        }
        let state = self.state.read(cx);
        if state.selected_chat.is_some()
            || !state
                .engine()
                .is_some_and(|e| e.engine_info().supports(zeron_proto::DRAFTS_CAPABILITY))
        {
            return;
        }
        self.prompt_draft_engine = state.engine().cloned();
        let target = self.pickers.read(cx).prompt_draft_target(cx);
        let mut content = DraftContent {
            prompt: self.input.read(cx).text().to_string(),
            target,
            attachments: vec![],
        };
        let assets = Vec::new();
        for image in self.attachments.get("").into_iter().flatten() {
            append_asset(&mut content, &mut self.prompt_draft_assets, image, None);
        }
        for appshot in self.appshots.get("").into_iter().flatten() {
            append_asset(
                &mut content,
                &mut self.prompt_draft_assets,
                &appshot.screenshot,
                Some(serde_json::json!({
                    "id": appshot.id, "appName": appshot.app_name, "bundleIdentifier": appshot.bundle_identifier,
                    "windowTitle": appshot.window_title, "accessibility": appshot.accessibility,
                    "capturedAt": appshot.captured_at, "dimensions": appshot.screenshot_dimensions,
                })),
            );
        }
        if !content.has_content() && self.prompt_draft.is_none() {
            return;
        }
        let editor = self.prompt_draft.get_or_insert_with(|| EditingDraft {
            id: uuid::Uuid::new_v4().to_string(),
            revision: None,
            created_at: chrono::Utc::now().timestamp_millis(),
            saved: None,
            snapshot: DraftBundle::default(),
        });
        if editor.snapshot.content == content {
            return;
        }
        editor.snapshot = DraftBundle { content, assets };
        self.prompt_draft_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            this.update(cx, |composer, cx| {
                composer.flush_prompt_draft(cx);
            })
            .ok();
        }));
    }

    pub(crate) fn flush_prompt_draft(&mut self, cx: &mut Context<Self>) -> Option<SaveDraft> {
        let engine = self
            .prompt_draft_engine
            .clone()
            .or_else(|| self.state.read(cx).engine().cloned())?;
        let editor = self.prompt_draft.as_mut()?;
        if !editor.snapshot.content.has_content() {
            let id = editor.id.clone();
            let gate = self.prompt_draft_gate.clone();
            if editor.revision.is_some() {
                cx.spawn(async move |_, _| {
                    let _guard = gate.lock().await;
                    let _ = engine
                        .client()
                        .call(
                            methods::CHANGE_DRAFT,
                            serde_json::json!({ "action": "discard", "id": id }),
                        )
                        .await;
                })
                .detach();
            }
            self.prompt_draft = None;
            return None;
        }
        let changed = editor.saved.as_ref() != Some(&editor.snapshot.content);
        let base_revision = editor.revision.clone();
        if changed || editor.revision.is_none() {
            editor.revision = Some(uuid::Uuid::new_v4().to_string());
            editor.saved = Some(editor.snapshot.content.clone());
        }
        let request = SaveDraft {
            id: editor.id.clone(),
            revision: editor.revision.clone()?,
            base_revision,
            created_at: editor.created_at,
            content: editor.snapshot.content.clone(),
            assets: editor
                .snapshot
                .content
                .attachments
                .iter()
                .filter_map(|a| self.prompt_draft_assets.get(&a.id).cloned())
                .collect(),
        };
        if changed {
            let request = request.clone();
            let gate = self.prompt_draft_gate.clone();
            cx.spawn(async move |this, cx| {
                let _guard = gate.lock().await;
                let result = engine
                    .client()
                    .call(methods::SAVE_DRAFT, serde_json::to_value(&request).unwrap())
                    .await;
                this.update(cx, |composer, cx| {
                    if let Err(error) = result {
                        if composer.prompt_draft.as_ref().is_some_and(|d| {
                            d.id == request.id && d.revision.as_ref() == Some(&request.revision)
                        }) {
                            if let Some(draft) = &mut composer.prompt_draft {
                                draft.saved = None;
                            }
                        }
                        composer.failure = Some(format!("Couldn't save draft: {error}").into());
                        composer.failure_key = Some(String::new());
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        Some(request)
    }

    pub(crate) fn start_prompt_draft(&mut self, cx: &mut Context<Self>) {
        if self.current_key.is_empty() {
            self.capture_prompt_draft(cx);
            self.flush_prompt_draft(cx);
        }
        self.prompt_draft_debounce = None;
        self.prompt_draft_loading = false;
        self.prompt_draft_load_generation += 1;
        self.prompt_draft = None;
        self.drafts.remove("");
        self.attachments.remove("");
        self.appshots.remove("");
        if self.current_key.is_empty() {
            self.input.update(cx, |input, cx| input.set_text("", cx));
        }
        cx.notify();
    }

    pub(crate) fn open_prompt_draft(&mut self, row: PromptDraft, cx: &mut Context<Self>) {
        if self.current_key.is_empty() {
            self.capture_prompt_draft(cx);
            self.flush_prompt_draft(cx);
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.prompt_draft_debounce = None;
        self.prompt_draft_load_generation += 1;
        let generation = self.prompt_draft_load_generation;
        self.prompt_draft_loading = true;
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::LOAD_DRAFT,
                    serde_json::json!({"revision": row.revision}),
                )
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| serde_json::from_value::<DraftBundle>(v).map_err(|e| e.to_string()));
            this.update(cx, |composer, cx| {
                if composer.prompt_draft_load_generation != generation
                    || !composer
                        .state
                        .read(cx)
                        .engine()
                        .is_some_and(|e| e.same_connection(&engine))
                {
                    return;
                }
                composer.prompt_draft_loading = false;
                match result {
                    Err(error) => {
                        composer.failure = Some(format!("Couldn't open draft: {error}").into());
                        composer.failure_key = None;
                    }
                    Ok(bundle) => {
                        composer.state.update(cx, |state, cx| {
                            state.select_chat(None, cx);
                            state.select_space(bundle.content.target.space_id.clone(), cx);
                            state.selected_device = Some(bundle.content.target.device_id.clone());
                            cx.notify();
                        });
                        composer.on_state_changed(cx);
                        composer.pickers.update(cx, |pickers, cx| {
                            pickers.restore_prompt_draft_target(&bundle.content.target, cx)
                        });
                        composer.attachments.remove("");
                        composer.appshots.remove("");
                        for asset in &bundle.content.attachments {
                            let Some(bytes) = bundle
                                .assets
                                .iter()
                                .find(|b| b.blob == asset.blob)
                                .and_then(|b| STANDARD.decode(&b.data).ok())
                            else {
                                continue;
                            };
                            if let Some(data) = bundle.assets.iter().find(|b| b.blob == asset.blob)
                            {
                                composer
                                    .prompt_draft_assets
                                    .insert(asset.id.clone(), data.clone());
                            }
                            let mut image = attachments::stage_png_bytes(asset.name.clone(), bytes);
                            image.id = asset.id.clone();
                            if let Some(meta) = &asset.appshot {
                                if let Ok(accessibility) =
                                    serde_json::from_value(meta["accessibility"].clone())
                                {
                                    let appshot = CapturedAppshot {
                                        id: meta["id"].as_str().unwrap_or(&asset.id).into(),
                                        app_name: meta["appName"]
                                            .as_str()
                                            .unwrap_or("Application")
                                            .into(),
                                        bundle_identifier: meta["bundleIdentifier"]
                                            .as_str()
                                            .map(str::to_owned),
                                        window_title: meta["windowTitle"]
                                            .as_str()
                                            .map(str::to_owned),
                                        accessibility,
                                        screenshot_dimensions: serde_json::from_value(
                                            meta["dimensions"].clone(),
                                        )
                                        .ok()
                                        .flatten(),
                                        screenshot: image,
                                        app_icon: None,
                                        captured_at: serde_json::from_value(
                                            meta["capturedAt"].clone(),
                                        )
                                        .unwrap_or_else(|_| chrono::Utc::now()),
                                    };
                                    composer
                                        .appshots
                                        .entry(String::new())
                                        .or_default()
                                        .push(appshot);
                                    continue;
                                }
                            }
                            composer
                                .attachments
                                .entry(String::new())
                                .or_default()
                                .push(image);
                        }
                        composer.prompt_draft = Some(EditingDraft {
                            id: row.id,
                            revision: Some(row.revision),
                            created_at: row.created_at,
                            saved: Some(bundle.content.clone()),
                            snapshot: bundle.clone(),
                        });
                        composer
                            .drafts
                            .insert(String::new(), bundle.content.prompt.clone());
                        composer
                            .input
                            .update(cx, |input, cx| input.set_text(bundle.content.prompt, cx));
                        composer.failure = None;
                        composer.focus_pending = true;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}
fn append_asset(
    content: &mut DraftContent,
    cache: &mut HashMap<String, DraftAsset>,
    image: &StagedAttachment,
    appshot: Option<serde_json::Value>,
) {
    let asset = cache.entry(image.id.clone()).or_insert_with(|| DraftAsset {
        blob: format!("{:x}", Sha256::digest(image.bytes())),
        data: STANDARD.encode(image.bytes()),
    });
    content.attachments.push(DraftAttachment {
        id: image.id.clone(),
        name: image.name.clone(),
        blob: asset.blob.clone(),
        appshot,
    });
}
