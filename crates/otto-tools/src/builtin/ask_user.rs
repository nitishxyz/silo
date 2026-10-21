use super::*;
use crate::ToolOutput;
tool!(
    AskUser,
    spec(
        "ask_user",
        "Ask the user a focused question when a decision cannot be inferred. Optional options are suggested answers; the user may answer freely. Returns an error when no interactive approver is available or the question is unanswered.",
        json!({"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"}}}),
        &["question"],
        Effects::NONE
    ),
    run
);
async fn run(input: Value, cx: ToolCx) -> Result<ToolOutput, ToolError> {
    let question = string(&input, "question")?;
    let options: Vec<String> = input
        .get("options")
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()
        .map_err(|e| ToolError::invalid(e.to_string()))?
        .unwrap_or_default();
    let approver = cx.approver.as_ref().ok_or_else(|| ToolError::failed("No interactive approver is available"))?;
    tokio::select! { answer=approver.ask(&cx.call_id,question,&options)=>answer.map(ToolOutput::text).ok_or_else(||ToolError::failed("Question was unanswered")),()=cx.cancel.cancelled()=>Err(ToolError::new(crate::ToolErrorKind::Cancelled,"Question cancelled")) }
}
