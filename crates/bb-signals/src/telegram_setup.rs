//! Interactive one-time Telegram user-account login for the ingester.
//!
//! Reads `TELEGRAM_API_ID` / `TELEGRAM_API_HASH`, the phone number from
//! `TELEGRAM_PHONE` (prompted when unset), the login code and, for 2FA
//! accounts, the cloud password (read without echo). The authorised session is
//! saved to `TELEGRAM_SESSION_PATH` (default
//! `data/telegram-session.grammers.json`). Prompts and status go to stderr.

use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Arc;

use grammers_client::{Client, SenderPool, SignInError};

use crate::telegram_ingester::session_path_from_env;
use crate::telegram_session::JsonFileSession;

fn prompt(message: &str) -> std::io::Result<String> {
    let mut stderr = std::io::stderr().lock();
    stderr.write_all(message.as_bytes())?;
    stderr.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

/// Returns the process exit code: 0 success, 1 auth failure, 2 config error.
pub async fn run() -> u8 {
    let api_id = bb_core::env::string(&["TELEGRAM_API_ID"], "");
    let api_hash = bb_core::env::string(&["TELEGRAM_API_HASH"], "");
    let session_path = session_path_from_env();
    if api_id.is_empty() || api_hash.is_empty() {
        eprintln!(
            "TELEGRAM_API_ID and TELEGRAM_API_HASH are required. Create them at https://my.telegram.org/apps and export them first."
        );
        return 2;
    }
    let Ok(api_id) = api_id.parse::<i32>() else {
        eprintln!("TELEGRAM_API_ID must be an integer.");
        return 2;
    };
    match login(api_id, &api_hash, &session_path).await {
        Ok(()) => {
            eprintln!("Telegram session saved: {}", session_path.display());
            eprintln!("You can now start: bb signals-telegram-ingester --daemon");
            0
        }
        Err(err) => {
            eprintln!("Telegram authentication failed: {err}");
            eprintln!("Retry with: bb signals-telegram-setup");
            1
        }
    }
}

async fn login(api_id: i32, api_hash: &str, session_path: &Path) -> anyhow::Result<()> {
    let session = Arc::new(JsonFileSession::open_or_create(session_path)?);
    let SenderPool { runner, handle, .. } = SenderPool::new(Arc::clone(&session), api_id);
    let client = Client::new(handle.clone());
    let pool_task = tokio::spawn(runner.run());

    let result = async {
        if client.is_authorized().await? {
            eprintln!("Telegram session is already authorized.");
        } else {
            let phone = match bb_core::env::optional_string(&["TELEGRAM_PHONE"]) {
                Some(phone) => phone,
                None => prompt("Enter your phone number (international format): ")?,
            };
            let token = client.request_login_code(&phone, api_hash).await?;
            let code = prompt("Enter the code you received: ")?;
            match client.sign_in(&token, &code).await {
                Ok(_) => {}
                Err(SignInError::PasswordRequired(password_token)) => {
                    let hint = password_token.hint().unwrap_or("none").to_owned();
                    let password = rpassword::prompt_password(format!(
                        "Enter your 2FA password (hint: {hint}): "
                    ))?;
                    client
                        .check_password(password_token, password.trim())
                        .await?;
                }
                Err(err) => return Err(anyhow::Error::new(err)),
            }
        }
        session.save().await?;
        anyhow::Ok(())
    }
    .await;

    handle.quit();
    let _ = pool_task.await;
    result?;
    if !session_path.exists() {
        anyhow::bail!(
            "auth completed, but the session file was not found at {}; check TELEGRAM_SESSION_PATH",
            session_path.display()
        );
    }
    Ok(())
}
