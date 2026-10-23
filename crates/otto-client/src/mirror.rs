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
    indices: HashMap<String, usize>,
    durable: Option<(Vec<Item>, HashMap<String, ToolState>)>,
    live: Vec<Event>,
}
impl SessionMirror {
    #[must_use]
    pub fn new(view: SessionView) -> Self {
        let indices = view.items.iter().enumerate().map(|(i, item)| (item.id.clone(), i)).collect();
        let tools = view
            .running_tool_calls
            .into_iter()
            .map(|c| {
                (c.call_id, ToolState { name: c.name, input: c.input, status: ToolStatus::Running, started_at: c.started_at, ..ToolState::default() })
            })
            .collect();
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
            indices,
            durable: None,
            live: Vec::new(),
        }
    }
    #[must_use]
    pub fn item(&self, id: &str) -> Option<&Item> {
        self.indices.get(id).map(|i| &self.items[*i])
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
        match event {
            Event::SessionChanged { info } => self.info = info.clone(),
            Event::TurnStarted { turn } => {
                self.turn = Some((*turn, TurnStatus::Running));
                self.turn_started_at = None;
                self.info.status = SessionStatus::Busy;
                self.last_error = None;
            }
            Event::TurnFinished { turn, status, .. } => {
                self.turn = Some((*turn, *status));
                self.turn_started_at = None;
                self.info.status = if *status == TurnStatus::Error { SessionStatus::Error } else { SessionStatus::Idle };
            }
            Event::ItemStarted { item_id, role } => {
                if !self.indices.contains_key(item_id) {
                    self.put(Item::new(item_id, *role, vec![]));
                }
            }
            Event::ItemDone { item } => self.put(item.clone()),
            Event::TextDelta { item_id, .. } | Event::ReasoningDelta { item_id, .. } => {
                if let Some(i) = self.indices.get(item_id) {
                    event.append_delta(&mut self.items[*i].parts);
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
            Event::Usage { usage, cost_usd, .. } => {
                let total = &mut self.info.usage.usage;
                total.input = total.input.saturating_add(usage.input);
                total.output = total.output.saturating_add(usage.output);
                total.cache_read = total.cache_read.saturating_add(usage.cache_read);
                total.cache_write = total.cache_write.saturating_add(usage.cache_write);
                total.reasoning = total.reasoning.saturating_add(usage.reasoning);
                self.info.usage.cost_usd += cost_usd.unwrap_or(0.0);
            }
            Event::Error { class, message, retryable } => {
                self.last_error = Some((class.clone(), message.clone(), *retryable));
                self.info.status = SessionStatus::Error;
            }
            Event::SessionsChanged {} | Event::Compaction { .. } | Event::SubagentChanged { .. } => {}
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
