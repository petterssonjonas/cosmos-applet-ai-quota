// SPDX-License-Identifier: MPL-2.0

use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::{Alignment, Length, Limits, Subscription, window::Id};
use cosmic::prelude::*;
use cosmic::widget;

use crate::auth::{self, ExistingSource};
use crate::model::{Provider, ProviderError, Snapshot};

const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

pub struct AppModel {
    core: cosmic::Core,
    popup: Option<Id>,
    providers: [ProviderState; 5],
    form: Option<ConnectForm>,
    icon: widget::icon::Handle,
}

struct ProviderState {
    provider: Provider,
    snapshot: Option<Snapshot>,
    source: Option<String>,
    updated_at: Option<i64>,
    attempted_at: Option<Instant>,
    error: Option<String>,
    loading: bool,
    connecting: bool,
    generation: u64,
}

impl ProviderState {
    fn new(provider: Provider) -> Self {
        Self {
            provider,
            snapshot: None,
            source: None,
            updated_at: None,
            attempted_at: None,
            error: None,
            loading: false,
            connecting: false,
            generation: 0,
        }
    }
}

struct ConnectForm {
    provider: Provider,
    key: String,
    cookie: String,
    workspace: String,
    error: Option<String>,
}

#[derive(Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    Tick,
    Refresh,
    Fetched(
        Provider,
        u64,
        Result<Option<(Snapshot, String)>, ProviderError>,
    ),
    Connect(Provider),
    Back,
    KeyChanged(String),
    CookieChanged(String),
    WorkspaceChanged(String),
    Existing(ExistingSource),
    BrowserLogin,
    SaveKey,
    SaveCookie,
    Disconnect,
    AuthFinished(Provider, u64, Result<(), ProviderError>),
    SurfaceAction(cosmic::surface::Action<Message>),
}

// Input messages carry secrets; never let runtime diagnostics print their contents.
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TogglePopup => "TogglePopup",
            Self::PopupClosed(_) => "PopupClosed",
            Self::Tick => "Tick",
            Self::Refresh => "Refresh",
            Self::Fetched(..) => "Fetched",
            Self::Connect(_) => "Connect",
            Self::Back => "Back",
            Self::KeyChanged(_) => "KeyChanged([redacted])",
            Self::CookieChanged(_) => "CookieChanged([redacted])",
            Self::WorkspaceChanged(_) => "WorkspaceChanged",
            Self::Existing(_) => "Existing",
            Self::BrowserLogin => "BrowserLogin",
            Self::SaveKey => "SaveKey",
            Self::SaveCookie => "SaveCookie",
            Self::Disconnect => "Disconnect",
            Self::AuthFinished(..) => "AuthFinished",
            Self::SurfaceAction(_) => "SurfaceAction",
        })
    }
}

enum ConnectAction {
    Existing(ExistingSource),
    Browser,
    Key(String),
    Cookie(String, Option<String>),
    Disconnect,
}

impl AppModel {
    fn refresh(&mut self, force: bool) -> Task<cosmic::Action<Message>> {
        let tasks = self
            .providers
            .iter_mut()
            .filter_map(|state| {
                if state.loading
                    || state.connecting
                    || (!force
                        && state
                            .attempted_at
                            .is_some_and(|at| at.elapsed() < AUTO_REFRESH_INTERVAL))
                {
                    return None;
                }
                state.loading = true;
                state.attempted_at = Some(Instant::now());
                let provider = state.provider;
                let generation = state.generation;
                Some(Task::perform(
                    async move {
                        let Some(credential) = auth::load(provider).await? else {
                            return Ok(None);
                        };
                        let snapshot = crate::providers::fetch(provider, &credential).await?;
                        Ok(Some((snapshot, credential.source)))
                    },
                    move |result| {
                        cosmic::Action::App(Message::Fetched(provider, generation, result))
                    },
                ))
            })
            .collect::<Vec<_>>();
        Task::batch(tasks)
    }

    fn connect(&mut self, action: ConnectAction) -> Task<cosmic::Action<Message>> {
        let Some(form) = self.form.as_mut() else {
            return Task::none();
        };
        let provider = form.provider;
        let state = &mut self.providers[provider as usize];
        if state.connecting {
            return Task::none();
        }
        form.error = None;
        state.generation += 1;
        state.connecting = true;
        state.loading = false;
        state.error = None;
        state.snapshot = None;
        state.source = None;
        state.updated_at = None;
        let generation = state.generation;
        Task::perform(
            async move {
                match action {
                    ConnectAction::Existing(source) => {
                        auth::connect_existing(provider, source).await
                    }
                    ConnectAction::Browser => auth::login(provider).await,
                    ConnectAction::Key(key) => auth::save_key(provider, key).await,
                    ConnectAction::Cookie(cookie, workspace) => {
                        auth::save_cookie(provider, cookie, workspace).await
                    }
                    ConnectAction::Disconnect => auth::disconnect(provider).await,
                }
            },
            move |result| cosmic::Action::App(Message::AuthFinished(provider, generation, result)),
        )
    }

