//! Login, laid out like Discord's: email/password (with captcha and 2FA) on
//! the left, a QR code for the mobile app on the right.

use std::sync::Arc;

use cord_core::auth::remote::{PendingUser, RemoteAuthEvent};
use cord_core::auth::{AuthError, CaptchaSolution, LoginOutcome, MfaChallenge, MfaMethod};
use cord_core::http::HttpError;
use cord_core::{Core, cdn};
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputContentType, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AnyView, App, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, Render, Styled,
    Subscription, Task, Window, black, div, white,
};

use crate::ui::u;
use crate::router::{RouteContext, navigate};
use crate::toasts::Toast;

pub fn qr_modules(data: &str) -> Vec<Vec<bool>> {
    let Ok(code) = qrcode::QrCode::new(data.as_bytes()) else { return Vec::new() };
    let width = code.width();
    code.to_colors()
        .chunks(width)
        .map(|row| row.iter().map(|c| *c == qrcode::Color::Dark).collect())
        .collect()
}

/// Cleans up a pasted token: whitespace and the quotes DevTools shows around it.
pub(crate) fn prepare_token(text: &str) -> Result<String, &'static str> {
    let token = text.trim().trim_matches('"').trim();
    if token.is_empty() {
        return Err("Paste your token first.");
    }
    if token.chars().any(char::is_whitespace) {
        return Err("That doesn't look like a token: it contains spaces.");
    }
    Ok(token.to_string())
}

pub(crate) fn auth_message(error: AuthError) -> String {
    match error {
        AuthError::Rejected(message) => message,
        AuthError::Http(HttpError::Network(_)) => "Couldn't reach Discord. Check your connection.".into(),
        AuthError::Http(error) => format!("Discord returned an error ({error})."),
    }
}

/// Login attempts before giving up (so at most two captcha solves).
const LOGIN_ATTEMPTS: usize = 3;

const CAPTCHA_LOOP: &str = "Discord kept asking for a captcha. Try the QR code instead.";

/// Logs in, opening the captcha window whenever Discord asks for one.
async fn login_with_captcha(core: Arc<Core>, email: String, password: String) -> Result<LoginOutcome, String> {
    let mut captcha: Option<CaptchaSolution> = None;
    for attempt in 1..=LOGIN_ATTEMPTS {
        match core.login(email.clone(), password.clone(), captcha.take()).await.map_err(auth_message)? {
            LoginOutcome::Captcha(_) if attempt == LOGIN_ATTEMPTS => break,
            LoginOutcome::Captcha(challenge) => {
                let solved = core
                    .runtime()
                    .spawn(async move {
                        let key = crate::captcha::solve(&challenge).await?;
                        Some(CaptchaSolution::for_challenge(&challenge, key))
                    })
                    .await
                    .ok()
                    .flatten();
                captcha = Some(solved.ok_or("The captcha was closed before it was solved.")?);
            }
            other => return Ok(other),
        }
    }
    Err(CAPTCHA_LOOP.into())
}

enum Qr {
    Waiting,
    Code(Vec<Vec<bool>>),
    Scanned(PendingUser),
    Expired,
    Failed(String),
}

enum Step {
    Credentials,
    Mfa { challenge: MfaChallenge, method: MfaMethod },
}

