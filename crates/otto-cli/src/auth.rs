use std::{
    io::{self, IsTerminal},
    process::{Command, Stdio},
};

use anyhow::{Result, bail};
use otto_llm::providers::{LoginMode, Providers};

use crate::{AuthCommand, LoginArgs, prompt};

pub async fn run(command: AuthCommand, providers: &Providers) -> Result<()> {
    match command {
        AuthCommand::Login(args) => login(args, providers).await,
        AuthCommand::Logout { provider } => {
            providers.logout(&provider)?;
            eprintln!("Removed stored credentials for {provider}. Environment API keys are unchanged.");
            Ok(())
        }
        AuthCommand::Status => {
            println!("PROVIDER\tMODE\tSIGNED IN\tEXPIRES (unix ms)\tACCOUNT");
            for status in providers.status() {
                if let Some(error) = status.error {
                    bail!("{error}");
                }
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    status.provider,
                    status.mode.as_deref().unwrap_or("-"),
                    if status.signed_in { "yes" } else { "no" },
                    status.expires_at_ms.map_or_else(|| "-".into(), |v| v.to_string()),
                    status.account_id.as_deref().unwrap_or("-")
                );
            }
            Ok(())
        }
    }
}

async fn login(args: LoginArgs, providers: &Providers) -> Result<()> {
    let provider = match args.provider {
        Some(provider) => provider,
        None => prompt("Provider (anthropic/openai)", false).await?,
    };
    let mut mode = if args.api_key.is_some() {
        LoginMode::ApiKey
    } else if args.device {
        LoginMode::Device
    } else if args.max {
        LoginMode::Max
    } else if args.console {
        LoginMode::Console
    } else if args.subscription {
        LoginMode::Subscription
    } else {
        match prompt("Mode (subscription/api-key)", false).await?.as_str() {
            "subscription" | "" => LoginMode::Subscription,
            "api-key" => LoginMode::ApiKey,
            _ => bail!("choose subscription or api-key"),
        }
    };
    if provider == "openai" && mode == LoginMode::Subscription && !browser_available() {
        mode = LoginMode::Device;
    }
    let mut flow = providers.begin_login(&provider, mode).await?;
    if let Some(url) = &flow.url {
        eprintln!("Open: {url}");
        if !open_browser(url) && provider == "openai" && mode == LoginMode::Subscription {
            drop(flow);
            flow = providers.begin_login(&provider, LoginMode::Device).await?;
            if let Some(url) = &flow.url {
                eprintln!("Open: {url}");
            }
        }
    }
    if let Some(code) = &flow.user_code {
        eprintln!("Device code: {code}");
    }
    let input = if mode == LoginMode::ApiKey {
        match args.api_key.filter(|key| !key.is_empty()) {
            Some(key) => key,
            None => prompt("API key", true).await?,
        }
    } else if flow.needs_input() {
        prompt("Paste code#state", false).await?
    } else {
        String::new()
    };
    providers.complete_login(flow, &input).await?;
    eprintln!("Signed in to {provider}.");
    Ok(())
}

fn browser_available() -> bool {
    io::stdin().is_terminal() && (cfg!(target_os = "macos") || std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some())
}
fn open_browser(url: &str) -> bool {
    if !browser_available() {
        return false;
    }
    Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" })
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