    fn provider_view<'a>(&'a self, state: &'a ProviderState) -> Element<'a, Message> {
        let title = match state.snapshot.as_ref().and_then(|s| s.plan.as_deref()) {
            Some(plan) => format!("{} — {plan}", state.provider.name()),
            None => state.provider.name().into(),
        };
        let mut content = widget::column([]).width(Length::Fill).spacing(6).push(
            widget::row([
                widget::text::title4(title).width(Length::Fill).into(),
                widget::button::text("Connect")
                    .on_press(Message::Connect(state.provider))
                    .into(),
            ])
            .align_y(Alignment::Center)
            .spacing(8),
        );
        if state.connecting {
            content = content.push(widget::text("Waiting for connection…"));
        }
        if let Some(error) = &state.error {
            content = content.push(widget::text(error).class(cosmic::theme::Text::Color(
                cosmic::theme::active().cosmic().destructive_color().into(),
            )));
        }
        if let Some(snapshot) = &state.snapshot {
            if snapshot.windows.is_empty() && snapshot.details.is_empty() {
                content = content.push(widget::text("Quota data unavailable"));
            }
            for window in &snapshot.windows {
                let value = window.remaining_percent.map_or_else(
                    || "Unavailable".into(),
                    |percent| format!("{percent:.1}% left"),
                );
                content = content.push(detail_row(&window.label, value));
                content = content.push(
                    widget::text(format!("Reset: {}", time_label(window.resets_at))).size(12),
                );
            }
            let now = chrono::Utc::now().timestamp();
            match &snapshot.resets {
                None => {
                    content = content.push(detail_row("Resets available", "Unavailable".into()))
                }
                Some(resets) => {
                    let available = resets
                        .iter()
                        .filter(|r| r.expires_at.is_none_or(|at| at > now));
                    content = content.push(detail_row(
                        "Resets available",
                        available.clone().count().to_string(),
                    ));
                    for reset in available {
                        content = content.push(
                            widget::text(format!(
                                "{} · expires {}",
                                reset.label,
                                time_label(reset.expires_at)
                            ))
                            .size(12),
                        );
                    }
                }
            }
            content = content.push(detail_row(
                "Credits available",
                snapshot
                    .credits
                    .clone()
                    .unwrap_or_else(|| "Unavailable".into()),
            ));
            for (label, value) in &snapshot.details {
                content = content.push(detail_row(label, value.clone()));
            }
            for note in &snapshot.notes {
                content = content.push(widget::text(note).size(12));
            }
        } else if !state.loading && !state.connecting && state.error.is_none() {
            content = content.push(widget::text("Not connected"));
        }
        if state.loading {
            content = content.push(
                widget::row([
                    widget::progress_bar::indeterminate_circular()
                        .size(16.0)
                        .into(),
                    widget::text("Refreshing…").size(12).into(),
                ])
                .align_y(Alignment::Center)
                .spacing(6),
            );
        } else if let Some(updated) = state.updated_at {
            content = content
                .push(widget::text(format!("Updated: {}", time_label(Some(updated)))).size(12));
        }
        if let Some(source) = &state.source {
            content = content.push(widget::text(format!("Source: {source}")).size(12));
        }
        content.into()
    }

