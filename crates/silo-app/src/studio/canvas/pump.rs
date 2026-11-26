//! The board's edits on their way to the server: one batch out at a time, in the order made, so they apply in
//! order; a Run waits until every edit made before it is saved (the card it runs may be one of them).

use std::collections::VecDeque;

use silo_proto::studio::{CanvasEdited, Op};

use super::board::{Board, Effect};

/// A batch to send: its number, ops and the rev they were made on.
pub(crate) type Batch = (u64, Vec<Op>, u64);

#[derive(Default)]
pub(crate) struct Pump {
    queue: VecDeque<Batch>,
    /// The batch sent and not answered yet.
    out: Option<u64>,
    /// Cards to run once the edits before them are saved, in order.
    runs: Vec<String>,
}

impl Pump {
    pub fn push(&mut self, batch: Batch) {
        self.queue.push_back(batch);
    }

    /// The next batch to send, when none is out (it is out from now on).
    pub fn next(&mut self) -> Option<Batch> {
        if self.out.is_some() {
            return None;
        }
        let b = self.queue.pop_front()?;
        self.out = Some(b.0);
        Some(b)
    }

    /// Batch `batch` was answered: the board takes the answer (an error: the batch undone, said); its effects.
    pub fn answered(&mut self, board: &mut Board, batch: u64, result: Result<CanvasEdited, String>) -> Vec<Effect> {
        if self.out == Some(batch) {
            self.out = None;
        }
        match result {
            Ok(edited) => board.acked(batch, &edited),
            Err(why) => board.failed(batch, &why),
        }
    }

    /// Run `node` once what was edited before is saved.
    pub fn run(&mut self, node: String) {
        self.runs.push(node);
    }

    /// The runs whose edits are all saved (none while a batch is out or waiting).
    pub fn ready_runs(&mut self) -> Vec<String> {
        if self.out.is_some() || !self.queue.is_empty() {
            return Vec::new();
        }
        std::mem::take(&mut self.runs)
    }
}
