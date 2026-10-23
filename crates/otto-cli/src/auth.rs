use std::{
    io::{self, IsTerminal},
    process::{Command, Stdio},
};

use anyhow::{Result, bail};
use otto_llm::{
    auth::AuthMode,
    providers::{IDS, LoginMode, Providers},
};

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
            providers.discover_local().await;
            println!("PROVIDER\tMODE\tSIGNED IN\tEXPIRES (unix ms)\tACCOUNT\tENDPOINT");
            for status in providers.status() {
                if let Some(error) = status.error {
                    bail!("{error}");
                }
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    status.provider,
                    status.mode.as_deref().unwrap_or("-"),
                    if status.signed_in { "yes" } else { "no" },
                    status.expires_at_ms.map_or_else(|| "-".into(), |v| v.to_string()),
                    status.account_id.as_deref().unwrap_or("-"),
                    status.base_url.as_deref().unwrap_or("-")
                );
            }
            Ok(())
        }
    }
}

async fn login(args: LoginArgs, providers: &Providers) -> Result<()> {
    let provider = match args.provider {
        Some(provider) => provider,
        None => prompt(&format!("Provider ({})", IDS.join("/")), false).await?,
    };
    if !otto_llm::providers::IDS.contains(&provider.as_str()) {
        bail!("unknown provider {provider}; choose one of {}", otto_llm::providers::IDS.join(", "));
    }
    let modes = providers.login_modes(&provider);
    let subscription = modes.contains(&AuthMode::Subscription);
    // Local and custom servers: where they listen. The environment override wins when set.
    if let Some(url) = &args.base_url {
        providers.set_base_url(&provider, Some(url))?;
    } else if let Some(default) = providers.base_url(&provider).or_else(|| (provider == "custom").then(String::new))
        && otto_llm::compat::dialect(&provider).is_some_and(|d| d.discovery.is_some())
        && args.api_key.is_none()
    {
        let url = prompt(&format!("Base URL [{default}]"), false).await?;
        if !url.is_empty() {
            providers.set_base_url(&provider, Some(&url))?;
        } else if default.is_empty() {
            bail!("{provider} needs a base URL");
        }
    }
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
    } else if !subscription {
        LoginMode::ApiKey
    } else if !modes.contains(&AuthMode::ApiKey) {
        LoginMode::Subscription
    } else {
        match prompt("Mode (subscription/api-key)", false).await?.as_str() {
            "subscription" | "" => LoginMode::Subscription,
            "api-key" => LoginMode::ApiKey,
            _ => bail!("choose subscription or api-key"),
        }
    };
    if mode == LoginMode::Subscription && !browser_available() {
        if providers.has_device_login(&provider) {
            mode = LoginMode::Device;
        } else if providers.has_paste_login(&provider) {
            mode = LoginMode::Paste;
        }
    }
    if provider == "antigravity" && mode != LoginMode::ApiKey {
        eprintln!("Google's Antigravity terms forbid third-party use. Your Google account may be suspended.");
        if !matches!(prompt("Continue? [y/N]", false).await?.to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("cancelled");
        }
    }
    let mut flow = providers.begin_login(&provider, mode).await?;
    if let Some(url) = &flow.url {
        eprintln!("Open: {url}");
        if !open_browser(url) && flow.user_code.is_none() && mode == LoginMode::Subscription && providers.has_device_login(&provider) {
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
            None if otto_llm::compat::dialect(&provider).is_some_and(|d| !d.key_required) => prompt("API key (optional)", true).await?,
            None => prompt("API key", true).await?,
        }
    } else if flow.needs_input() {
        prompt("Paste code or URL", false).await?
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
