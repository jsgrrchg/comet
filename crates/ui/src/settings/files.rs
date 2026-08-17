//! Settings → Files: local preferences for workspace-file editing.

use gpui::{Context, EventEmitter, SharedString, Window, div, prelude::*, px};

use super::widgets;
use crate::{icons, theme::Theme};

const DELAY_OPTIONS: [u64; 5] = [300, 600, 900, 1_500, 3_000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesSettingsEvent {
    Changed { autosave_delay_ms: u64 },
}

pub struct FilesSettingsPage {
    autosave_delay_ms: u64,
}

impl EventEmitter<FilesSettingsEvent> for FilesSettingsPage {}

impl FilesSettingsPage {
    pub fn new(autosave_delay_ms: u64, _cx: &mut Context<Self>) -> Self {
        Self { autosave_delay_ms }
    }
}

impl Render for FilesSettingsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let selected = self.autosave_delay_ms;
        let options = DELAY_OPTIONS.into_iter().map(|delay| {
            let active = delay == selected;
            div()
                .id(SharedString::from(format!("files-autosave-{delay}")))
                .h(px(28.0))
                .px(px(10.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(if active {
                    theme.accent.opacity(0.7)
                } else {
                    theme.border
                })
                .bg(if active {
                    theme.accent.opacity(0.11)
                } else {
                    crate::theme::wash(0.025)
                })
                .text_size(px(11.5))
                .text_color(if active { theme.text } else { theme.text_muted })
                .flex()
                .items_center()
                .cursor_pointer()
                .hover(|style| style.bg(crate::theme::wash(0.08)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.autosave_delay_ms = delay;
                    cx.emit(FilesSettingsEvent::Changed {
                        autosave_delay_ms: delay,
                    });
                    cx.notify();
                }))
                .child(if delay >= 1_000 {
                    format!("{} s", delay as f32 / 1_000.0)
                } else {
                    format!("{delay} ms")
                })
        });

        let card = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .items_start()
                .child(widgets::row_tile(&theme, icons::FOLDER))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(&theme, "Autosave delay"))
                        .child(widgets::meta_line(
                            &theme,
                            vec![
                                div()
                                    .child("Save files after editing has been idle for this long.")
                                    .into_any_element(),
                            ],
                        ))
                        .child(
                            div()
                                .mt(px(12.0))
                                .flex()
                                .flex_wrap()
                                .gap(px(7.0))
                                .children(options),
                        ),
                ),
        );

        div()
            .id("files-settings-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Files", None))
                    .child(
                        widgets::page_subtitle(
                            &theme,
                            "Control how workspace files are saved while you edit.",
                        )
                        .max_w(px(512.0))
                        .line_height(px(20.0)),
                    )
                    .child(card),
            )
    }
}
