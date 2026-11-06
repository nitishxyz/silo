//! `otto-silo media`: media providers' keys and status (docs/MEDIA.md), straight in the credential store like `auth`.
use anyhow::{Result, bail};
use clap::Subcommand;
use std::io::IsTerminal;

#[derive(Subcommand)]
pub(crate) enum MediaCommand {
    /// Media providers, where their keys come from, and each kind's default.
    Status,
    /// Set a provider's media key: typed without echo, or piped on stdin (Higgsfield: id:secret).
    Key {
        provider: String,
        /// Remove the media key instead (a chat or environment key may still apply).
        #[arg(long)]
        remove: bool,
    },
}

pub(crate) async fn run(command: MediaCommand, providers: &otto_llm::Providers, home: std::path::PathBuf) -> Result<()> {
    let media = otto_media::Media::new(providers.credentials().clone(), home);
    match command {
        MediaCommand::Status => {
            let status = media.status();
            for p in &status.providers {
                let kinds: Vec<&str> = p.kinds.iter().map(|k| kind(*k)).collect();
                let source = match p.source {
                    Some(otto_proto::MediaKeySource::Media) => "media key",
                    Some(otto_proto::MediaKeySource::Chat) => "chat API key",
                    Some(otto_proto::MediaKeySource::Env) => "environment",
                    None => "not connected",
                };
                println!("{}\t{}\t{}\t{source}", p.id, p.name, kinds.join(","));
            }
            for (k, choice) in &status.defaults {
                println!("default {}\t{}\t{}", kind(*k), choice.provider.as_deref().unwrap_or("auto"), choice.model.as_deref().unwrap_or("-"));
            }
            Ok(())
        }
        MediaCommand::Key { provider, remove } => {
            let key = if remove {
                None
            } else if std::io::stdin().is_terminal() {
                Some(crate::prompt("API key", true).await?)
            } else {
                Some(crate::stdin_text().await?.trim().to_owned())
            };
            if key.as_deref() == Some("") {
                bail!("no key given; use --remove to remove one");
            }
            let info = media.set_key(&provider, key)?;
            println!("{}: {}", info.name, if info.connected { "connected" } else { "not connected" });
            Ok(())
        }
    }
}

fn kind(kind: otto_proto::MediaKind) -> &'static str {
    match kind {
        otto_proto::MediaKind::Image => "image",
        otto_proto::MediaKind::Video => "video",
        otto_proto::MediaKind::Audio => "audio",
    }
}

/// `@` model mentions resolved through the media catalog (docs/MEDIA.md §3.0.1).
pub(crate) struct MentionModels(pub(crate) std::sync::Arc<otto_media::Media>);

impl otto_core::MentionModels for MentionModels {
    fn resolve(&self, handle: &str) -> Option<otto_core::MentionedModel> {
        let (provider, _, model, name, kind) = self.0.resolve_handle(handle)?;
        Some(otto_core::MentionedModel { handle: handle.trim().to_ascii_lowercase(), provider, model, name, kind })
    }
}
