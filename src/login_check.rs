//! `rustcord --login-check` (debug builds): one real email/password login in
//! the terminal, with the real captcha window. It proves Discord accepts a
//! captcha solved in our window. Prints no token; saves it to the keyring.

use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, bail};
use cord_core::auth::token_store::KeyringStore;
use cord_core::auth::{CaptchaSolution, LoginOutcome, MfaMethod};
use cord_core::{Core, CoreConfig};

/// Login attempts before giving up (so at most two captcha solves).
const LOGIN_ATTEMPTS: usize = 3;

fn prompt(label: &str) -> anyhow::Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

pub fn run() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let identity = cord_core::startup_identity(&cord_core::paths::cache_dir()).await;
        println!("Client build number: {}", identity.build_number);
        let core = Core::new(identity, CoreConfig::default(), Arc::new(KeyringStore::new()), tokio::runtime::Handle::current());

        let email = prompt("Email: ")?;
        let password = rpassword::prompt_password("Password: ")?;
        let mut captcha: Option<CaptchaSolution> = None;
        let mut captcha_seen = false;

        for attempt in 1..=LOGIN_ATTEMPTS {
            let token = match core.login(email.clone(), password.clone(), captcha.take()).await? {
                LoginOutcome::Token(token) => token,
                LoginOutcome::Captcha(challenge) => {
                    captcha_seen = true;
                    if attempt == LOGIN_ATTEMPTS {
                        bail!("Discord kept asking for a captcha: the solved token was not accepted");
                    }
                    println!(
                        "Captcha required (service {}, rqdata {}). Opening the window…",
                        challenge.service,
                        if challenge.rqdata.is_some() { "present" } else { "absent" }
                    );
                    let key = crate::captcha::solve(&challenge).await.context("captcha window closed")?;
                    captcha = Some(CaptchaSolution::for_challenge(&challenge, key));
                    continue;
                }
                LoginOutcome::Mfa(mfa) => {
                    let method = if mfa.totp { MfaMethod::Totp } else { MfaMethod::Backup };
                    let code = prompt(if method == MfaMethod::Totp { "Authenticator code: " } else { "Backup code: " })?;
                    core.submit_mfa(mfa.ticket, method, code).await?
                }
            };
            core.save_token(&token)?;
            println!(
                "Login accepted{}. Token saved to the system keyring.",
                if captcha_seen { " after a captcha solved in our window" } else { " (no captcha was asked)" }
            );
            return Ok(());
        }
        bail!("Discord kept asking for a captcha: the solved token was not accepted")
    })
}
