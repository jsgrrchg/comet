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

struct SplitHost {
    shell: Entity<Shell>,
    _data_dir: tempfile::TempDir,
}

impl Render for SplitHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.shell.update(cx, |shell, cx| {
            let main = shell.render_main(window, 900., 900., cx);
            div()
                .size_full()
                .flex()
                .child(div().w(px(900.)).h_full().flex().child(main))
        })
    }
}

#[gpui::test]
fn panes_render_side_by_side_and_dividers_resize_them(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = shell_window(dir.path(), cx);
    let shell = window.root(cx).unwrap();
    window
        .update(cx, |shell, _, cx| shell.open_chat_in_split("b".into(), cx))
        .unwrap();
    cx.run_until_parked();
    let (_, cx) = cx.add_window_view(|_, _| SplitHost {
        shell: shell.clone(),
        _data_dir: dir,
    });
    cx.update(|window, cx| window.draw(cx).clear());
    let a = cx.debug_bounds("split-pane-a").expect("pane a renders");
    let b = cx.debug_bounds("split-pane-b").expect("pane b renders");
    assert!(a.right() <= b.left(), "a sits left of b");
    assert!((f32::from(a.size.width) - 449.5).abs() < 1.0);
    let divider = cx.debug_bounds("split-divider-0").unwrap().center();
    cx.simulate_mouse_down(divider, MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_mouse_move(
        gpui::point(divider.x + px(30.), divider.y),
        Some(MouseButton::Left),
        gpui::Modifiers::default(),
    );
    cx.simulate_mouse_move(
        gpui::point(px(600.), divider.y),
        Some(MouseButton::Left),
        gpui::Modifiers::default(),
    );
    cx.simulate_mouse_up(
        gpui::point(px(600.), divider.y),
        MouseButton::Left,
        gpui::Modifiers::default(),
    );
    cx.update(|window, cx| window.draw(cx).clear());
    let ratios = shell.read_with(cx, |shell, cx| {
        shell.shown_split(cx).unwrap().normalized_ratios()
    });
    assert!((ratios[0] - 600. / 900.).abs() < 0.01, "{ratios:?}");
    let a = cx.debug_bounds("split-pane-a").unwrap();
    assert!((f32::from(a.size.width) - 599.).abs() < 2.0, "{a:?}");
    // Dragging never squeezes a pane under its minimum.
    let divider = cx.debug_bounds("split-divider-0").unwrap().center();
    cx.simulate_mouse_down(divider, MouseButton::Left, gpui::Modifiers::default());
    cx.simulate_mouse_move(
        gpui::point(px(890.), divider.y),
        Some(MouseButton::Left),
        gpui::Modifiers::default(),
    );
    cx.simulate_mouse_up(
        gpui::point(px(890.), divider.y),
        MouseButton::Left,
        gpui::Modifiers::default(),
    );
    let ratios = shell.read_with(cx, |shell, cx| {
        shell.shown_split(cx).unwrap().normalized_ratios()
    });
    assert!(ratios[1] * 900. >= 239., "{ratios:?}");
    // Clicking into a pane focuses it.
    let b = cx.debug_bounds("split-pane-b").unwrap().center();
    cx.simulate_click(b, gpui::Modifiers::default());
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("b"))
    });
    let a = cx.debug_bounds("split-pane-a").unwrap().center();
    cx.simulate_click(a, gpui::Modifiers::default());
    cx.run_until_parked();
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("a"));
        assert_eq!(shell.active_chat, "a");
    });
}

