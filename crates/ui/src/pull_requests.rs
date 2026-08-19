use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, IntoElement, Render, ScrollHandle, SharedString, Subscription,
    Task, Window, container_query, div, prelude::*, px,
};
use zeron_proto::{ChangeRequestListItem, Device};
use zeron_rpc::{RpcError, capability_errors, methods};

use crate::icons::{self, icon};
use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const SNAPSHOT_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
enum PullRequestsPageError {
    CliUnavailable,
    Authentication,
    RateLimited,
    Network,
    RemoteOffline(String),
    UpdateRequired(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PullRequestsLoadState {
    Idle,
    Loading,
    Ready,
    Failed(PullRequestsPageError),
}

/// Ephemeral dashboard for open pull requests authored by the active GitHub CLI account.
pub struct PullRequestsPage {
    state: Entity<AppState>,
    /// `None` keeps local calls direct; a value is forwarded by the relay.
    target_device: Option<String>,
    items: Vec<ChangeRequestListItem>,
    load_state: PullRequestsLoadState,
    last_loaded_at: Option<Instant>,
    generation: u64,
    request_task: Option<Task<()>>,
    scroll: ScrollHandle,
    device_menu: popover::Popup<()>,
    _observe: Subscription,
}

impl PullRequestsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            target_device: None,
            items: Vec::new(),
            load_state: PullRequestsLoadState::Idle,
            last_loaded_at: None,
            generation: 0,
            request_task: None,
            scroll: ScrollHandle::new(),
            device_menu: popover::Popup::default(),
            _observe: observe,
        };
        page.load(cx);
        page
    }

    /// Called whenever shell navigation makes the already-owned entity visible again.
    pub fn on_visible(&mut self, cx: &mut Context<Self>) {
        let stale = self
            .last_loaded_at
            .is_none_or(|loaded| loaded.elapsed() >= SNAPSHOT_TTL);
        if !matches!(self.load_state, PullRequestsLoadState::Loading) && stale {
            self.load(cx);
        }
    }

    fn close_device_menu(&mut self, cx: &mut Context<Self>) {
        if self.device_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.device_menu);
            cx.notify();
        }
    }

    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.close_device_menu(cx);
        if self.target_device == target {
            return;
        }

        self.generation = self.generation.wrapping_add(1);
        self.request_task = None;
        self.target_device = target;
        self.items.clear();
        self.load_state = PullRequestsLoadState::Idle;
        self.last_loaded_at = None;
        self.scroll.set_offset(gpui::Point::default());
        self.load(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.load_state, PullRequestsLoadState::Loading) {
            self.load(cx);
        }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.load_state = PullRequestsLoadState::Failed(PullRequestsPageError::Network);
            cx.notify();
            return;
        };

        let target_name = selected_device_name(&self.state.read(cx), self.target_device.as_deref());
        if let Some(target) = self.target_device.as_deref()
            && !self.state.read(cx).device_online(target, Utc::now())
        {
            self.load_state =
                PullRequestsLoadState::Failed(PullRequestsPageError::RemoteOffline(target_name));
            cx.notify();
            return;
        }

        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let params = params_for_target(self.target_device.as_deref());
        self.load_state = PullRequestsLoadState::Loading;
        self.request_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_OPEN_CHANGE_REQUESTS, params)
                .await;
            this.update(cx, |page, cx| {
                if !response_is_current(page.generation, generation) {
                    return;
                }

                page.request_task = None;
                let loaded = match result {
                    Ok(value) => serde_json::from_value::<Vec<ChangeRequestListItem>>(value)
                        .map_err(|_| PullRequestsPageError::Network),
                    Err(error) => Err(map_rpc_error(&error, &target_name)),
                };
                let succeeded = loaded.is_ok();
                page.load_state = settle_snapshot(&mut page.items, loaded);
                if succeeded {
                    page.last_loaded_at = Some(Instant::now());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (devices, local_id) = {
            let state = self.state.read(cx);
            (
                eligible_desktop_devices(&state.devices),
                state.local_device_id.clone(),
            )
        };
        if devices.len() <= 1 {
            return div().into_any_element();
        }

        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|device| Some(device.id.as_str()) == effective.as_deref());
        let label: SharedString = selected
            .map(|device| device.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let glyph = selected
            .map(|device| platform_icon(&device.platform))
            .unwrap_or(icons::LAPTOP);
        let open = self.device_menu.is_open();

        let mut trigger = div()
            .id("pull-requests-device-switcher")
            .flex_none()
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .bg(if open {
                crate::theme::ink(0.06)
            } else {
                gpui::transparent_black()
            })
            .when(!open, |element| {
                element.hover(|style| style.bg(crate::theme::ink(0.04)))
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|page, _, _, _| page.device_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|page, _, _, cx| {
                if page.device_menu.take_press_was_open() {
                    page.close_device_menu(cx);
                } else {
                    page.device_menu.open(());
                }
                cx.notify();
            }))
            .child(icon(glyph).size(px(16.0)).text_color(theme.text_muted))
            .child(
                div()
                    .max_w(px(130.0))
                    .truncate()
                    .text_size(px(12.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(label),
            )
            .child(
                icon(icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(0.5)),
            );

        if self.device_menu.get().is_some() {
            let closing = self.device_menu.closing_since();
            let menu = popover::popover_card(theme)
                .w(px(240.0))
                .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_device_menu(cx)))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Desktop devices"))
                .children(devices.into_iter().enumerate().map(|(index, device)| {
                    let active = effective.as_deref() == Some(device.id.as_str());
                    let local = local_id.as_deref() == Some(device.id.as_str());
                    let device_id = device.id.clone();
                    let name: SharedString = device.name.clone().into();
                    let online = self.state.read(cx).device_online(&device.id, Utc::now());
                    popover::menu_row(theme, active, format!("pull-requests-device-row-{index}"))
                        .id(("pull-requests-device-row", index))
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.set_target_device((!local).then(|| device_id.clone()), cx);
                        }))
                        .child(
                            icon(platform_icon(&device.platform))
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(local, |element| {
                            element.child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme.text_muted.opacity(0.45))
                                    .child("You"),
                            )
                        })
                        .child(div().size(px(6.0)).rounded_full().bg(if online {
                            theme.success
                        } else {
                            crate::theme::ink(0.2)
                        }))
                }));
            trigger = trigger.child(popover::anchored_menu(
                "pull-requests-device-menu",
                menu.into_any_element(),
                closing,
            ));
        }

        trigger.into_any_element()
    }

    fn render_empty_or_error(&self, theme: &Theme) -> AnyElement {
        let (title, body) = match &self.load_state {
            PullRequestsLoadState::Failed(error) => error_copy(error),
            _ => (
                "No open pull requests".to_string(),
                "Pull requests authored by you will appear here.".to_string(),
            ),
        };
        div()
            .mt(px(72.0))
            .flex()
            .flex_col()
            .items_center()
            .text_center()
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .max_w(px(420.0))
                    .text_size(px(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(body)),
            )
            .into_any_element()
    }
}

