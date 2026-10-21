//! `silo run <block> -- <command…>` (`docs/PHASE7.md` §7.4): type a command
//! into a block's shell, wait for it to finish, print its output and exit
//! with its code.

use std::{io::Write, time::Duration};

use anyhow::{Context, bail};
use silo_client::{Control, Events};
use silo_proto::{
    control::{CommandOutput, CommandRecords, Event, Method},
    model::CommandRecord,
};

/// Run `command` in `block`; returns its exit code.
///
/// # Errors
/// Control failures, no finished command within `timeout`, or a shell that
/// reports no exit code.
pub async fn run(control: &Control, events: &mut Events, block: String, command: &[String], timeout: Option<Duration>) -> anyhow::Result<i32> {
    control.subscribe().await?;
    // Only a command that finishes after this one is ours.
    let before: CommandRecords = control.call(Method::BlockCommands { block_id: block.clone(), limit: Some(1) }).await?;
    let after = before.records.iter().map(|r| r.index).max();
    let mut text = shell_join(command);
    text.push('\r');
    control.call_value(Method::TerminalSend { block_id: block.clone(), text }).await?;
    let wait = finished(events, &block, after);
    let record = match timeout {
        Some(limit) => tokio::time::timeout(limit, wait).await.with_context(|| format!("no command finished within {} s", limit.as_secs()))??,
        None => wait.await?,
    };
    let output: CommandOutput = control.call(Method::TerminalCommandOutput { block_id: block, index: Some(record.index) }).await?;
    let mut out = std::io::stdout().lock();
    if !output.text.is_empty() {
        writeln!(out, "{}", output.text)?;
    }
    out.flush()?;
    if output.truncated {
        eprintln!("(output truncated)");
    }
    match record.exit {
        Some(code) => Ok(code),
        None => bail!("the shell reported no exit code"),
    }
}

/// The next command record of `block` newer than `after`.
async fn finished(events: &mut Events, block: &str, after: Option<u64>) -> anyhow::Result<CommandRecord> {
    while let Some((_, event)) = events.recv().await {
        if let Event::CommandFinished { block_id, record, .. } = event
            && block_id == block
            && after.is_none_or(|after| record.index > after)
        {
            return Ok(record);
        }
    }
    bail!("the server closed the connection")
}

/// Words joined for a POSIX shell: each quoted unless it is plain.
#[must_use]
pub fn shell_join(words: &[String]) -> String {
    let plain = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
    words.iter().map(|w| if plain(w) { w.clone() } else { format!("'{}'", w.replace('\'', r"'\''")) }).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_quoted_only_when_needed() {
        let words = |w: &[&str]| w.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
        assert_eq!(shell_join(&words(&["false"])), "false");
        assert_eq!(shell_join(&words(&["ls", "-la", "./a b"])), "ls -la './a b'");
        assert_eq!(shell_join(&words(&["echo", "it's", ""])), r"echo 'it'\''s' ''");
        assert_eq!(shell_join(&words(&["sh", "-c", "exit 3"])), "sh -c 'exit 3'");
    }
}