#[gpui::test]
fn the_sidebar_lists_a_split_once_where_its_first_member_would(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = shell_window(dir.path(), cx);
    window
        .update(cx, |shell, _, cx| {
            shell.settings.sidebar_organization = SidebarOrganization::InOneList;
            // Newest first: a, b, c, d.
            assert_eq!(shell.sidebar_visible_order(cx), ["a", "b", "c", "d"]);
            shell.open_chat("c".into(), cx);
            shell.open_chat_in_split("a".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            // The split's slot is a's; c no longer lists on its own.
            assert_eq!(shell.sidebar_visible_order(cx), ["a", "b", "d"]);
            // Focus c, then step: from the split's slot to b, and back to
            // the split's last focused pane.
            shell.focus_split_pane("c", cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| shell.cycle_session(true, cx))
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert_eq!(shell.active_chat, "b");
            shell.cycle_session(false, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert_eq!(
                shell.active_chat, "c",
                "the split reopens on its focused pane"
            );
            assert!(shell.shown_split(cx).is_some());
            shell.jump_to_session(2, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert_eq!(shell.active_chat, "d");
            // A pinned member pins the split's row.
            shell
                .settings
                .sidebar_pins_mut("local".into())
                .push("d".into());
            shell.open_chat_in_split("b".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert_eq!(shell.sidebar_visible_order(cx), ["d", "a"]);
            shell.jump_to_session(1, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, _| assert_eq!(shell.active_chat, "c"))
        .unwrap();
}

/// A turn that finishes in an unfocused pane is seen there: no Done badge
/// waits for a click on a chat that is on screen.
#[gpui::test]
fn chats_in_every_pane_are_marked_seen(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = shell_window(dir.path(), cx);
    window
        .update(cx, |shell, window, cx| {
            shell.debug_gate = Some(GatePhase::Ready);
            shell.splash = SplashPhase::Gone;
            shell.open_chat_in_split("b".into(), cx);
            shell.focus_split_pane("a", cx);
            window.activate_window();
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            shell.state.update(cx, |state, cx| {
                for chat in state
                    .chats
                    .iter_mut()
                    .filter(|c| c.id == "b" || c.id == "c")
                {
                    chat.last_message_at = Some(Utc::now());
                    chat.last_seen_at = None;
                }
                cx.notify();
            });
            assert!(
                shell
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .any(|c| c.id == "b" && c.unseen())
            );
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear())
        .unwrap();
    window
        .update(cx, |shell, _, cx| {
            let unseen = |id: &str| {
                shell
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .any(|c| c.id == id && c.unseen())
            };
            assert!(!unseen("b"), "b is on screen beside a");
            assert!(unseen("c"), "c is not");
        })
        .unwrap();
}

#[gpui::test]
fn focus_steps_between_panes_and_stops_at_the_ends(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = shell_window(dir.path(), cx);
    window
        .update(cx, |shell, _, cx| {
            shell.open_chat_in_split("b".into(), cx);
            shell.open_chat_in_split("c".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    for (forward, expected) in [(true, "c"), (false, "b"), (false, "a"), (false, "a")] {
        window
            .update(cx, |shell, _, cx| shell.step_split_focus(forward, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |shell, _, _| assert_eq!(shell.active_chat, expected))
            .unwrap();
    }
}

/// A split's panes claim their room in the window fit: where one chat
/// leaves space for the sidebar and the right pane, three panes evict the
/// column opened first.
#[test]
fn split_panes_claim_room_in_the_window_fit() {
    use panel_layout::horizontal_panel_fit;
    let one = horizontal_panel_fit(
        1100.0,
        256.0,
        (true, 1),
        (true, 2),
        (false, 0),
        false,
        300.0,
    );
    assert!(one.sidebar && one.right);
    let three = horizontal_panel_fit(
        1100.0,
        256.0,
        (true, 1),
        (true, 2),
        (false, 0),
        false,
        3.0 * split_panes::SPLIT_PANE_MIN,
    );
    assert!(!three.sidebar && three.right);
}

/// Splitting the conversation on screen, and closing back down to it, keeps
/// the columns it shows.
#[gpui::test]
fn joining_and_leaving_a_split_keeps_the_columns(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let window = shell_window(dir.path(), cx);
    window
        .update(cx, |shell, _, cx| {
            shell.toggle_right_pane(cx);
            assert!(shell.right_pane_open(cx));
            shell.open_chat_in_split("b".into(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert!(shell.shown_split(cx).is_some());
            assert!(shell.right_pane_open(cx), "the split keeps a's right pane");
            shell.close_split_pane("a", cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |shell, _, cx| {
            assert!(shell.shown_split(cx).is_none());
            assert_eq!(shell.active_chat, "b");
            assert!(shell.right_pane_open(cx), "b keeps the split's right pane");
        })
        .unwrap();
}