    fn connection_view<'a>(&'a self, form: &'a ConnectForm) -> Element<'a, Message> {
        let state = &self.providers[form.provider as usize];
        let enabled = !state.connecting;
        let mut content = widget::column([]).spacing(12)
            .push(widget::text::title3(format!("Connect {}", form.provider.name())))
            .push(widget::text("Existing logins stay read-only. New logins are isolated from your CLI accounts.").size(12));
        if matches!(form.provider, Provider::Claude | Provider::OpenAi) {
            content = content.push(action_button(
                "Use existing CLI login",
                Message::Existing(ExistingSource::Cli),
                enabled,
            ));
        }
        content = content.push(action_button(
            "Use existing OpenCode credentials",
            Message::Existing(ExistingSource::OpenCode),
            enabled,
        ));
        if matches!(
            form.provider,
            Provider::Claude | Provider::OpenAi | Provider::OpenRouter
        ) {
            content = content.push(action_button(
                "Sign in with browser",
                Message::BrowserLogin,
                enabled,
            ));
        }
        if form.provider != Provider::Claude {
            let mut input =
                widget::secure_input("API key", form.key.as_str(), None, true).label("API key");
            if enabled {
                input = input.on_input(Message::KeyChanged);
            }
            content = content.push(input).push(action_button(
                "Save API key",
                Message::SaveKey,
                enabled && !form.key.trim().is_empty(),
            ));
        }
        if matches!(form.provider, Provider::Claude | Provider::OpenCodeZen) {
            content = content.push(widget::text("Optional dashboard session: required for data not exposed to API keys/OAuth. Treat session cookies like passwords.").size(12));
            let mut cookie =
                widget::secure_input("Session cookie", form.cookie.as_str(), None, true)
                    .label("Dashboard cookie");
            if enabled {
                cookie = cookie.on_input(Message::CookieChanged);
            }
            content = content.push(cookie);
            let mut workspace = widget::text_input(
                "Organization/workspace ID (optional if only one)",
                form.workspace.as_str(),
            );
            if enabled {
                workspace = workspace.on_input(Message::WorkspaceChanged);
            }
            content = content.push(workspace);
            content = content.push(action_button(
                "Connect dashboard session",
                Message::SaveCookie,
                enabled && !form.cookie.trim().is_empty(),
            ));
        }
        if state.connecting {
            content = content.push(widget::text("Complete sign-in in your browser. This can take up to five minutes; the panel remains usable."));
        }
        if let Some(error) = form.error.as_ref().or(state.error.as_ref()) {
            content = content.push(widget::text(error));
        }
        content = content.push(
            widget::row([
                action_button("Back", Message::Back, enabled),
                action_button("Disconnect", Message::Disconnect, enabled),
            ])
            .spacing(8),
        );
        content.into()
    }
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = "dev.jp.CosmosAppletAiQuota";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, _flags: ()) -> (Self, Task<cosmic::Action<Message>>) {
        let mut app = Self {
            core,
            popup: None,
            providers: Provider::ALL.map(ProviderState::new),
            form: None,
            icon: widget::icon::from_svg_bytes(include_bytes!("../resources/icon.svg"))
                .symbolic(true),
        };
        let task = app.refresh(true);
        (app, task)
    }

    fn view(&self) -> Element<'_, Message> {
        let applet = &self.core.applet;
        applet
            .applet_tooltip(
                applet
                    .icon_button_from_handle(self.icon.clone())
                    .on_press(Message::TogglePopup),
                "Cosmos AI Quota",
                false,
                Message::SurfaceAction,
                self.core.main_window_id(),
            )
            .into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        let content: Element<'_, Message> = if let Some(form) = &self.form {
            self.connection_view(form)
        } else {
            let mut column = widget::column([]).width(Length::Fill).spacing(16).push(
                widget::row([
                    widget::text::title3("Cosmos AI Quota").width(Length::Fill).into(),
                    widget::button::text("Refresh")
                        .on_press(Message::Refresh)
                        .into(),
                ])
                .align_y(Alignment::Center),
            );
            for state in &self.providers {
                column = column
                    .push(widget::divider::horizontal::default())
                    .push(self.provider_view(state));
            }
            column
                .push(widget::text("Refreshes every 15 minutes · reset times are local").size(12))
                .into()
        };
        self.core
            .applet
            .popup_container(
                widget::container(
                    widget::scrollable(
                        widget::container(content)
                            .padding(cosmic::iced::Padding {
                                top: 0.0,
                                right: 16.0,
                                bottom: 0.0,
                                left: 0.0,
                            })
                            .width(Length::Fill),
                    )
                    .width(Length::Fill)
                    .height(Length::Fixed(640.0)),
                )
                .padding(16)
                .width(Length::Fill),
            )
            .limits(
                Limits::NONE
                    .min_width(440.0)
                    .max_width(440.0)
                    .max_height(760.0),
            )
            .into()
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn subscription(&self) -> Subscription<Message> {
        cosmic::iced::time::every(Duration::from_secs(30)).map(|_| Message::Tick)
    }

    fn update(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::SurfaceAction(action) => return cosmic::surface::surface_task(action),
            Message::TogglePopup => {
                if let Some(id) = self.popup.take() {
                    return destroy_popup(id);
                }
                let Some(parent) = self.core.main_window_id() else {
                    return Task::none();
                };
                let id = Id::unique();
                self.popup = Some(id);
                let mut settings = self
                    .core
                    .applet
                    .get_popup_settings(parent, id, None, None, None);
                settings.positioner.size_limits = Limits::NONE
                    .min_width(440.0)
                    .max_width(440.0)
                    .max_height(760.0);
                return Task::batch([get_popup(settings), self.refresh(false)]);
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                }
            }
            Message::Tick => return self.refresh(false),
            Message::Refresh => return self.refresh(true),
            Message::Fetched(provider, generation, result) => {
                let state = &mut self.providers[provider as usize];
                if state.generation != generation {
                    return Task::none();
                }
                state.loading = false;
                match result {
                    Ok(Some((mut snapshot, source))) => {
                        if provider == Provider::OpenAi
                            && let Some(index) = snapshot
                                .windows
                                .iter()
                                .position(|window| window.label == "Weekly")
                        {
                            snapshot.windows[..=index].rotate_right(1);
                        }
                        state.snapshot = Some(snapshot);
                        state.source = Some(source);
                        state.updated_at = Some(chrono::Utc::now().timestamp());
                        state.error = None;
                    }
                    Ok(None) => {
                        state.snapshot = None;
                        state.source = None;
                        state.error = None;
                        state.updated_at = None;
                    }
                    Err(error) => {
                        // A borrowed CLI login can switch accounts between reads. Never retain
                        // another account's old quota after a failed refresh.
                        state.snapshot = None;
                        state.source = None;
                        state.updated_at = None;
                        state.error = Some(error.0);
                    }
                }
            }
            Message::Connect(provider) => {
                self.form = Some(ConnectForm {
                    provider,
                    key: String::new(),
                    cookie: String::new(),
                    workspace: String::new(),
                    error: None,
                })
            }
            Message::Back => self.form = None,
            Message::KeyChanged(value) => {
                if let Some(form) = self.form.as_mut() {
                    form.key = value;
                }
            }
            Message::CookieChanged(value) => {
                if let Some(form) = self.form.as_mut() {
                    form.cookie = value;
                }
            }
            Message::WorkspaceChanged(value) => {
                if let Some(form) = self.form.as_mut() {
                    form.workspace = value;
                }
            }
            Message::Existing(source) => return self.connect(ConnectAction::Existing(source)),
            Message::BrowserLogin => return self.connect(ConnectAction::Browser),
            Message::SaveKey => {
                if let Some(form) = self.form.as_mut() {
                    let key = std::mem::take(&mut form.key);
                    return self.connect(ConnectAction::Key(key));
                }
            }
            Message::SaveCookie => {
                if let Some(form) = self.form.as_mut() {
                    let cookie = std::mem::take(&mut form.cookie);
                    let workspace = (!form.workspace.trim().is_empty())
                        .then(|| form.workspace.trim().to_owned());
                    return self.connect(ConnectAction::Cookie(cookie, workspace));
                }
            }
            Message::Disconnect => return self.connect(ConnectAction::Disconnect),
            Message::AuthFinished(provider, generation, result) => {
                let state = &mut self.providers[provider as usize];
                if state.generation != generation {
                    return Task::none();
                }
                state.connecting = false;
                match result {
                    Ok(()) => {
                        state.attempted_at = None;
                        if self
                            .form
                            .as_ref()
                            .is_some_and(|form| form.provider == provider)
                        {
                            self.form = None;
                        }
                        return self.refresh(false);
                    }
                    Err(error) => {
                        state.error = Some(error.0.clone());
                        if let Some(form) =
                            self.form.as_mut().filter(|form| form.provider == provider)
                        {
                            form.error = Some(error.0);
                        }
                        state.attempted_at = None;
                        return self.refresh(false);
                    }
                }
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

fn detail_row(label: &str, value: String) -> Element<'_, Message> {
    widget::row([
        widget::text(label).width(Length::Fill).into(),
        widget::text(value)
            .width(Length::Fill)
            .align_x(Alignment::End)
            .into(),
    ])
    .spacing(8)
    .width(Length::Fill)
    .into()
}