impl Render for PullRequestsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let initial_loading =
            self.items.is_empty() && matches!(self.load_state, PullRequestsLoadState::Loading);
        let refreshing =
            !self.items.is_empty() && matches!(self.load_state, PullRequestsLoadState::Loading);
        let refresh_error =
            !self.items.is_empty() && matches!(self.load_state, PullRequestsLoadState::Failed(_));
        let count = (!initial_loading
            && !matches!(self.load_state, PullRequestsLoadState::Failed(_))
            || !self.items.is_empty())
        .then_some(self.items.len());
        let items = self.items.clone();
        let grid_theme = theme.clone();
        let scroll = self.scroll.clone();

        div()
            .id("pull-requests-page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .child(
                div()
                    .w_full()
                    .max_w(px(1040.0))
                    .mx_auto()
                    .px(px(24.0))
                    .pt(px(32.0))
                    .pb(px(64.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(widgets::page_header(&theme, "Pull requests", count))
                            .child(div().flex_1())
                            .child(self.render_device_switcher(&theme, cx))
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("pull-requests-refresh")
                                    .flex_none()
                                    .hover(|style| widgets::ghost_hover(&theme, style))
                                    .when(
                                        matches!(self.load_state, PullRequestsLoadState::Loading),
                                        |element| element.opacity(0.5),
                                    )
                                    .on_click(cx.listener(|page, _, _, cx| page.refresh(cx)))
                                    .child(if refreshing || initial_loading {
                                        crate::loaders::mini_gradient_spinner(
                                            "pull-requests-refresh-spinner",
                                            1.75,
                                            cx.entity_id(),
                                            cx,
                                        )
                                        .into_any_element()
                                    } else {
                                        icon(icons::REFRESH)
                                            .size(px(16.0))
                                            .text_color(theme.text_muted)
                                            .into_any_element()
                                    })
                                    .child("Refresh"),
                            ),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        "Open pull requests authored by you on GitHub.",
                    ))
                    .when(refresh_error, |element| {
                        element.child(widgets::error_strip(&theme, "Refresh failed. Try again."))
                    })
                    .child(if initial_loading {
                        div()
                            .mt(px(72.0))
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(12.0))
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .child(crate::loaders::gradient_spinner(
                                "pull-requests-loading",
                                &theme,
                                3.0,
                                cx.entity_id(),
                                cx,
                            ))
                            .child("Loading pull requests…")
                            .into_any_element()
                    } else if items.is_empty() {
                        self.render_empty_or_error(&theme)
                    } else {
                        container_query(move |size, _, cx| {
                            render_card_grid(
                                &items,
                                dashboard_columns(f32::from(size.width)),
                                &grid_theme,
                                cx,
                            )
                        })
                        .mt(px(24.0))
                        .into_any_element()
                    }),
            )
    }
}

