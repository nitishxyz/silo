//! Shell command records (OSC 133, Phase 7 §7.3): what each command was,
//! how it ended and how long it took. The block actor keeps a
//! [`CommandLog`] and the row anchors that go with it.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use silo_proto::{
    api::method_name,
    control::{CommandRecords, Method, RpcError},
    model::CommandRecord,
    text::{MAX_COMMAND, untrusted},
};

use crate::{Server, block::BlockMsg};

/// Finished commands kept per block.
pub const MAX_COMMANDS: usize = 1000;

/// A block's last [`MAX_COMMANDS`] commands, oldest first, and the one
/// running (C without D yet).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandLog {
    records: VecDeque<CommandRecord>,
    #[serde(default)]
    running: Option<CommandRecord>,
    /// The next record's index (monotonic per block).
    #[serde(default)]
    next: u64,
}

impl CommandLog {
    fn index(&mut self) -> u64 {
        let index = self.next;
        self.next += 1;
        index
    }

    /// C: a command starts; returns its index. A C without the previous
    /// command's D replaces that one.
    pub fn start(&mut self, command: &str, now_ms: u64) -> u64 {
        let command = Some(untrusted(command, MAX_COMMAND)).filter(|c| !c.is_empty());
        let index = self.index();
        self.running = Some(CommandRecord { index, command, exit: None, started_at: Some(now_ms), duration_ms: None });
        index
    }

    /// A: a new prompt without a D for the running command: it is dropped.
    pub fn abandon(&mut self) -> Option<u64> {
        self.running.take().map(|r| r.index)
    }

    /// D: the running command finished (or, without a C, one with no start
    /// or duration). Returns it and the indexes evicted to make room.
    pub fn end(&mut self, exit: Option<i32>, now_ms: u64) -> (CommandRecord, Vec<u64>) {
        let record = match self.running.take() {
            Some(running) => CommandRecord { exit, duration_ms: running.started_at.map(|start| now_ms.saturating_sub(start)), ..running },
            None => CommandRecord { index: self.index(), command: None, exit, started_at: None, duration_ms: None },
        };
        self.records.push_back(record.clone());
        let mut evicted = Vec::new();
        while self.records.len() > MAX_COMMANDS {
            evicted.extend(self.records.pop_front().map(|r| r.index));
        }
        (record, evicted)
    }

    /// The running command's index.
    #[must_use]
    pub fn running(&self) -> Option<u64> {
        self.running.as_ref().map(|r| r.index)
    }

    /// The last `limit` records (all when `None`), oldest first, the
    /// running one last.
    #[must_use]
    pub fn records(&self, limit: Option<u32>) -> Vec<CommandRecord> {
        let all: Vec<CommandRecord> = self.records.iter().chain(&self.running).cloned().collect();
        let keep = limit.map_or(all.len(), |l| usize::try_from(l).unwrap_or(usize::MAX).min(all.len()));
        all[all.len() - keep..].to_vec()
    }

    /// A finished record: `index`, or the last one.
    #[must_use]
    pub fn finished(&self, index: Option<u64>) -> Option<&CommandRecord> {
        match index {
            Some(index) => self.records.iter().find(|r| r.index == index),
            None => self.records.back(),
        }
    }

    /// Indexes of the finished records.
    pub fn indexes(&self) -> impl Iterator<Item = u64> + '_ {
        self.records.iter().map(|r| r.index)
    }
}

pub async fn handle(server: &Server, _client_id: u64, method: Method) -> Result<Value, RpcError> {
    let gone = |block: &str| RpcError::not_found(format!("block {block}"));
    match method {
        Method::BlockCommands { block_id, limit } => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            server.registry.lock().unwrap().block(&block_id)?.send(BlockMsg::Commands { limit, reply });
            let records = rx.await.map_err(|_| gone(&block_id))?;
            serde_json::to_value(CommandRecords { records }).map_err(RpcError::internal)
        }
        Method::TerminalCommandOutput { block_id, index } => {
            let (reply, rx) = tokio::sync::oneshot::channel();
            server.registry.lock().unwrap().block(&block_id)?.send(BlockMsg::CommandOutput { index, reply });
            let output = rx.await.map_err(|_| gone(&block_id))??;
            serde_json::to_value(output).map_err(RpcError::internal)
        }
        method => Err(RpcError::method_not_found(&method_name(&method))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_starts_and_d_finishes_a_record() {
        let mut log = CommandLog::default();
        let index = log.start("cargo\u{202e} build", 1000);
        assert_eq!(log.records(None).last().map(|r| (r.index, r.exit)), Some((index, None)), "running shows last");
        let (record, evicted) = log.end(Some(1), 3500);
        assert!(evicted.is_empty());
        assert_eq!(
            record,
            CommandRecord { index, command: Some("cargo build".into()), exit: Some(1), started_at: Some(1000), duration_ms: Some(2500) }
        );
        let (bare, _) = log.end(Some(0), 4000);
        assert_eq!((bare.index, bare.started_at, bare.duration_ms, bare.command), (index + 1, None, None, None), "D without C");
        assert_eq!(log.finished(None).map(|r| r.index), Some(index + 1));
        assert_eq!(log.finished(Some(index)).and_then(|r| r.exit), Some(1));
        log.start("vim", 5000);
        assert_eq!(log.abandon(), Some(index + 2));
        assert_eq!(log.records(Some(1)).len(), 1);
    }

    #[test]
    fn keeps_the_last_thousand() {
        let mut log = CommandLog::default();
        let mut evicted = Vec::new();
        for i in 0..MAX_COMMANDS as u64 + 3 {
            log.start("true", i);
            evicted.extend(log.end(Some(0), i).1);
        }
        assert_eq!(evicted, [0, 1, 2]);
        let records = log.records(None);
        assert_eq!((records.len(), records[0].index), (MAX_COMMANDS, 3));
    }
}
