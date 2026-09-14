//! Offline native framebuffer fixture. No engine, credentials, or network.
use gpui::{prelude::*, *};
use std::{sync::Arc, time::Duration};
use zeron_ui::remote_desktop::desktop::Desktop;
struct Fixture {
    desktop: Entity<Desktop>,
    _task: Task<()>,
}
impl Fixture {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let desktop = cx.new(|cx| Desktop::new(window, cx));
        let output = desktop.clone();
        let task = cx.spawn_in(window, async move |_, cx| {
            let mut sequence = 0;
            loop {
                sequence += 1;
                let (width, height) = (1280u16, 800u16);
                let mut bgra = vec![0; width as usize * height as usize * 4];
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let colors = [
                            [0, 0, 255, 255],
                            [0, 255, 0, 255],
                            [255, 0, 0, 255],
                            [255, 255, 255, 255],
                        ];
                        let mut color = colors[(x / 320).min(3)];
                        if y > 500 {
                            color = if (x / 2 + y / 2) % 2 == 0 {
                                [255; 4]
                            } else {
                                [0, 0, 0, 255]
                            };
                        }
                        if x.abs_diff((sequence as usize * 8) % 1280) < 15
                            && (250..350).contains(&y)
                        {
                            color = [0, 255, 255, 255];
                        }
                        bgra[(y * width as usize + x) * 4..(y * width as usize + x + 1) * 4]
                            .copy_from_slice(&color);
                    }
                }
                let frame = zeron_rdp::Frame {
                    generation: 1,
                    sequence,
                    width,
                    height,
                    bgra: Arc::from(bgra),
                };
                if output
                    .update_in(cx, |view, window, cx| view.update_frame(&frame, window, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(34))
                    .await;
            }
        });
        Self {
            desktop,
            _task: task,
        }
    }
}
impl Render for Fixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x20242a))
            .text_color(rgb(0xffffff))
            .child(div().h(px(36.)).child(
                "Remote Desktop · red / green / blue / white · 1280 × 800 · offline fixture",
            ))
            .child(div().flex_1().min_h_0().child(self.desktop.clone()))
    }
}
fn main() {
    gpui_platform::application().run(|cx| {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(800.), px(600.)),
                    cx,
                ))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Fixture::new(window, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