pub struct LoginPage {
    ctx: RouteContext,
    email: Entity<InputState>,
    password: Entity<InputState>,
    code: Entity<InputState>,
    token: Entity<InputState>,
    token_error: Option<String>,
    step: Step,
    qr: Qr,
    busy: bool,
    /// The Discord login window is open.
    web_login_open: bool,
    error: Option<String>,
    qr_task: Option<Task<()>>,
    login_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl LoginPage {
    fn new(ctx: RouteContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let email = cx.new(|cx| InputState::new(window, cx));
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let code = cx.new(|cx| InputState::new(window, cx).placeholder("Code"));
        let token = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("Paste your token"));

        // Enter submits from any field.
        let mut subscriptions: Vec<Subscription> = [&email, &password, &code]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit(cx);
                    }
                })
            })
            .collect();
        subscriptions.push(cx.subscribe_in(&token, window, |this, _, event, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.use_token(cx);
            }
        }));
        email.update(cx, |email, cx| email.focus(window, cx));

        let mut page = LoginPage {
            ctx,
            email,
            password,
            code,
            token,
            token_error: None,
            step: Step::Credentials,
            qr: Qr::Waiting,
            busy: false,
            web_login_open: false,
            error: None,
            qr_task: None,
            login_task: None,
            _subscriptions: subscriptions,
        };
        page.start_qr(cx);
        page
    }

    fn start_qr(&mut self, cx: &mut Context<Self>) {
        self.qr = Qr::Waiting;
        let core = self.ctx.services.core.clone();
        let mut session = core.remote_auth();
        self.qr_task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = session.events.recv().await {
                // Discord can ask for a captcha before handing over the token:
                // open the captcha window and pass the answer back.
                if let RemoteAuthEvent::CaptchaRequired(challenge) = &event {
                    let (challenge, reply) = (challenge.clone(), session.captcha_sender());
                    core.runtime().spawn(async move {
                        let key = crate::captcha::solve(&challenge).await;
                        let _ = reply.send(key.map(|key| CaptchaSolution::for_challenge(&challenge, key)));
                    });
                }
                if this.update(cx, |this, cx| this.on_qr(event, cx)).is_err() {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn on_qr(&mut self, event: RemoteAuthEvent, cx: &mut Context<Self>) {
        match event {
            RemoteAuthEvent::Qr(url) => self.qr = Qr::Code(qr_modules(&url)),
            RemoteAuthEvent::Scanned(user) => self.qr = Qr::Scanned(user),
            RemoteAuthEvent::Token(token) => return self.finish(token, cx),
            // Cancelled on the phone: offer a fresh code straight away.
            RemoteAuthEvent::Cancelled => return self.start_qr(cx),
            RemoteAuthEvent::Expired => self.qr = Qr::Expired,
            RemoteAuthEvent::Failed(message) => self.qr = Qr::Failed(message),
            // The captcha window is opened by start_qr's loop; keep showing "Check your phone".
            RemoteAuthEvent::CaptchaRequired(_) => return,
        }
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        match &self.step {
            Step::Credentials => self.login(cx),
            Step::Mfa { challenge, method } => {
                let (ticket, method) = (challenge.ticket.clone(), *method);
                self.verify_mfa(ticket, method, cx)
            }
        }
    }

    fn login(&mut self, cx: &mut Context<Self>) {
        let email = self.email.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        if email.is_empty() || password.is_empty() {
            self.error = Some("Enter your email and password.".into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();

        let core = self.ctx.services.core.clone();
        self.login_task = Some(cx.spawn(async move |this, cx| {
            let result = login_with_captcha(core, email, password).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(LoginOutcome::Token(token)) => return this.finish(token, cx),
                    Ok(LoginOutcome::Mfa(challenge)) => {
                        let method = if challenge.totp { MfaMethod::Totp } else { MfaMethod::Backup };
                        this.step = Step::Mfa { challenge, method };
                    }
                    Ok(LoginOutcome::Captcha(_)) => {
                        this.error = Some(CAPTCHA_LOOP.into())
                    }
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
            });
        }));
    }

    fn verify_mfa(&mut self, ticket: String, method: MfaMethod, cx: &mut Context<Self>) {
        let code = self.code.read(cx).value().trim().to_string();
        if code.is_empty() {
            self.error = Some("Enter the code.".into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();

        let core = self.ctx.services.core.clone();
        self.login_task = Some(cx.spawn(async move |this, cx| {
            let result = core.submit_mfa(ticket, method, code).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(token) => return this.finish(token, cx),
                    Err(error) => this.error = Some(auth_message(error)),
                }
                cx.notify();
            });
        }));
    }

    fn switch_mfa(&mut self, cx: &mut Context<Self>) {
        if let Step::Mfa { method, .. } = &mut self.step {
            *method = match method {
                MfaMethod::Totp => MfaMethod::Backup,
                MfaMethod::Backup => MfaMethod::Totp,
            };
        }
        self.error = None;
        cx.notify();
    }

    fn back_to_credentials(&mut self, cx: &mut Context<Self>) {
        self.step = Step::Credentials;
        self.error = None;
        cx.notify();
    }

    /// Logs in with a token the user pasted. If Discord rejects it, the
    /// session ends with the usual "Signed out" toast and lands back here.
    fn use_token(&mut self, cx: &mut Context<Self>) {
        let text = self.token.read(cx).value().to_string();
        match prepare_token(&text) {
            Ok(token) => self.finish(token, cx),
            Err(message) => {
                self.token_error = Some(message.into());
                cx.notify();
            }
        }
    }

    /// Discord's real login page in a throwaway window; it hands back the token.
    fn web_login(&mut self, cx: &mut Context<Self>) {
        self.web_login_open = true;
        cx.notify();
        let window = cx.background_executor().spawn(async { crate::web_login::run() });
        cx.spawn(async move |this, cx| {
            let token = window.await;
            let _ = this.update(cx, |this, cx| {
                this.web_login_open = false;
                match token {
                    Some(token) => this.finish(token, cx),
                    // Just closed: back to the form, nothing to report.
                    None => cx.notify(),
                }
            });
        })
        .detach();
    }

    fn finish(&mut self, token: String, cx: &mut Context<Self>) {
        let services = self.ctx.services.clone();
        if let Err(error) = services.core.save_token(&token) {
            services.toast(
                Toast::warning("Session not saved", format!("You're logged in, but you'll have to log in again next time: {error}")),
                cx,
            );
        }
        services.store.update(cx, |store, cx| store.start(token, cx));
        navigate(&self.ctx.router, "/channels/@me", cx);
    }

    fn error_line(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        self.error.clone().map(|error| {
            h_flex()
                .gap_2()
                .text_sm()
                .text_color(cx.theme().danger)
                .child(Icon::new(IconName::TriangleAlert).size(u(14.)))
                .child(error)
        })
    }

    fn credentials_form(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_5()
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("Welcome back!"))
                    .child(div().text_color(cx.theme().muted_foreground).child("We're so excited to see you again!")),
            )
            .child(
                Form::new()
                    .child(
                        Field::new()
                            .label("Email or phone number")
                            .child(Input::new(&self.email).content_type(InputContentType::EmailAddress)),
                    )
                    .child(
                        Field::new()
                            .label("Password")
                            .child(Input::new(&self.password).content_type(InputContentType::Password).mask_toggle()),
                    ),
            )
            .children(self.error_line(cx))
            .child(
                Button::new("login-submit")
                    .large()
                    .w_full()
                    .label("Log In")
                    .loading(self.busy)
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
            )
            .child(
                Button::new("login-web")
                    .primary()
                    .large()
                    .w_full()
                    .label("Log In on Discord's Page")
                    .loading(self.web_login_open)
                    .disabled(self.web_login_open)
                    .on_click(cx.listener(|this, _, _, cx| this.web_login(cx))),
            )
            .child(
                v_flex()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(Field::new().label("Or log in with a token").child(Input::new(&self.token).mask_toggle()))
                    .when_some(self.token_error.clone(), |this, error| {
                        this.child(div().text_sm().text_color(cx.theme().danger).child(error))
                    })
                    .child(
                        Button::new("login-token")
                            .w_full()
                            .label("Use token")
                            .on_click(cx.listener(|this, _, _, cx| this.use_token(cx))),
                    ),
            )
            .into_any_element()
    }

    fn mfa_form(&self, challenge: &MfaChallenge, method: MfaMethod, cx: &mut Context<Self>) -> AnyElement {
        let (title, hint, switch) = match method {
            MfaMethod::Totp => (
                "Two-factor authentication",
                "Enter the 6-digit code from your authenticator app.",
                "Use a backup code instead",
            ),
            MfaMethod::Backup => (
                "Use a backup code",
                "Enter one of your 8-character backup codes.",
                "Use your authenticator app instead",
            ),
        };
        let can_switch = challenge.totp && challenge.backup;
        v_flex()
            .gap_5()
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(div().text_color(cx.theme().muted_foreground).child(hint)),
            )
            .child(Input::new(&self.code))
            .children(self.error_line(cx))
            .child(
                Button::new("mfa-submit")
                    .primary()
                    .large()
                    .w_full()
                    .label("Log In")
                    .loading(self.busy)
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
            )
            .when(can_switch, |this| {
                this.child(
                    Button::new("mfa-switch")
                        .ghost()
                        .label(switch)
                        .on_click(cx.listener(|this, _, _, cx| this.switch_mfa(cx))),
                )
            })
            .child(
                Button::new("mfa-back")
                    .ghost()
                    .label("Back to login")
                    .on_click(cx.listener(|this, _, _, cx| this.back_to_credentials(cx))),
            )
            .into_any_element()
    }

    fn qr_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let retry = |id: &'static str, label: &'static str, cx: &mut Context<Self>| {
            Button::new(id).small().label(label).on_click(cx.listener(|this, _, _, cx| this.start_qr(cx)))
        };
        let body: AnyElement = match &self.qr {
            Qr::Waiting => div()
                .size(u(176.))
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(muted)
                .child("Preparing QR code…")
                .into_any_element(),
            Qr::Code(modules) => v_flex()
                .p(u(10.))
                .bg(white())
                .rounded(u(8.))
                .children(modules.iter().map(|row| {
                    h_flex().children(row.iter().map(|dark| div().size(u(4.)).when(*dark, |d| d.bg(black()))))
                }))
                .into_any_element(),
            Qr::Scanned(user) => v_flex()
                .items_center()
                .gap_2()
                .child(
                    Avatar::new().border_0()
                        .src(cdn::avatar_url(user.id, user.avatar.as_deref(), 128))
                        .name(user.username.clone())
                        .large(),
                )
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Check your phone!"))
                .child(div().text_sm().text_color(muted).child(format!("Logging in as {}", user.username)))
                .into_any_element(),
            Qr::Expired => v_flex()
                .items_center()
                .gap_3()
                .child(div().text_sm().text_color(muted).child("This QR code expired."))
                .child(retry("qr-refresh", "Get a new code", cx))
                .into_any_element(),
            Qr::Failed(message) => v_flex()
                .items_center()
                .gap_3()
                .child(div().text_sm().text_color(cx.theme().danger).child(message.clone()))
                .child(retry("qr-retry", "Try again", cx))
                .into_any_element(),
        };
        v_flex()
            .w(u(240.))
            .flex_shrink_0()
            .items_center()
            .gap_4()
            .child(body)
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Log in with QR Code"))
            .child(
                div()
                    .text_sm()
                    .text_center()
                    .text_color(muted)
                    .child("Scan this with the Discord mobile app to log in instantly."),
            )
    }
}

