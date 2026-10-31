use otto_proto::*;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyResult {
    Applied,
    Duplicate,
    Gap { expected: i64, got: i64 },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolStatus {
    #[default]
    Input,
    Running,
    Done,
    Error,
}
#[derive(Clone, Debug, Default)]
pub struct ToolState {
    pub name: String,
    pub input_delta: String,
    pub input: Value,
    pub status: ToolStatus,
    pub messages: Vec<String>,
    pub progress: Vec<Value>,
    pub output: Option<Value>,
    pub item_id: Option<String>,
    /// When a call already running in the loaded view started (ms since the Unix epoch).
    pub started_at: Option<i64>,
}
/// A model request waiting to be retried (`model.retrying`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retrying {
    pub class: String,
    pub message: String,
    pub delay_ms: u64,
    /// The attempt about to be made (the first retry is 2).
    pub attempt: u32,
    pub max_attempts: u32,
    /// When the wait began (ms since the Unix epoch).
    pub at_ms: i64,
}
/// Where a compaction is (`compaction.started` → `compaction` or `compaction.failed`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionStatus {
    Running,
    Done,
    Failed { message: String, cancelled: bool },
}
/// A compaction seen live: its summary as it streams, until its checkpoint
/// item (`id`) arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compacting {
    /// The id of the item that holds (or will hold) its checkpoint.
    pub id: String,
    /// `manual` or `auto`; empty when it began before this mirror.
    pub trigger: String,
    pub summary: String,
    pub status: CompactionStatus,
    /// How many items came before it: where it shows among them.
    pub at: usize,
}
#[derive(Clone, Debug)]
pub struct SessionMirror {
    pub info: SessionInfo,
    pub items: Vec<Item>,
    pub tools: HashMap<String, ToolState>,
    pub pending_approvals: Vec<PendingApproval>,
    pub pending_questions: Vec<PendingQuestion>,
    pub queue: Vec<QueuedInput>,
    pub last_seq: i64,
    pub turn: Option<(i64, TurnStatus)>,
    /// When the turn running in the loaded view started (ms since the Unix epoch); a later turn is seen live.
    pub turn_started_at: Option<i64>,
    /// When the loaded items' timed parts ran (the view's journal times); later parts are seen live.
    pub part_times: Vec<PartTime>,
    pub last_error: Option<(String, String, bool)>,
    /// The model request is waiting to be retried (`model.retrying`), until the next stream event.
    pub retrying: Option<Retrying>,
    /// Compactions seen live, in order (`compaction.*`). One leaves when its
    /// checkpoint item arrives; a failed one stays.
    pub compactions: Vec<Compacting>,
    /// Older items are stored than `items` holds: [`SessionMirror::prepend`] a
    /// `session.view` page before `oldest_seq`. Never set by an older server.
    pub has_older: bool,
    /// The item sequence of the oldest stored item loaded (`SessionView::oldest_seq`).
    pub oldest_seq: Option<i64>,
    /// The latest `update_todos` call's input when it is older than the loaded items (`SessionView::todos`).
    pub todos: Option<Value>,
    /// The loaded turns that changed files (`SessionView::turns`, then each `turn.finished` with `changes`,
    /// under the last assistant item loaded), oldest first.
    pub turns: Vec<TurnSummary>,
    /// Each loaded turn's usage (`SessionView::turn_usage`, then the live `usage` steps and `turn.finished`, under
    /// the last assistant item loaded), oldest first. Empty from an older server's view until a turn runs.
    pub turn_usage: Vec<TurnUsage>,
    /// The sequence each stored item was last written at (`SessionView::item_seqs`, then each `item.done`):
    /// what `session.fork` takes to keep the history through it. Empty from an older server's view.
    pub item_seqs: HashMap<String, i64>,
    indices: HashMap<String, usize>,
    durable: Option<(Vec<Item>, HashMap<String, ToolState>)>,
    live: Vec<Event>,
}
impl SessionMirror {
    #[must_use]
    pub fn new(view: SessionView) -> Self {
        let indices = view.items.iter().enumerate().map(|(i, item)| (item.id.clone(), i)).collect();
        let mut tools: HashMap<String, ToolState> = view
            .running_tool_calls
            .into_iter()
            .map(|c| {
                (c.call_id, ToolState { name: c.name, input: c.input, status: ToolStatus::Running, started_at: c.started_at, ..ToolState::default() })
            })
            .collect();
        // Calls the model was still writing when the view was taken: their input so far (later deltas continue it).
        for c in view.streaming_tool_calls {
            tools.entry(c.call_id).or_insert(ToolState { name: c.name, input_delta: c.input_delta, ..ToolState::default() });
        }
        Self {
            info: view.info,
            items: view.items,
            tools,
            pending_approvals: view.pending_approvals,
            pending_questions: view.pending_questions,
            queue: view.queue,
            last_seq: view.seq,
            turn: None,
            turn_started_at: view.turn_started_at,
            part_times: view.part_times,
            last_error: None,
            retrying: None,
            compactions: Vec::new(),
            has_older: view.has_older,
            oldest_seq: view.oldest_seq,
            todos: view.todos,
            turns: view.turns,
            turn_usage: view.turn_usage,
            item_seqs: view.item_seqs.into_iter().collect(),
            indices,
            durable: None,
            live: Vec::new(),
        }
    }
    /// Forget calls the model began writing that no loaded item holds and that never ran (`ToolStatus::Input`):
    /// left by a request that failed and was retried, or a turn that ended, they would otherwise show as
    /// running at the end of the transcript, then as failed.
    fn drop_abandoned_inputs(&mut self) {
        let items = &self.items;
        self.tools.retain(|id, t| t.status != ToolStatus::Input || items.iter().any(|i| i.tool_calls().any(|(c, _, _)| c == id)));
    }

