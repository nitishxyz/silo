use crate::{Result, session::Context};
use otto_proto::{ApprovalDecision, Event, PendingApproval, PendingQuestion};
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
        self.cx
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
        let result = tokio::select! {_=self.cancel.cancelled()=>ApprovalDecision::Deny,result=rx=>result.unwrap_or(ApprovalDecision::Deny)};
        self.cx
            .state(move |s| {
                s.approvals.remove(&id);
            })
            .await?;
        Ok(result)
    }
}
#[async_trait::async_trait]
impl otto_tools::Approver for Bridge {
    async fn approve(&self, call_id: &str, summary: &str, detail: serde_json::Value) -> bool {
        matches!(self.gate(call_id, "tool", summary, detail).await, Ok(ApprovalDecision::Allow | ApprovalDecision::AlwaysAllow))
    }
    async fn ask(&self, call_id: &str, question: &str, options: &[String]) -> Option<String> {
        let id = crate::id();
        let (tx, rx) = oneshot::channel();
        let pending = PendingQuestion { question_id: id.clone(), call_id: call_id.into(), question: question.into(), options: options.to_vec() };
        let key = id.clone();
        self.cx
            .state(move |s| {
                s.questions.insert(key, (pending, tx));
            })
            .await
            .ok()?;
        self.notify(format!("otto asks: {question}"), "question");
        self.cx
            .emit(
                Event::QuestionAsked { question_id: id.clone(), call_id: call_id.into(), question: question.into(), options: options.to_vec() },
                Projection::None,
                self.turn,
                self.step,
            )
            .await
            .ok()?;
        let result = tokio::select! {_=self.cancel.cancelled()=>None,result=rx=>result.ok()};
        self.cx
            .state(move |s| {
                s.questions.remove(&id);
            })
            .await
            .ok()?;
        result
    }
}
