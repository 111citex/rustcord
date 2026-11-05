//! The captcha window. GPUI owns the main thread and its event loop, and the
//! webview needs its own, so the captcha runs in a short-lived child process:
//! this same binary started with `--captcha`. The parent writes the challenge
//! to the child's stdin; the child prints the solved key to stdout and exits.
//! The human solves the captcha; nothing here automates it.

use std::process::Stdio;

use cord_core::auth::CaptchaChallenge;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct HelperRequest {
    sitekey: String,
    rqdata: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct HelperResponse {
    key: String,
}

const PAGE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>Captcha</title>
<style>
  html, body { margin: 0; height: 100%; background: #1e1f22; color: #dbdee1; font: 14px sans-serif; }
  body { display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 16px; }
</style>
<script>
  const SITEKEY = __SITEKEY__;
  const RQDATA = __RQDATA__;
  function onHcaptchaLoad() {
    const id = hcaptcha.render("captcha", {
      sitekey: SITEKEY,
      theme: "dark",
      callback: (token) => window.ipc.postMessage(token),
    });
    if (RQDATA) hcaptcha.setData(id, { rqdata: RQDATA });
  }
</script>
<script src="https://js.hcaptcha.com/1/api.js?render=explicit&onload=onHcaptchaLoad" async defer></script>
</head>
<body>
  <div>Solve the captcha to finish logging in.</div>
  <div id="captcha"></div>
</body>
</html>
"#;

/// A JS string literal that can't end the surrounding `<script>`.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).expect("strings serialize").replace("</", "<\\/")
}

pub fn page_html(sitekey: &str, rqdata: Option<&str>) -> String {
    PAGE.replace("__RQDATA__", &rqdata.map(js_string).unwrap_or_else(|| "null".into()))
        .replace("__SITEKEY__", &js_string(sitekey))
}

pub fn parse_helper_output(stdout: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(stdout).ok()?;
    let line = text.lines().rev().find(|l| l.trim_start().starts_with('{'))?;
    let response: HelperResponse = serde_json::from_str(line).ok()?;
    (!response.key.is_empty()).then_some(response.key)
}

/// Opens the captcha window and waits for the human to solve it.
pub async fn solve(challenge: &CaptchaChallenge) -> Option<String> {
    use tokio::io::AsyncWriteExt;

    if challenge.service != "hcaptcha" {
        tracing::error!(service = %challenge.service, "unsupported captcha service");
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let mut child = tokio::process::Command::new(exe)
        .arg("--captcha")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .inspect_err(|error| tracing::error!(%error, "couldn't open the captcha window"))
        .ok()?;
    let request = serde_json::to_vec(&HelperRequest {
        sitekey: challenge.sitekey.clone(),
        rqdata: challenge.rqdata.clone(),
    })
    .ok()?;
    let mut stdin = child.stdin.take()?;
    stdin.write_all(&request).await.ok()?;
    drop(stdin); // EOF: the helper reads to the end
    let output = child.wait_with_output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    parse_helper_output(&output.stdout)
}

pub fn helper_main() {
    if let Err(error) = run_helper() {
        eprintln!("captcha window failed: {error:#}");
        std::process::exit(3);
    }
}

fn run_helper() -> anyhow::Result<()> {
    use std::io::Read;

    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::WindowBuilder;

    // WebKitGTK crashes on NVIDIA + Wayland (protocol error 71) without this.
    #[cfg(target_os = "linux")]
    // SAFETY: first thing in this process, before any other thread exists.
    unsafe {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1")
    };

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let request: HelperRequest = serde_json::from_str(&input)?;
    let html = page_html(&request.sitekey, request.rqdata.as_deref());

    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("Rustcord · Captcha")
        .with_inner_size(tao::dpi::LogicalSize::new(420.0, 640.0))
        .build(&event_loop)?;
    let builder = wry::WebViewBuilder::new().with_ipc_handler(move |request: wry::http::Request<String>| {
        let _ = proxy.send_event(request.into_body());
    });

    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use webkit2gtk::WebViewExt;
        use wry::{WebViewBuilderExtUnix, WebViewExtUnix};
        let vbox = window.default_vbox().ok_or_else(|| anyhow::anyhow!("window has no GTK container"))?;
        let webview = builder.build_gtk(vbox)?;
        // discord.com as the document origin, as on the web client.
        webview.webview().load_html(&html, Some("https://discord.com/login"));
        webview
    };
    // Windows: WebView2 origin interception is plan 1c. Until then this
    // loads with an opaque origin.
    #[cfg(not(target_os = "linux"))]
    let webview = builder.with_html(html).build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _alive = (&window, &webview);
        match event {
            Event::UserEvent(key) => {
                // Stdout is a pipe to the parent process, not a log.
                println!("{}", serde_json::json!({ "key": key }));
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
    fn page_embeds_sitekey_and_rqdata_as_js_strings() {
        let html = page_html("site-1", Some("rq\"data"));
        assert!(html.contains(r#"const SITEKEY = "site-1";"#));
        assert!(html.contains(r#"const RQDATA = "rq\"data";"#));
        assert!(html.contains("https://js.hcaptcha.com/1/api.js"));
        assert!(html.contains("window.ipc.postMessage"));
    }

    #[test]
    fn page_without_rqdata_uses_null() {
        assert!(page_html("s", None).contains("const RQDATA = null;"));
    }

    #[test]
    fn rqdata_cannot_close_the_script_tag() {
        let html = page_html("s", Some("</script><img src=x>"));
        assert!(!html.contains("</script><img"));
        assert!(html.contains(r#"<\/script><img src=x>"#));
    }

    #[test]
    fn parses_helper_output() {
        assert_eq!(parse_helper_output(b"{\"key\":\"P1_abc\"}\n").as_deref(), Some("P1_abc"));
        // Stray output from GTK before the JSON line is ignored.
        assert_eq!(parse_helper_output(b"Gtk-Message: hi\n{\"key\":\"P1_abc\"}\n").as_deref(), Some("P1_abc"));
        assert_eq!(parse_helper_output(b""), None);
        assert_eq!(parse_helper_output(b"{\"key\":\"\"}"), None);
    }
}