    /// Put a page of older items (`session.view` with `before_seq: oldest_seq`)
    /// before the loaded ones; items already loaded are skipped. Returns how
    /// many were added. A page from an older server (every item, no
    /// `has_older`) adds the ones missing and ends paging.
    pub fn prepend(&mut self, page: SessionView) -> usize {
        let older: Vec<Item> = page.items.into_iter().filter(|i| !self.indices.contains_key(&i.id)).collect();
        for (id, seq) in page.item_seqs {
            self.item_seqs.entry(id).or_insert(seq);
        }
        let added = older.len();
        self.has_older = page.has_older && added > 0;
        self.oldest_seq = page.oldest_seq.or(self.oldest_seq);
        if added == 0 {
            return 0;
        }
        let mut times = page.part_times;
        times.append(&mut self.part_times);
        self.part_times = times;
        let mut turns: Vec<TurnSummary> = page.turns.into_iter().filter(|t| !self.turns.iter().any(|s| s.turn == t.turn)).collect();
        turns.append(&mut self.turns);
        self.turns = turns;
        let mut usage: Vec<TurnUsage> = page.turn_usage.into_iter().filter(|t| !self.turn_usage.iter().any(|s| s.turn == t.turn)).collect();
        usage.append(&mut self.turn_usage);
        self.turn_usage = usage;
        if let Some((items, _)) = &mut self.durable {
            items.splice(0..0, older.iter().cloned());
        }
        self.items.splice(0..0, older);
        self.indices = self.items.iter().enumerate().map(|(i, item)| (item.id.clone(), i)).collect();
        for c in &mut self.compactions {
            c.at += added;
        }
        added
    }
    /// After a resync replaced this mirror with the newest window, keep the
    /// older pages `old` had loaded before it (when the windows overlap).
    pub fn keep_older(&mut self, old: &Self) {
        let Some(first) = self.items.first() else { return };
        let Some(at) = old.items.iter().position(|i| i.id == first.id).filter(|at| *at > 0) else { return };
        let ids: std::collections::HashSet<&str> = old.items[..at].iter().map(|i| i.id.as_str()).collect();
        let page = SessionView {
            items: old.items[..at].to_vec(),
            part_times: old.part_times.iter().filter(|t| ids.contains(t.item_id.as_str())).cloned().collect(),
            has_older: old.has_older,
            oldest_seq: old.oldest_seq,
            todos: None,
            turns: old.turns.iter().filter(|t| t.item_id.as_deref().is_some_and(|id| ids.contains(id))).cloned().collect(),
            turn_usage: old.turn_usage.iter().filter(|t| t.item_id.as_deref().is_some_and(|id| ids.contains(id))).cloned().collect(),
            item_seqs: old.item_seqs.iter().filter(|(id, _)| ids.contains(id.as_str())).map(|(id, seq)| (id.clone(), *seq)).collect(),
            info: self.info.clone(),
            pending_approvals: Vec::new(),
            pending_questions: Vec::new(),
            queue: Vec::new(),
            running_tool_calls: Vec::new(),
            streaming_tool_calls: Vec::new(),
            seq: self.last_seq,
            turn_started_at: None,
        };
        self.prepend(page);
    }
    #[must_use]
    pub fn item(&self, id: &str) -> Option<&Item> {
        self.indices.get(id).map(|i| &self.items[*i])
    }
    /// `turn`'s usage, added (after the others) when it has none yet.
    fn turn_entry(&mut self, turn: i64) -> &mut TurnUsage {
        let at = match self.turn_usage.iter().position(|t| t.turn == turn) {
            Some(at) => at,
            None => {
                self.turn_usage.push(TurnUsage { turn, ..TurnUsage::default() });
                self.turn_usage.len() - 1
            }
        };
        &mut self.turn_usage[at]
    }
    /// The usage of the turn whose reply is `item_id`.
    #[must_use]
    pub fn turn_usage_of(&self, item_id: &str) -> Option<&TurnUsage> {
        self.turn_usage.iter().find(|t| t.item_id.as_deref() == Some(item_id))
    }
    fn put(&mut self, item: Item) {
        if let Some(i) = self.indices.get(&item.id) {
            self.items[*i] = item;
        } else {
            self.indices.insert(item.id.clone(), self.items.len());
            self.items.push(item);
        }
    }
    /// Apply a display-only overlay at the current durable boundary. Older
    /// anchors are stale; future anchors require replay before display.
    /// Persisted deltas consume matching overlay prefixes; ItemDone is authoritative.
    pub fn apply_live(&mut self, after_seq: i64, event: &Event) -> ApplyResult {
        if after_seq < self.last_seq || !event.is_live_delta() {
            return ApplyResult::Duplicate;
        }
        if after_seq > self.last_seq {
            return ApplyResult::Gap { expected: self.last_seq, got: after_seq };
        }
        // Compaction text is never journaled, so nothing replaces it: it adds up in place.
        if let Event::CompactionDelta { .. } = event {
            self.project(event);
            return ApplyResult::Applied;
        }
        if self.durable.is_none() {
            self.durable = Some((self.items.clone(), self.tools.clone()));
        }
        self.live.push(event.clone());
        self.project(event);
        ApplyResult::Applied
    }
    /// Discard ephemeral display state when replacing a transport.
    pub fn clear_live(&mut self) {
        if let Some((items, tools)) = self.durable.take() {
            self.items = items;
            self.tools = tools;
        }
        self.live.clear();
    }
    /// Duplicate sequences never mutate state; a gap leaves the mirror untouched.
    pub fn apply(&mut self, seq: i64, event: &Event) -> ApplyResult {
        let result = self.advance(seq);
        if result != ApplyResult::Applied {
            return result;
        }
        if let Some((items, tools)) = self.durable.take() {
            self.items = items;
            self.tools = tools;
        }
        Self::reconcile(&mut self.live, event);
        self.project(event);
        // Its journal sequence is the stored item's version.
        if let Event::ItemDone { item } = event {
            self.item_seqs.insert(item.id.clone(), seq);
        }
        self.info.last_seq = seq;
        if !self.live.is_empty() {
            self.durable = Some((self.items.clone(), self.tools.clone()));
            for overlay in self.live.clone() {
                self.project(&overlay);
            }
        }
        result
    }
    fn project(&mut self, event: &Event) {
        // Any progress (or the turn's end) means the retry wait is over.
        if !matches!(
            event,
            Event::ModelRetrying { .. }
                | Event::SessionsChanged {}
                | Event::SessionChanged { .. }
                | Event::PlanUsage { .. }
                | Event::AuthChanged {}
                | Event::ServerStarting { .. }
                | Event::ServerReady {}
                | Event::McpStatus { .. }
                | Event::McpChanged {}
                | Event::QueueChanged { .. }
                | Event::SubagentChanged { .. }
        ) {
            self.retrying = None;
        }
        match event {
            Event::SessionChanged { info } => self.info = info.clone(),
            Event::TurnStarted { turn } => {
                self.turn = Some((*turn, TurnStatus::Running));
                self.turn_started_at = None;
                self.info.status = SessionStatus::Busy;
                self.last_error = None;
            }
            Event::TurnFinished { turn, status, changes, usage } => {
                let item_id = self.items.iter().rev().find(|i| i.role == Role::Assistant).map(|i| i.id.clone());
                if let Some(changes) = changes {
                    self.turns.retain(|t| t.turn != *turn);
                    self.turns.push(TurnSummary { turn: *turn, item_id: item_id.clone(), changes: changes.clone() });
                }
                // The turn's own sum is authoritative (steps may have run before the view); its cost is the steps'.
                if *usage != Usage::default() {
                    let entry = self.turn_entry(*turn);
                    entry.usage = usage.clone();
                    entry.item_id = item_id;
                }
                // A compaction still running when its turn ended never reported its end: the turn stopped it.
                for c in self.compactions.iter_mut().filter(|c| c.status == CompactionStatus::Running) {
                    c.status = CompactionStatus::Failed { message: "Stopped".into(), cancelled: true };
                }
                self.turn = Some((*turn, *status));
                self.turn_started_at = None;
                self.info.status = if *status == TurnStatus::Error { SessionStatus::Error } else { SessionStatus::Idle };
                // A turn that finished well wrote every call it ran; one stopped or failed shows what it cut off.
                if *status == TurnStatus::Done {
                    self.drop_abandoned_inputs();
                }
            }
            Event::ItemStarted { item_id, role } => {
                if !self.indices.contains_key(item_id) {
                    self.put(Item::new(item_id, *role, vec![]));
                }
            }
            Event::ItemDone { item } => {
                // Its checkpoint is stored: the item shows it from here.
                if item.parts.iter().any(|p| matches!(p, Part::Compaction { .. })) {
                    self.compactions.retain(|c| c.id != item.id);
                }
                self.put(item.clone());
                // A finished reply holds every call its step made: one still being written outside it belonged
                // to an attempt that was dropped (a retried request).
                if item.role == Role::Assistant {
                    self.drop_abandoned_inputs();
                }
            }
            Event::TextDelta { item_id, .. } | Event::ReasoningDelta { item_id, .. } => {
                if let Some(i) = self.indices.get(item_id) {
                    event.append_delta(&mut self.items[*i].parts);
                }
            }
            Event::ToolInputStart { call_id, name } => {
                // The call exists from here, its input still streaming (`ToolStatus::Input`).
                let t = self.tools.entry(call_id.clone()).or_default();
                if t.name.is_empty() {
                    t.name.clone_from(name);
                }
            }
            Event::ToolInputDelta { call_id, delta } => self.tools.entry(call_id.clone()).or_default().input_delta.push_str(delta),
            Event::ToolCall { call_id, name, input } => {
                let t = self.tools.entry(call_id.clone()).or_default();
                t.name = name.clone();
                t.input = input.clone();
                t.status = ToolStatus::Running;
            }
            Event::ToolProgress { call_id, message, output } => {
                let t = self.tools.entry(call_id.clone()).or_default();
                t.messages.extend(message.clone());
                t.progress.extend(output.clone());
            }
            Event::ToolResult { call_id, is_error, output, item_id } => {
                let t = self.tools.entry(call_id.clone()).or_default();
                t.status = if *is_error { ToolStatus::Error } else { ToolStatus::Done };
                t.output = Some(output.clone());
                t.item_id = Some(item_id.clone());
            }
            Event::ApprovalRequested { approval_id, call_id, tool, summary, input, effects } => {
                self.pending_approvals.retain(|a| a.approval_id != *approval_id);
                self.pending_approvals.push(PendingApproval {
                    approval_id: approval_id.clone(),
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    summary: summary.clone(),
                    input: input.clone(),
                    effects: effects.clone(),
                });
                self.info.status = SessionStatus::WaitingApproval;
            }
            Event::ApprovalResolved { approval_id, .. } => {
                self.pending_approvals.retain(|a| a.approval_id != *approval_id);
                if self.pending_approvals.is_empty() && self.info.status == SessionStatus::WaitingApproval {
                    self.info.status = SessionStatus::Busy;
                }
            }
            Event::QuestionAsked { question_id, call_id, question, options } => {
                self.pending_questions.retain(|q| q.question_id != *question_id);
                self.pending_questions.push(PendingQuestion {
                    question_id: question_id.clone(),
                    call_id: call_id.clone(),
                    question: question.clone(),
                    options: options.clone(),
                });
            }
            Event::QuestionResolved { question_id, .. } => self.pending_questions.retain(|q| q.question_id != *question_id),
            Event::QueueChanged { queued } => self.queue = queued.clone(),
            Event::Usage { turn, usage, cost_usd, context, api_cost_usd, provider, model, .. } => {
                // An older server names no model: the session's (a step runs on it).
                let provider = provider.clone().unwrap_or_else(|| self.info.provider.clone());
                let model = model.clone().unwrap_or_else(|| self.info.model.clone());
                let cost = cost_usd.unwrap_or(0.0);
                self.turn_entry(*turn).add_step(&provider, &model, usage, cost, *api_cost_usd);
                self.info.context = *context;
                self.info.usage.add_step(&provider, &model, usage, cost, *api_cost_usd);
            }
            Event::Error { class, message, retryable } => {
                self.last_error = Some((class.clone(), message.clone(), *retryable));
                self.info.status = SessionStatus::Error;
            }
            Event::ModelRetrying { class, message, delay_ms, attempt, max_attempts, at_ms } => {
                self.retrying = Some(Retrying {
                    class: class.clone(),
                    message: message.clone(),
                    delay_ms: *delay_ms,
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                    at_ms: *at_ms,
                });
                // The failed request's calls never ran; the retry writes its own.
                self.drop_abandoned_inputs();
            }
            Event::CompactionStarted { id, trigger } => {
                self.compactions.retain(|c| c.id != *id);
                self.compactions.push(Compacting {
                    id: id.clone(),
                    trigger: trigger.clone(),
                    summary: String::new(),
                    status: CompactionStatus::Running,
                    at: self.items.len(),
                });
            }
            Event::CompactionDelta { id, text } => match self.compactions.iter_mut().find(|c| c.id == *id) {
                Some(c) => c.summary.push_str(text),
                // Stored already: a straggler.
                None if self.indices.get(id).is_some_and(|i| self.items[*i].parts.iter().any(|p| matches!(p, Part::Compaction { .. }))) => {}
                // Begun before this mirror's view: shown from here on.
                None => self.compactions.push(Compacting {
                    id: id.clone(),
                    trigger: String::new(),
                    summary: text.clone(),
                    status: CompactionStatus::Running,
                    at: self.items.len(),
                }),
            },
            Event::Compaction { summary, id, .. } => {
                self.info.context.used = 0;
                // An older server names none: the running one ended.
                let done = self.compactions.iter_mut().filter(|c| id.as_ref().map_or(c.status == CompactionStatus::Running, |id| c.id == *id));
                for c in done {
                    c.status = CompactionStatus::Done;
                    if let Some(summary) = summary {
                        c.summary.clone_from(summary);
                    }
                }
            }
            Event::CompactionFailed { id, message, cancelled } => {
                if let Some(c) = self.compactions.iter_mut().find(|c| c.id == *id) {
                    c.status = CompactionStatus::Failed { message: message.clone(), cancelled: *cancelled };
                }
            }
            Event::PlanUsage { provider, usage } => {
                if *provider == self.info.provider {
                    self.info.plan = Some(usage.clone());
                }
            }
            Event::SessionsChanged {}
            | Event::SubagentChanged { .. }
            | Event::AuthChanged {}
            | Event::ServerStarting { .. }
            | Event::ServerReady {}
            | Event::McpStatus { .. }
            | Event::McpChanged {} => {}
        }
    }
    /// Advance past a forward-compatible unknown event without projecting it.
    pub fn advance(&mut self, seq: i64) -> ApplyResult {
        if seq <= self.last_seq {
            return ApplyResult::Duplicate;
        }
        let expected = self.last_seq.saturating_add(1);
        if seq != expected {
            return ApplyResult::Gap { expected, got: seq };
        }
        self.last_seq = seq;
        self.info.last_seq = seq;
        ApplyResult::Applied
    }