fn render_card_grid(
    items: &[ChangeRequestListItem],
    columns: usize,
    theme: &Theme,
    _cx: &mut gpui::App,
) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .children(items.chunks(columns).enumerate().map(|(row, chunk)| {
            div()
                .w_full()
                .flex()
                .gap(px(14.0))
                .children(
                    chunk
                        .iter()
                        .enumerate()
                        .map(|(column, item)| render_card(item, row * columns + column, theme)),
                )
                .when(chunk.len() < columns, |element| {
                    element.children((chunk.len()..columns).map(|_| div().flex_1().min_w_0()))
                })
        }))
        .into_any_element()
}

fn render_card(item: &ChangeRequestListItem, index: usize, theme: &Theme) -> AnyElement {
    let url = item.url.clone();
    let updated = relative_updated_at(item.updated_at, Utc::now());
    let title = truncate_title(&item.title, 120);
    div()
        .id(SharedString::from(format!(
            "pull-request-card-{index}-{}-{}",
            item.repository, item.number
        )))
        .flex_1()
        .min_w_0()
        .h(px(156.0))
        .p(px(16.0))
        .rounded(px(12.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.card_glass_bg())
        .cursor_pointer()
        .hover(|style| {
            style
                .bg(crate::theme::ink(0.035))
                .border_color(theme.border.opacity(0.9))
        })
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            cx.open_url(&url);
        })
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(item.repository.clone())),
                )
                .child(SharedString::from(format!("#{}", item.number))),
        )
        .child(
            div()
                .mt(px(13.0))
                .h(px(42.0))
                .overflow_hidden()
                .text_size(px(14.0))
                .line_height(px(20.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(SharedString::from(title)),
        )
        .child(div().flex_1())
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .when(item.is_draft, |element| {
                    element.child(widgets::badge(theme, "Draft"))
                })
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(theme.text_muted.opacity(0.75))
                        .child(SharedString::from(updated)),
                )
                .child(
                    icon(icons::ARROW_UP_RIGHT)
                        .size(px(14.0))
                        .text_color(theme.text_muted.opacity(0.55)),
                ),
        )
        .into_any_element()
}