fn action_button(label: &str, message: Message, enabled: bool) -> Element<'_, Message> {
    let button = widget::button::text(label);
    if enabled {
        button.on_press(message).into()
    } else {
        button.into()
    }
}

fn time_label(timestamp: Option<i64>) -> String {
    timestamp
        .and_then(|timestamp| Local.timestamp_opt(timestamp, 0).single())
        .map(|date| date.format("%a %d %b %H:%M %Z").to_string())
        .unwrap_or_else(|| "Unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::Application;

    fn app() -> AppModel {
        AppModel {
            core: cosmic::Core::default(),
            popup: None,
            providers: Provider::ALL.map(ProviderState::new),
            form: None,
            icon: widget::icon::from_svg_bytes(include_bytes!("../resources/icon.svg"))
                .symbolic(true),
        }
    }

    #[test]
    fn automatic_refresh_waits_fifteen_minutes_but_manual_refresh_bypasses_wait() {
        let mut app = app();
        for state in &mut app.providers {
            state.attempted_at = Some(Instant::now() - Duration::from_secs(14 * 60));
        }
        let _ = app.refresh(false);
        assert!(app.providers.iter().all(|state| !state.loading));
        app.providers[Provider::OpenAi as usize].attempted_at =
            Some(Instant::now() - Duration::from_secs(15 * 60));
        let _ = app.refresh(false);
        assert!(app.providers[Provider::OpenAi as usize].loading);
        assert!(!app.providers[Provider::Claude as usize].loading);
        let _ = app.refresh(true);
        assert!(app.providers.iter().all(|state| state.loading));
    }

    #[test]
    fn previous_connection_cannot_replace_current_account_quota() {
        let mut app = app();
        let state = &mut app.providers[Provider::OpenAi as usize];
        state.generation = 2;
        state.snapshot = Some(Snapshot {
            plan: Some("current account".into()),
            ..Snapshot::default()
        });
        let _ = app.update(Message::Fetched(
            Provider::OpenAi,
            1,
            Ok(Some((
                Snapshot {
                    plan: Some("old account".into()),
                    ..Snapshot::default()
                },
                "old connection".into(),
            ))),
        ));
        assert_eq!(
            app.providers[Provider::OpenAi as usize]
                .snapshot
                .as_ref()
                .unwrap()
                .plan
                .as_deref(),
            Some("current account")
        );
    }

    #[test]
    fn failed_primary_refresh_does_not_present_previous_account_data() {
        let mut app = app();
        let state = &mut app.providers[Provider::Claude as usize];
        state.snapshot = Some(Snapshot {
            plan: Some("previous account".into()),
            ..Snapshot::default()
        });
        state.source = Some("CLI".into());
        state.updated_at = Some(100);
        let _ = app.update(Message::Fetched(
            Provider::Claude,
            0,
            Err(ProviderError("Sign-in expired".into())),
        ));
        let state = &app.providers[Provider::Claude as usize];
        assert!(state.snapshot.is_none() && state.source.is_none() && state.updated_at.is_none());
        assert_eq!(state.error.as_deref(), Some("Sign-in expired"));
    }

    #[test]
    fn another_provider_login_does_not_discard_unsaved_connection_input() {
        let mut app = app();
        let _ = app.update(Message::Connect(Provider::OpenRouter));
        let _ = app.update(Message::KeyChanged("unsaved-key".into()));
        let _ = app.update(Message::AuthFinished(Provider::Claude, 0, Ok(())));
        let form = app.form.as_ref().unwrap();
        assert_eq!(form.provider, Provider::OpenRouter);
        assert_eq!(form.key, "unsaved-key");
    }

    #[test]
    fn failed_reconnect_refetches_current_login_without_hiding_form_error() {
        let mut app = app();
        let _ = app.update(Message::Connect(Provider::Claude));
        let state = &mut app.providers[Provider::Claude as usize];
        state.connecting = true;
        state.attempted_at = Some(Instant::now());
        let _ = app.update(Message::AuthFinished(
            Provider::Claude,
            0,
            Err(ProviderError("New login was cancelled".into())),
        ));
        assert!(app.providers[Provider::Claude as usize].loading);
        let _ = app.update(Message::Fetched(
            Provider::Claude,
            0,
            Ok(Some((
                Snapshot {
                    plan: Some("Existing plan".into()),
                    ..Snapshot::default()
                },
                "CLI".into(),
            ))),
        ));
        assert_eq!(
            app.providers[Provider::Claude as usize]
                .snapshot
                .as_ref()
                .unwrap()
                .plan
                .as_deref(),
            Some("Existing plan"),
        );
        assert_eq!(
            app.form.as_ref().unwrap().error.as_deref(),
            Some("New login was cancelled")
        );
    }

    #[test]
    fn gui_debug_messages_do_not_disclose_credentials() {
        let secret = "private-credential-that-must-not-be-logged";
        for message in [
            Message::KeyChanged(secret.into()),
            Message::CookieChanged(secret.into()),
        ] {
            assert!(!format!("{message:?}").contains(secret));
        }
    }
}
