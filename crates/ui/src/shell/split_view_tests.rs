//! Split views end to end on a shell without an engine: panes follow the
//! selection, drafts follow their chat, and the side columns belong to the
//! split rather than to whichever pane has focus.
use super::*;
use gpui::{AppContext, TestAppContext};

fn shell_window(dir: &std::path::Path, cx: &mut TestAppContext) -> gpui::WindowHandle<Shell> {
    cx.update(|cx| {
        gpui_base::init(cx);
        cx.set_global(Theme::default());
        crate::app_menus::init(cx);
        settings::init(settings::UiSettings::default(), dir, cx);
        crate::history::init(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            cx,
        );
    });
    let window = cx.add_window(|_, cx| {
        let state = cx.new(|_| AppState::new());
        Shell::new(
            state,
            EngineBootConfig {
                data_dir: dir.into(),
                ipc_port: 0,
                edge_url: "http://127.0.0.1:1".into(),
                edge_token: None,
                org_id: None,
                workos_client_id: None,
                default_harness: zeron_proto::HarnessId::Mock,
            },
            cx,
        )
    });
    window
        .update(cx, |shell, _, cx| {
            shell.reduced_motion = true;
            shell.state.update(cx, |state, _| {
                state.workspace_scope = Some(WorkspaceScope::Local);
                state.local_device_id = Some("local".into());
                let chats = ["a", "b", "c", "d"]
                    .into_iter()
                    .enumerate()
                    .map(|(ix, id)| {
                        serde_json::from_value(serde_json::json!({
                            "id": id, "title": id, "deviceId": "local", "archived": false,
                            "createdAt": Utc::now() - chrono::Duration::minutes(ix as i64),
                        }))
                        .unwrap()
                    })
                    .collect();
                state.apply_chats(chats);
            });
            shell.open_chat("a".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
}

fn input_text(composer: &Entity<Composer>, cx: &App) -> String {
    composer.read(cx).input.read(cx).text().to_string()
}

#[gpui::test]
fn chats_open_side_by_side_and_keep_their_drafts(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = shell_window(dir.path(), cx);
    window
        .update(cx, |shell, _, cx| {
            shell.composer.update(cx, |composer, cx| {
                composer
                    .input
                    .update(cx, |input, cx| input.set_text("draft a", cx))
            });
            shell.open_chat_in_split("b".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            let view = shell
                .shown_split(cx)
                .cloned()
                .expect("a split is on screen");
            assert_eq!(view.chats, ["a", "b"]);
            assert_eq!(shell.active_chat, "b", "the opened chat has focus");
            assert_eq!(shell.layout_key(cx), format!("split:{}", view.id));
            assert!(shell.split_pane_open("a") && shell.split_pane_open("b"));
            let pane_a = shell.split_panes["a"].composer.clone();
            assert_eq!(
                input_text(&pane_a, cx),
                "draft a",
                "the draft moved into its pane"
            );
            assert!(input_text(&shell.composer, cx).is_empty());
            assert_eq!(
                shell.active_composer().entity_id(),
                shell.split_panes["b"].composer.entity_id()
            );
            // Persisted for the profile.
            assert_eq!(shell.settings.split_views_by_profile["local"], [view]);
        })
        .unwrap();
    // Leaving the split for another chat hands the drafts back; coming
    // back restores the split with the pane focused last.
    window
        .update(cx, |shell, _, cx| shell.open_chat("c".into(), cx))
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert!(shell.shown_split(cx).is_none());
            assert!(shell.split_panes.is_empty());
            assert_eq!(shell.layout_key(cx), "c");
            assert_eq!(shell.composer.read(cx).saved_draft("a"), Some("draft a"));
            shell.open_chat("a".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert!(shell.shown_split(cx).is_some());
            assert_eq!(input_text(&shell.split_panes["a"].composer, cx), "draft a");
        })
        .unwrap();
}

#[gpui::test]
fn focus_moves_between_panes_without_moving_the_columns(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = shell_window(dir.path(), cx);
    window
        .update(cx, |shell, _, cx| {
            shell.open_chat_in_split("b".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, window, cx| {
            shell.toggle_right_pane(cx);
            shell.add_browser_surface(None, window, cx);
            assert!(shell.right_pane_open(cx));
            assert_eq!(shell.right_surface_rows(cx).len(), 1, "b's browser tab");
            shell.focus_split_pane("a", cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert_eq!(shell.active_chat, "a");
            assert!(
                shell.right_pane_open(cx),
                "the pane stays open for the split"
            );
            assert!(
                shell.right_surface_rows(cx).is_empty(),
                "its content follows the focused chat"
            );
            assert!(!shell.panels.get("a").changes_open);
            // Opening a lone chat restores that chat's own columns.
            shell.open_chat("c".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert!(!shell.right_pane_open(cx));
            shell.open_chat("b".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert!(shell.right_pane_open(cx));
            assert_eq!(shell.right_surface_rows(cx).len(), 1);
        })
        .unwrap();
}

#[gpui::test]
fn closing_panes_dissolves_the_split_and_full_splits_refuse(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = shell_window(dir.path(), cx);
    window
        .update(cx, |shell, _, cx| {
            shell.open_chat_in_split("b".into(), cx);
            shell.open_chat_in_split("c".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert_eq!(shell.shown_split(cx).unwrap().chats, ["a", "b", "c"]);
            assert!(!shell.split_chat_beside("c", "d", crate::split_view::SplitSide::Right, cx));
            assert_eq!(shell.shown_split(cx).unwrap().chats.len(), 3);
            // Closing the focused pane focuses its neighbour.
            shell.close_split_pane("c", cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert_eq!(shell.active_chat, "b");
            assert_eq!(shell.shown_split(cx).unwrap().chats, ["a", "b"]);
            shell.close_split_pane("a", cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert!(shell.split_views(cx).is_empty());
            assert!(shell.split_panes.is_empty());
            assert_eq!(shell.active_chat, "b");
        })
        .unwrap();
    // Archiving a member prunes it.
    window
        .update(cx, |shell, _, cx| shell.open_chat_in_split("c".into(), cx))
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            let mut chats = shell.state.read(cx).chats.clone();
            chats.iter_mut().find(|c| c.id == "b").unwrap().archived = true;
            shell.state.update(cx, |state, cx| {
                state.apply_chats(chats);
                cx.notify();
            });
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert!(shell.split_views(cx).is_empty());
        })
        .unwrap();
}
