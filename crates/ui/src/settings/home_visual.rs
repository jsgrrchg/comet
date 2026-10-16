//! Settings → Appearance → Home visual: the user's own Luau visual, drawn
//! below the blank new-thread composer. The script is the format agents
//! show with `show_visual`; it is checked the way the engine checks theirs
//! before it is saved, so a broken script never reaches the home view.

use gpui::{Context, Entity, SharedString, Subscription, Task, div, prelude::*, px};

use crate::composer::{CODE_INPUT_CONTEXT, ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::settings::widgets;
use crate::theme::Theme;

/// The visual's id, which also seeds its randomness: the check and the
/// home view see the same random data.
pub const HOME_VISUAL_ID: &str = "home-visual";

pub struct HomeVisualCard {
    input: Entity<ComposerInput>,
    error: Option<SharedString>,
    checking: Option<Task<()>>,
    _input_events: Subscription,
}

impl HomeVisualCard {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let saved = crate::settings::current(cx).new_thread_visual;
        let input = cx.new(|cx| {
            ComposerInput::with_context("-- Paste a Luau visual script", CODE_INPUT_CONTEXT, cx)
                .with_monospace()
                .with_text_metrics(12.0, 18.0)
                .with_viewport_height(240.0)
                .with_tab_stop()
        });
        if let Some(saved) = saved {
            input.update(cx, |input, cx| input.set_text(saved, cx));
        }
        let input_events = cx.subscribe(&input, |card: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                card.error = None;
                cx.notify();
            }
        });
        Self {
            input,
            error: None,
            checking: None,
            _input_events: input_events,
        }
    }

    fn draft(&self, cx: &gpui::App) -> String {
        self.input.read(cx).text().trim().to_owned()
    }

    /// Check the draft off the main thread, then save it (an empty draft
    /// clears the visual).
    fn apply(&mut self, cx: &mut Context<Self>) {
        let script = self.draft(cx);
        if script.is_empty() {
            self.clear(cx);
            return;
        }
        let check = cx.background_executor().spawn({
            let script = script.clone();
            async move {
                zeron_visual::check(&script, zeron_visual::seed_for(HOME_VISUAL_ID)).map(|_| ())
            }
        });
        self.error = None;
        self.checking = Some(cx.spawn(async move |this, cx| {
            let checked = check.await;
            this.update(cx, |card, cx| {
                card.checking = None;
                match checked {
                    Ok(()) => crate::settings::set_new_thread_visual(Some(script), cx),
                    Err(error) => card.error = Some(error.message.into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.checking = None;
        self.error = None;
        self.input.update(cx, |input, cx| input.set_text("", cx));
        crate::settings::set_new_thread_visual(None, cx);
        cx.notify();
    }
}

impl Render for HomeVisualCard {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).for_settings_surface();
        let saved = crate::settings::current(cx).new_thread_visual;
        let draft = self.draft(cx);
        let checking = self.checking.is_some();
        let applied = saved.as_deref().map(str::trim) == Some(draft.as_str())
            || (saved.is_none() && draft.is_empty());
        let status = if checking {
            "Checking the script…"
        } else if saved.is_some() && applied {
            "Showing below the new-session composer."
        } else if saved.is_some() {
            "Unapplied changes; the saved visual is still showing."
        } else {
            "A Luau visual, in the format agents use with show_visual."
        };
        let actions = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .when(saved.is_some() || !draft.is_empty(), |row| {
                row.child(
                    widgets::text_action(&theme, widgets::ActionTone::Quiet, "Clear")
                        .id("home-visual-clear")
                        .tab_index(0)
                        .role(gpui::Role::Button)
                        .aria_label("Remove the home visual")
                        .focus_visible(|s| s.border_2().border_color(theme.accent))
                        .on_click(cx.listener(|card, _, _, cx| card.clear(cx))),
                )
            })
            .child(
                widgets::text_action(
                    &theme,
                    widgets::ActionTone::Solid,
                    if checking { "Checking…" } else { "Apply" },
                )
                .id("home-visual-apply")
                .tab_index(0)
                .role(gpui::Role::Button)
                .aria_label("Check and apply the home visual")
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .when(applied || checking, |button| button.opacity(0.5))
                .when(!applied && !checking, |button| {
                    button.on_click(cx.listener(|card, _, _, cx| card.apply(cx)))
                }),
            );
        widgets::section_card(&theme)
            .mt_0()
            .child(
                widgets::card_row(&theme, true)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(160.0))
                            .child(widgets::row_title(&theme, "Visual script"))
                            .child(widgets::meta_line(
                                &theme,
                                vec![div().child(status).into_any_element()],
                            )),
                    )
                    .child(actions),
            )
            .child(
                div()
                    .mx(px(16.0))
                    .mb(px(16.0))
                    .child(popover::dialog_field(self.input.clone().into_any_element()))
                    .when_some(self.error.clone(), |block, error| {
                        block.child(widgets::error_strip(&theme, error).mt(px(12.0)))
                    }),
            )
    }
}