fn eligible_desktop_devices(devices: &[Device]) -> Vec<Device> {
    let mut devices: Vec<_> = devices
        .iter()
        .filter(|device| !matches!(device.platform.as_str(), "ios" | "android"))
        .cloned()
        .collect();
    devices.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
    devices
}

fn params_for_target(target: Option<&str>) -> serde_json::Value {
    match target {
        Some(target) => serde_json::json!({ "targetDeviceId": target }),
        None => serde_json::json!({}),
    }
}

fn selected_device_name(state: &AppState, target: Option<&str>) -> String {
    target
        .or(state.local_device_id.as_deref())
        .and_then(|id| state.device_name(id))
        .map(single_line)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "This device".to_string())
}

fn map_rpc_error(error: &RpcError, target_name: &str) -> PullRequestsPageError {
    match error {
        RpcError::Capability(code) if code == capability_errors::PULL_REQUESTS_CLI_UNAVAILABLE => {
            PullRequestsPageError::CliUnavailable
        }
        RpcError::Capability(code) if code == capability_errors::PULL_REQUESTS_AUTHENTICATION => {
            PullRequestsPageError::Authentication
        }
        RpcError::Capability(code) if code == capability_errors::PULL_REQUESTS_RATE_LIMITED => {
            PullRequestsPageError::RateLimited
        }
        RpcError::UnknownMethod(_) => {
            PullRequestsPageError::UpdateRequired(target_name.to_string())
        }
        RpcError::Capability(_)
        | RpcError::BadParams(_)
        | RpcError::Failed(_)
        | RpcError::Transport(_)
        | RpcError::Closed => PullRequestsPageError::Network,
    }
}

fn error_copy(error: &PullRequestsPageError) -> (String, String) {
    match error {
        PullRequestsPageError::CliUnavailable => (
            "GitHub CLI isn’t available on this device".into(),
            "Install gh and sign in to view your pull requests.".into(),
        ),
        PullRequestsPageError::Authentication => (
            "Sign in to GitHub on this device".into(),
            "Run gh auth login, then refresh this page.".into(),
        ),
        PullRequestsPageError::RateLimited => (
            "GitHub’s rate limit was reached".into(),
            "Try again later.".into(),
        ),
        PullRequestsPageError::Network => (
            "Couldn’t load pull requests".into(),
            "Check the connection and try again.".into(),
        ),
        PullRequestsPageError::RemoteOffline(name) => (
            format!("{name} is offline"),
            "Reconnect the device and try again.".into(),
        ),
        PullRequestsPageError::UpdateRequired(name) => (
            format!("Update Zeron on {name}"),
            "This device doesn’t support the pull request dashboard yet.".into(),
        ),
    }
}

fn platform_icon(platform: &str) -> &'static str {
    match platform {
        "macos" | "darwin" => icons::LAPTOP,
        _ => icons::MONITOR,
    }
}

fn dashboard_columns(width: f32) -> usize {
    if width < 560.0 {
        1
    } else if width < 900.0 {
        2
    } else {
        3
    }
}

fn relative_updated_at(updated_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(updated_at).num_seconds().max(0);
    let (amount, unit) = if seconds < 60 {
        return "Updated just now".into();
    } else if seconds < 3_600 {
        (seconds / 60, "minute")
    } else if seconds < 86_400 {
        (seconds / 3_600, "hour")
    } else if seconds < 604_800 {
        (seconds / 86_400, "day")
    } else if seconds < 2_592_000 {
        (seconds / 604_800, "week")
    } else if seconds < 31_536_000 {
        (seconds / 2_592_000, "month")
    } else {
        (seconds / 31_536_000, "year")
    };
    format!(
        "Updated {amount} {unit}{} ago",
        if amount == 1 { "" } else { "s" }
    )
}

