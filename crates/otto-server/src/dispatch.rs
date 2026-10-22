use crate::{
    State,
    connection::{Sink, Subs},
};
use otto_core::CreateSession;
use otto_proto::*;
use serde_json::{Value, json};

pub(crate) fn core_error(error: otto_core::Error) -> RpcError {
    use otto_core::Error as E;
    match error {
        E::Store(otto_store::Error::NotFound(name)) => RpcError::not_found(name),
        E::Store(otto_store::Error::InvalidTransition(message)) => RpcError::bad_request(message),
        E::Invalid(message) if message.starts_with("unknown session") => RpcError::not_found(message),
        E::Invalid(message) if message.contains("busy") => RpcError::busy(message),
        E::Invalid(message) => RpcError::bad_request(message),
        E::Model(error) if error.class == otto_llm::ErrorClass::Auth => RpcError::auth_required(error.to_string()),
        E::Io(error) if error.kind() == std::io::ErrorKind::NotFound => RpcError::not_found(error),
        E::Closed => RpcError::busy("session is stopping"),
        error => RpcError::internal(error),
    }
}

pub(crate) async fn dispatch(state: &State, method: Method, sink: &Sink, subs: &Subs) -> Result<Value, RpcError> {
    let runtime = &state.config.runtime;
    match method {
        Method::ServerInfo {} => Ok(json!(state.info)),
        Method::ServerShutdown {} | Method::Ping {} => Ok(json!({})),
        Method::ApiList {} => Ok(json!(
            METHODS
                .iter()
                .map(|(method, description)| ApiMethod { method: (*method).into(), description: (*description).into() })
                .collect::<Vec<_>>()
        )),
        Method::ApiDescribe { method } => Ok(json!(api::describe(&method).ok_or_else(|| RpcError::method_not_found(&method))?)),
        Method::ProjectOpen { root } => Ok(json!(runtime.open_project(root).await.map_err(core_error)?)),
        Method::ProjectList {} => Ok(json!(runtime.list_projects().await.map_err(core_error)?)),
        Method::SessionCreate { project_root, project_id, agent, approval_mode, model, provider, title, parent } => {
            let project = match (project_root, project_id) {
                (Some(root), None) => runtime.open_project(root).await.map_err(core_error)?,
                (None, Some(id)) => {
                    runtime.list_projects().await.map_err(core_error)?.into_iter().find(|p| p.id == id).ok_or_else(|| RpcError::not_found(id))?
                }
                _ => return Err(RpcError::bad_request("exactly one of project_root and project_id is required")),
            };
            if let Some(parent) = &parent {
                runtime.view(parent).await.map_err(core_error)?;
            }
            let handle = runtime
                .create_session(CreateSession {
                    project_id: project.id,
                    cwd: project.root.into(),
                    parent_id: parent,
                    agent: agent.unwrap_or_default(),
                    approval_mode,
                    provider,
                    model,
                    title: title.unwrap_or_default(),
                    ..Default::default()
                })
                .await
                .map_err(core_error)?;
            Ok(json!(handle.view().await.map_err(core_error)?.info))
        }
        Method::SessionList { project_id, include_archived } => {
            let projects = match project_id {
                Some(id) => vec![id],
                None => runtime.list_projects().await.map_err(core_error)?.into_iter().map(|p| p.id).collect(),
            };
            let mut sessions = Vec::new();
            for project in projects {
                sessions.extend(runtime.list_sessions(&project, include_archived).await.map_err(core_error)?);
            }
            Ok(json!(sessions))
        }
        Method::SessionView { session_id } => Ok(json!(runtime.view(&session_id).await.map_err(core_error)?)),
        Method::SessionSubscribe { session_id, after_seq } => {
            Ok(json!(crate::connection::subscribe(state, session_id, after_seq.unwrap_or(0), sink.clone(), subs).await?))
        }
        Method::SessionUnsubscribe { session_id } => {
            if let Some(task) = subs.lock().await.sessions.remove(&session_id) {
                task.abort();
                let _ = task.await;
            }
            Ok(json!({}))
        }
        Method::SessionFork { session_id, at_seq } => {
            let seq = match at_seq {
                Some(seq) => seq,
                None => runtime.view(&session_id).await.map_err(core_error)?.seq,
            };
            let handle = runtime.fork(&session_id, seq).await.map_err(core_error)?;
            Ok(json!(handle.view().await.map_err(core_error)?.info))
        }
        Method::SessionSetMode { session_id, approval_mode } => {
            runtime.set_approval_mode(&session_id, approval_mode).await.map_err(core_error)?;
            Ok(json!({}))
        }
        Method::SessionRename { session_id, title } => {
            runtime.rename(&session_id, title).await.map_err(core_error)?;
            Ok(json!({}))
        }
        Method::SessionArchive { session_id } => {
            runtime.archive(&session_id, true).await.map_err(core_error)?;
            Ok(json!({}))
        }
        Method::SessionPrompt { session_id, input, delivery } => {
            if input.is_empty() {
                return Err(RpcError::bad_request("input must not be empty"));
            }
            Ok(json!(PromptAccepted { input_id: runtime.prompt(&session_id, input, delivery).await.map_err(core_error)? }))
        }
        Method::SessionCancel { session_id } => {
            runtime.cancel(&session_id).await.map_err(core_error)?;
            Ok(json!({}))
        }
        Method::SessionCompact { session_id } => {
            runtime.compact(&session_id).await.map_err(core_error)?;
            Ok(json!({}))
        }
        Method::ApprovalAnswer { session_id, approval_id, decision, note } => {
            runtime.answer_approval_with_note(&session_id, &approval_id, decision, note).await.map_err(core_error)?;
            Ok(json!({}))
        }
        Method::QuestionAnswer { session_id, question_id, answer } => {
            runtime.answer_question(&session_id, &question_id, answer).await.map_err(core_error)?;
            Ok(json!({}))
        }
        Method::ModelList { provider } => Ok(json!(state.config.auth.list_models(provider).await?)),
        Method::AuthStatus {} => Ok(json!(state.config.auth.status().await?)),
        Method::AuthLogin { provider, mode, api_key, device } => {
            if provider.is_empty()
                || (mode == AuthMode::ApiKey && api_key.as_deref().is_none_or(str::is_empty))
                || (mode == AuthMode::Subscription && api_key.is_some())
                || (device && mode != AuthMode::Subscription)
            {
                return Err(RpcError::bad_request("login requires a provider and credentials matching its mode"));
            }
            Ok(json!(state.config.auth.login(provider, mode, api_key, device).await?))
        }
        Method::AuthLoginComplete { login_id, code } => Ok(json!(state.config.auth.login_complete(login_id, code).await?)),
        Method::AuthLoginCancel { login_id } => {
            state.config.auth.login_cancel(login_id).await?;
            Ok(json!({}))
        }
        Method::AuthLogout { provider } => {
            state.config.auth.logout(provider).await?;
            Ok(json!({}))
        }
        Method::ToolsList { session_id } => {
            let id = session_id.ok_or_else(|| RpcError::bad_request("tools.list requires session_id to resolve effective tools"))?;
            Ok(json!(runtime.tools(&id).await.map_err(core_error)?))
        }
        Method::AgentsList {} => Ok(json!(runtime.agents(None))),
        Method::ConfigGet { key } => {
            let settings = state.settings.lock().await;
            Ok(json!(ConfigValue { value: key.map_or_else(|| settings.clone(), |key| settings.get(key).cloned().unwrap_or(Value::Null)) }))
        }
        Method::ConfigSet { key, value } => {
            if key.is_empty() {
                return Err(RpcError::bad_request("config key must not be empty"));
            }
            let mut settings = state.settings.lock().await;
            let mut next = settings.clone();
            next.as_object_mut().ok_or_else(|| RpcError::internal("config must be an object"))?.insert(key, value);
            let next_defaults = defaults(&next)?;
            runtime.set_config(next_defaults).map_err(core_error)?;
            if let Err(error) = save_json(&state.config.home.join("config.json"), &next).await {
                runtime.set_config(defaults(&settings)?).map_err(core_error)?;
                return Err(error);
            }
            *settings = next;
            Ok(json!({}))
        }
    }
}
pub(crate) fn defaults(value: &Value) -> Result<otto_core::SessionDefaults, RpcError> {
    if !value.is_object() {
        return Err(RpcError::bad_request("config must be an object"));
    }
    let string = |key: &str| -> Result<Option<String>, RpcError> {
        match value.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
            _ => Err(RpcError::bad_request(format!("{key} must be a nonempty string or null"))),
        }
    };
    Ok(otto_core::SessionDefaults {
        agent: string("agent")?,
        provider: string("provider")?,
        model: string("model")?,
        approval_mode: match value.get("approval_mode") {
            None | Some(Value::Null) => Some(Default::default()),
            Some(mode) => Some(serde_json::from_value(mode.clone()).map_err(|e| RpcError::bad_request(format!("approval_mode: {e}")))?),
        },
    })
}
async fn save_json(path: &std::path::Path, value: &Value) -> Result<(), RpcError> {
    let temporary = path.with_extension("json.tmp");
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(value).map_err(RpcError::internal)?).await.map_err(RpcError::internal)?;
    tokio::fs::rename(temporary, path).await.map_err(RpcError::internal)
}