impl Render for LoginPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let form = match &self.step {
            Step::Credentials => self.credentials_form(cx),
            Step::Mfa { challenge, method } => {
                let (challenge, method) = (challenge.clone(), *method);
                self.mfa_form(&challenge, method, cx)
            }
        };
        h_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .w(u(784.))
                    .max_w_full()
                    .gap_8()
                    .p_8()
                    .items_center()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().popover)
                    .shadow_lg()
                    .child(div().flex_1().min_w_0().child(form))
                    .child(div().w(u(1.)).h(u(300.)).bg(cx.theme().border))
                    .child(self.qr_panel(cx)),
            )
    }
}

pub fn page(ctx: RouteContext, window: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|cx| LoginPage::new(ctx, window, cx)).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_is_square_with_finder_pattern() {
        let m = qr_modules("https://discord.com/ra/abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG");
        assert!(!m.is_empty());
        assert!(m.iter().all(|row| row.len() == m.len()));
        // Top-left finder pattern: a 7-module dark border.
        assert!(m[0][..7].iter().all(|d| *d));
        assert!(m[6][..7].iter().all(|d| *d));
        assert!(!m[1][1]);
    }

    #[test]
    fn pasted_tokens_are_cleaned_up() {
        assert_eq!(prepare_token("  abc.def.ghi \n"), Ok("abc.def.ghi".to_string()));
        // Copied from DevTools with the quotes.
        assert_eq!(prepare_token("\"abc.def.ghi\""), Ok("abc.def.ghi".to_string()));
        assert!(prepare_token("   ").is_err());
        assert!(prepare_token("abc def").is_err());
    }

    #[test]
    fn auth_errors_read_as_sentences() {
        assert_eq!(auth_message(AuthError::Rejected("Login or password is invalid.".into())), "Login or password is invalid.");
        assert_eq!(
            auth_message(AuthError::Http(HttpError::Network("dns".into()))),
            "Couldn't reach Discord. Check your connection."
        );
    }
}