fn truncate_title(title: &str, max_chars: usize) -> String {
    let title = single_line(title);
    if title.chars().count() <= max_chars {
        return title;
    }
    let mut truncated: String = title.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn response_is_current(current: u64, response: u64) -> bool {
    current == response
}

fn settle_snapshot<T>(
    snapshot: &mut Vec<T>,
    result: Result<Vec<T>, PullRequestsPageError>,
) -> PullRequestsLoadState {
    match result {
        Ok(items) => {
            *snapshot = items;
            PullRequestsLoadState::Ready
        }
        Err(error) => PullRequestsLoadState::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn device(id: &str, platform: &str) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            platform: platform.into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }
    }

    #[test]
    fn only_desktop_devices_are_eligible_targets() {
        let eligible = eligible_desktop_devices(&[
            device("mac", "macos"),
            device("linux", "linux"),
            device("phone", "ios"),
            device("tablet", "android"),
        ]);
        assert_eq!(
            eligible.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["linux", "mac"]
        );
    }

    #[test]
    fn target_params_keep_local_calls_direct() {
        assert_eq!(params_for_target(None), serde_json::json!({}));
        assert_eq!(
            params_for_target(Some("host")),
            serde_json::json!({ "targetDeviceId": "host" })
        );
    }

    #[test]
    fn grid_breakpoints_follow_the_content_width() {
        assert_eq!(dashboard_columns(559.0), 1);
        assert_eq!(dashboard_columns(560.0), 2);
        assert_eq!(dashboard_columns(899.0), 2);
        assert_eq!(dashboard_columns(900.0), 3);
    }

    #[test]
    fn relative_dates_are_readable_and_pluralized() {
        let now = Utc::now();
        assert_eq!(relative_updated_at(now, now), "Updated just now");
        assert_eq!(
            relative_updated_at(now - TimeDelta::hours(1), now),
            "Updated 1 hour ago"
        );
        assert_eq!(
            relative_updated_at(now - TimeDelta::days(2), now),
            "Updated 2 days ago"
        );
    }

    #[test]
    fn stale_responses_cannot_replace_a_new_target_snapshot() {
        assert!(response_is_current(7, 7));
        assert!(!response_is_current(8, 7));
    }

    #[test]
    fn successful_empty_refresh_replaces_the_previous_snapshot() {
        let mut snapshot = vec![1, 2];
        let state = settle_snapshot(&mut snapshot, Ok(Vec::new()));
        assert!(snapshot.is_empty());
        assert_eq!(state, PullRequestsLoadState::Ready);
    }

    #[test]
    fn failed_refresh_preserves_the_previous_snapshot() {
        let mut snapshot = vec![1, 2];
        let state = settle_snapshot(&mut snapshot, Err(PullRequestsPageError::Authentication));
        assert_eq!(snapshot, [1, 2]);
        assert_eq!(
            state,
            PullRequestsLoadState::Failed(PullRequestsPageError::Authentication)
        );
    }

    #[test]
    fn error_mapping_uses_stable_codes_and_hides_transport_details() {
        assert_eq!(
            map_rpc_error(
                &RpcError::Capability(capability_errors::PULL_REQUESTS_AUTHENTICATION.into()),
                "Studio Mac",
            ),
            PullRequestsPageError::Authentication
        );
        assert_eq!(
            map_rpc_error(
                &RpcError::UnknownMethod("ListOpenChangeRequests".into()),
                "Studio Mac"
            ),
            PullRequestsPageError::UpdateRequired("Studio Mac".into())
        );
        assert_eq!(
            map_rpc_error(&RpcError::Transport("secret detail".into()), "Studio Mac"),
            PullRequestsPageError::Network
        );
    }

    #[test]
    fn visible_device_names_and_titles_are_sanitized() {
        assert_eq!(single_line("MacBook\n Pro"), "MacBook Pro");
        assert!(truncate_title(&"a".repeat(200), 120).ends_with('…'));
        assert_eq!(truncate_title("Short title", 120), "Short title");
    }
}
