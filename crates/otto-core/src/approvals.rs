use crate::{
    QuestionReply, Result,
    session::{Answered, Context},
};
use otto_proto::{ApprovalDecision, Elicitation, Event, FormReply, PendingApproval, PendingQuestion, QuestionAction};
use otto_store::Projection;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
pub(crate) struct Bridge {
    pub cx: Arc<Context>,
    pub cancel: CancellationToken,
    pub turn: i64,
    pub step: i64,
}
impl Bridge {
    /// Fire Notification hooks without holding up the call.
    fn notify(&self, message: String, kind: &'static str) {
        let cx = self.cx.clone();
        tokio::spawn(async move {
            let _ = crate::hooks::run(&cx, "Notification", kind, serde_json::json!({"message":message,"notification_type":kind})).await;
        });
    }
    pub async fn gate(&self, call_id: &str, tool: &str, summary: &str, input: serde_json::Value) -> Result<ApprovalDecision> {
        let id = crate::id();
        let (tx, rx) = oneshot::channel();
        let pending = PendingApproval {
            approval_id: id.clone(),
            call_id: call_id.into(),
            tool: tool.into(),
            summary: summary.into(),
            input: input.clone(),
            effects: vec![],
        };
        let key = id.clone();
        self.cx
            .state(move |s| {
                s.approvals.insert(key, (pending, tx));
            })
            .await?;
        self.notify(format!("otto needs your permission to use {tool}"), "permission_prompt");
        let seq = self
            .cx
            .emit(
                Event::ApprovalRequested {
                    approval_id: id.clone(),
                    call_id: call_id.into(),
                    tool: tool.into(),
                    summary: summary.into(),
                    input,
                    effects: vec![],
                },
                Projection::None,
                self.turn,
                self.step,
            )
            .await?;
        crate::subagents::waiting(&self.cx, seq, "approval", &id, summary).await;
        let result = tokio::select! {_=self.cancel.cancelled()=>ApprovalDecision::Deny,result=rx=>result.unwrap_or(ApprovalDecision::Deny)};
        self.cx
            .state(move |s| {
                s.approvals.remove(&id);
            })
            .await?;
        Ok(result)
    }
}
/// A question still pending when its asker goes away (a cancelled call, an MCP
/// server that stopped waiting) is withdrawn: cancelled through the session, so
/// clients drop its card.
struct Withdraw {
    cx: Arc<Context>,
    id: Option<String>,
}
impl Drop for Withdraw {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let (runtime, session) = (self.cx.runtime.clone(), self.cx.row.id.clone());
        rt.spawn(async move {
            if let Ok(handle) = runtime.session(&session).await {
                let _ = handle.reply_question(&id, QuestionReply { action: Some(QuestionAction::Cancel), ..Default::default() }).await;
            }
        });
    }
}
impl Bridge {
    /// Post `pending` (its id is set here) and wait for the answer: `None` when the call
    /// is cancelled or the turn ends. `notice` goes to Notification hooks, `waiting` to a
    /// sub-agent's parent.
    async fn question(&self, mut pending: PendingQuestion, notice: String, waiting: String) -> Result<Option<Answered>> {
        let id = crate::id();
        pending.question_id = id.clone();
        let (tx, rx) = oneshot::channel();
        let event = Event::QuestionAsked {
            question_id: id.clone(),
            call_id: pending.call_id.clone(),
            question: pending.question.clone(),
            options: pending.options.clone(),
            server: pending.server.clone(),
            form: pending.form.clone(),
        };
        let key = id.clone();
        self.cx
            .state(move |s| {
                s.questions.insert(key, (pending, tx));
            })
            .await?;
        let mut withdraw = Withdraw { cx: self.cx.clone(), id: Some(id.clone()) };
        self.notify(notice, "question");
        let seq = self.cx.emit(event, Projection::None, self.turn, self.step).await?;
        crate::subagents::waiting(&self.cx, seq, "question", &id, &waiting).await;
        let answered = tokio::select! {_=self.cancel.cancelled()=>None,result=rx=>result.ok()};
        if answered.is_some() {
            // The session took it off when it was answered.
            withdraw.id = None;
        }
        Ok(answered)
    }

    /// An MCP server's elicitation as a question with fields; `decline` when it cannot
    /// be asked, `cancel` when the call stops or the turn ends first.
    pub async fn elicit(&self, call_id: &str, elicitation: Elicitation) -> FormReply {
        let Elicitation { server, message, form } = elicitation;
        let notice = format!("MCP server {server} asks: {message}");
        let mut waiting = format!("MCP server {server} asks: {}", crate::subagents::asked(&message, &form.options()));
        if form.fields.len() > 1 {
            let names: Vec<&str> = form.fields.iter().map(|f| f.name.as_str()).collect();
            waiting.push_str(&format!(" Answer with a JSON object of {}.", names.join(", ")));
        }
        let pending = PendingQuestion {
            call_id: call_id.into(),
            question: message,
            options: form.options(),
            server: Some(server),
            form: Some(form),
            ..Default::default()
        };
        match self.question(pending, notice, waiting).await {
            Ok(Some(answered)) => FormReply { action: answered.action, content: answered.content },
            Ok(None) => FormReply::cancel(),
            Err(error) => {
                tracing::debug!(%error, "elicitation could not be asked");
                FormReply::decline()
            }
        }
    }
}
#[async_trait::async_trait]
impl otto_tools::Approver for Bridge {
    async fn approve(&self, call_id: &str, summary: &str, detail: serde_json::Value) -> bool {
        matches!(self.gate(call_id, "tool", summary, detail).await, Ok(ApprovalDecision::Allow | ApprovalDecision::AlwaysAllow))
    }
    async fn ask(&self, call_id: &str, question: &str, options: &[String]) -> Option<String> {
        let pending = PendingQuestion { call_id: call_id.into(), question: question.into(), options: options.to_vec(), ..Default::default() };
        let notice = format!("otto asks: {question}");
        let waiting = crate::subagents::asked(question, options);
        let answered = self.question(pending, notice, waiting).await.ok()??;
        (answered.action == QuestionAction::Accept).then_some(answered.text)
    }
}
