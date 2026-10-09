//! Keeps the focused chat's workspace search index warm on its host, so the
//! first `@`, file-tree or cmd+K search in a chat does not pay for the scan.
//!
//! Focusing a chat warms its index pinned and renews the pin every
//! [`WARM_HEARTBEAT`]; leaving it releases the pin (`pin: false`), handing
//! the index to the host's idle eviction. Hosts that predate
//! `WarmWorkspaceSearch` are skipped silently.

use std::time::Duration;

use gpui::{Context, Task};

use super::Shell;
use crate::files::client::{FilesRequestContext, WorkspaceFilesClient};
use crate::state::EngineHandle;

/// Well inside the host's pin lifetime (`PIN_TTL`, 3 min).
pub(super) const WARM_HEARTBEAT: Duration = Duration::from_secs(60);

pub(super) struct SearchWarm {
    chat_id: String,
    context: FilesRequestContext,
    engine: EngineHandle,
    _heartbeat: Task<()>,
}

impl SearchWarm {
    fn is_for(&self, chat_id: &str, context: &FilesRequestContext, engine: &EngineHandle) -> bool {
        self.chat_id == chat_id && &self.context == context && self.engine.same_connection(engine)
    }
}

impl Shell {
    /// Follow the selected chat: pin its index, unpin the one left behind.
    pub(super) fn sync_search_warm(&mut self, cx: &mut Context<Self>) {
        let (focused, engine) = {
            let state = self.state.read(cx);
            let focused = state.selected_chat.as_deref().and_then(|chat_id| {
                FilesRequestContext::for_chat(state, chat_id)
                    .map(|context| (chat_id.to_owned(), context))
            });
            (focused, state.engine().cloned())
        };
        let current = focused.as_ref().zip(engine.as_ref());
        if let (Some(warm), Some(((chat_id, context), engine))) = (&self.search_warm, current)
            && warm.is_for(chat_id, context, engine)
        {
            return;
        }
        if let Some(previous) = self.search_warm.take() {
            let client = WorkspaceFilesClient::new(previous.engine, previous.context);
            cx.spawn(async move |_, _| client.hint_search_warm(false).await)
                .detach();
        }
        let (Some((chat_id, context)), Some(engine)) = (focused, engine) else {
            return;
        };
        let client = WorkspaceFilesClient::new(engine.clone(), context.clone());
        let heartbeat = cx.spawn(async move |_, cx| {
            while client.hint_search_warm(true).await {
                cx.background_executor().timer(WARM_HEARTBEAT).await;
            }
        });
        self.search_warm = Some(SearchWarm {
            chat_id,
            context,
            engine,
            _heartbeat: heartbeat,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;
    use crate::theme::Theme;
    use gpui::AppContext as _;

    fn test_shell(
        cx: &mut gpui::TestAppContext,
        path: &std::path::Path,
    ) -> gpui::WindowHandle<Shell> {
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
        });
        cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                crate::EngineBootConfig {
                    data_dir: path.into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        })
    }

    fn chat(id: &str) -> zeron_proto::Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": id, "deviceId": "local", "archived": false,
            "createdAt": "2026-09-20T00:00:00Z",
        }))
        .unwrap()
    }

    /// `(chatId, pin)` of every WarmWorkspaceSearch sent so far, answering each.
    fn drain_warms(
        cx: &mut gpui::TestAppContext,
        runtime: &tokio::runtime::Runtime,
        requests: &mut tokio::sync::mpsc::Receiver<String>,
        replies: &tokio::sync::mpsc::Sender<String>,
    ) -> Vec<(String, bool)> {
        let mut warms = Vec::new();
        loop {
            cx.run_until_parked();
            let Ok(request) = requests.try_recv() else {
                return warms;
            };
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            if request["method"] == zeron_rpc::methods::WARM_WORKSPACE_SEARCH {
                warms.push((
                    request["params"]["chatId"].as_str().unwrap().to_owned(),
                    request["params"]["pin"].as_bool().unwrap(),
                ));
            }
            let reply = serde_json::json!({ "id": request["id"], "ok": { "state": "ready" } });
            runtime.block_on(replies.send(reply.to_string())).unwrap();
        }
    }

    #[gpui::test]
    fn focusing_a_chat_pins_its_index_and_releases_the_previous(cx: &mut gpui::TestAppContext) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let (out, mut requests) = tokio::sync::mpsc::channel(16);
        let (replies, inbound) = tokio::sync::mpsc::channel(16);
        let engine =
            crate::state::EngineHandle::from_test_client(zeron_rpc::RpcClient::new(out, inbound));
        let dir = tempfile::tempdir().unwrap();
        let window = test_shell(cx, dir.path());
        window
            .update(cx, |shell, _, cx| {
                shell.state.update(cx, |state, cx| {
                    state.local_device_id = Some("local".into());
                    state.chats_synced = true;
                    state.chats = vec![chat("a"), chat("b")];
                    state.set_test_engine(engine);
                    state.select_chat(Some("a".into()), cx);
                });
            })
            .unwrap();
        assert_eq!(
            drain_warms(cx, &runtime, &mut requests, &replies),
            vec![("a".to_owned(), true)]
        );

        window
            .update(cx, |shell, _, cx| {
                shell
                    .state
                    .update(cx, |state, cx| state.select_chat(Some("b".into()), cx));
            })
            .unwrap();
        let mut warms = drain_warms(cx, &runtime, &mut requests, &replies);
        warms.sort();
        assert_eq!(warms, vec![("a".to_owned(), false), ("b".to_owned(), true)]);

        // Unrelated state churn does not re-send.
        window
            .update(cx, |shell, _, cx| {
                shell.state.update(cx, |_, cx| cx.notify())
            })
            .unwrap();
        assert!(drain_warms(cx, &runtime, &mut requests, &replies).is_empty());
    }
}