    fn delta_key(event: &Event) -> Option<(u8, &str)> {
        match event {
            Event::TextDelta { item_id, .. } => Some((0, item_id)),
            Event::ReasoningDelta { item_id, summary, .. } => Some((if *summary { 1 } else { 2 }, item_id)),
            Event::ToolInputDelta { call_id, .. } => Some((3, call_id)),
            Event::ToolProgress { call_id, .. } => Some((4, call_id)),
            _ => None,
        }
    }
    fn delta_text(event: &mut Event) -> Option<&mut String> {
        match event {
            Event::TextDelta { text, .. } | Event::ReasoningDelta { text, .. } => Some(text),
            Event::ToolInputDelta { delta, .. } => Some(delta),
            _ => None,
        }
    }
    fn reconcile(live: &mut Vec<Event>, persisted: &Event) {
        if let Event::ItemDone { item } = persisted {
            live.retain(|event| !matches!(Self::delta_key(event), Some((0..=2, id)) if id == item.id));
        }
        if let Event::ToolCall { call_id, .. } | Event::ToolResult { call_id, .. } = persisted {
            live.retain(|event| !matches!(Self::delta_key(event), Some((3..=4, id)) if id == call_id));
        }
        if matches!(persisted, Event::TurnFinished { .. }) {
            live.clear();
        }
        let Some(key) = Self::delta_key(persisted) else {
            return;
        };
        if let Event::ToolProgress { call_id, message, output } = persisted {
            if let Some(index) = live.iter().position(|event| {
                matches!(event,
                    Event::ToolProgress { call_id: id, message: m, output: o } if id == call_id && m == message && o == output
                )
            }) {
                live.remove(index);
            }
            return;
        }
        let mut persisted = persisted.clone();
        let mut remaining = Self::delta_text(&mut persisted).cloned().unwrap_or_default();
        // An empty record (a thinking block that streams no text) consumes its empty live twin; left in the
        // overlay it would be projected again after every later record, a new empty part at the item's end.
        if remaining.is_empty() {
            if let Some(i) = live.iter_mut().position(|e| Self::delta_key(e) == Some(key) && Self::delta_text(e).is_some_and(|t| t.is_empty())) {
                live.remove(i);
            }
            return;
        }
        // Live deltas start where the subscription did, which may be inside this record (a block
        // opened mid-reply): skip the record's head the overlay never saw.
        let overlay: String =
            live.iter().filter(|e| Self::delta_key(e) == Some(key)).filter_map(|e| Self::delta_text(&mut e.clone()).cloned()).collect();
        if !remaining.starts_with(&overlay)
            && !overlay.starts_with(&remaining)
            && let Some(i) = remaining.char_indices().skip(1).map(|(i, _)| i).find(|i| overlay.starts_with(&remaining[*i..]))
        {
            remaining.drain(..i);
        }
        live.retain_mut(|event| {
            if Self::delta_key(event) != Some(key) {
                return true;
            }
            let Some(text) = Self::delta_text(event) else {
                return false;
            };
            if remaining.is_empty() {
                return true;
            }
            if let Some(rest) = remaining.strip_prefix(text.as_str()) {
                remaining = rest.to_owned();
                false
            } else if let Some(rest) = text.strip_prefix(&remaining) {
                *text = rest.to_owned();
                remaining.clear();
                !text.is_empty()
            } else {
                // A lossy live stream may miss a prefix. Durable data always wins.
                false
            }
        });
    }
}
impl From<SessionView> for SessionMirror {
    fn from(view: SessionView) -> Self {
        Self::new(view)
    }
}
