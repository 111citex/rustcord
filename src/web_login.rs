//! Logging in on Discord's own page, in a throwaway window: captchas, 2FA,
//! passkeys and the rest are all Discord's doing, not ours. Like the captcha
//! window, the webview needs its own event loop, so this is a short-lived child
//! process: this same binary started with `--web-login`. It prints the account's
//! token to its stdout (a pipe to the parent, never a log or an argument) and
//! exits. Nothing it stored outlives it: the webview is incognito.

use std::process::{Command, Stdio};

const LOGIN_URL: &str = "https://discord.com/login";

/// Watches Discord's own page for its first authorized request, on Discord's
/// own origin only (captcha frames load this script too).
const WATCH_FOR_TOKEN: &str = r#"(function () {
  const host = location.hostname;
  if (location.protocol !== "https:" || (host !== "discord.com" && !host.endsWith(".discord.com"))) return;
  let sent = false;
  const send = (value) => {
    if (sent || typeof value !== "string" || value.length < 20) return;
    sent = true;
    window.ipc.postMessage(value);
  };
  const setHeader = XMLHttpRequest.prototype.setRequestHeader;
  XMLHttpRequest.prototype.setRequestHeader = function (name, value) {
    if (String(name).toLowerCase() === "authorization") send(value);
    return setHeader.apply(this, arguments);
  };
  const realFetch = window.fetch;
  window.fetch = function (input, init) {
    try {
      const headers = (init && init.headers) || (input && input.headers);
      if (headers) send(headers.get ? headers.get("authorization") : headers.authorization || headers.Authorization);
    } catch (_) {}
    return realFetch.apply(this, arguments);
  };
})();"#;

/// Whether a message from the page came from Discord itself.
fn from_discord(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#', ':']).next().unwrap_or("");
    host == "discord.com" || host.ends_with(".discord.com")
}

/// Opens the window and waits; the token, or `None` if it was just closed.
/// Blocks: run it off the UI thread.
pub fn run() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let output = Command::new(exe)
        .arg("--web-login")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .inspect_err(|error| tracing::error!(%error, "couldn't open the login window"))
        .ok()?;
    if !output.status.success() {
        return None;
    }
    crate::captcha::parse_helper_output(&output.stdout)
}

pub fn helper_main() {
    if let Err(error) = run_helper() {
        eprintln!("login window failed: {error:#}");
        std::process::exit(3);
    }
}

fn run_helper() -> anyhow::Result<()> {
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;

    // WebKitGTK crashes on NVIDIA + Wayland (protocol error 71) without this.
    #[cfg(target_os = "linux")]
    // SAFETY: first thing in this process, before any other thread exists.
    unsafe {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1")
    };

    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("Rustcord · Log in to Discord")
        .with_inner_size(tao::dpi::LogicalSize::new(1000.0, 760.0))
        .build(&event_loop)?;
    // The same browser the rest of the app says it is.
    let user_agent = cord_core::identity::Identity::new(0).user_agent();
    let builder = wry::WebViewBuilder::new()
        .with_url(LOGIN_URL)
        .with_incognito(true)
        .with_user_agent(&user_agent)
        .with_initialization_script(WATCH_FOR_TOKEN)
        .with_ipc_handler(move |request: wry::http::Request<String>| {
            if from_discord(&request.uri().to_string()) {
                let _ = proxy.send_event(request.into_body());
            }
        });

    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().ok_or_else(|| anyhow::anyhow!("window has no GTK container"))?;
        builder.build_gtk(vbox)?
    };
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _alive = (&window, &webview);
        match event {
            Event::UserEvent(token) => {
                // Stdout is a pipe to the parent process, not a log.
                println!("{}", serde_json::json!({ "key": token }));
                std::process::exit(0);
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => std::process::exit(2),
            _ => {}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_discord_itself_may_answer() {
        assert!(from_discord("https://discord.com/login"));
        assert!(from_discord("https://canary.discord.com/app"));
        assert!(!from_discord("https://discord.com.evil.example/login"));
        assert!(!from_discord("https://newassets.hcaptcha.com/x"));
        assert!(!from_discord("http://discord.com/login"));
    }
}
