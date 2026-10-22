use crate::{Error, Result, Runtime, SequencedEvent};
use otto_proto::*;
use otto_store::Projection;
use std::sync::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct SessionHandle {
    pub id: String,
    pub(crate) tx: mpsc::Sender<Command>,
    pub(crate) events: broadcast::Sender<SequencedEvent>,
    deltas: broadcast::Sender<Event>,
    pub(crate) busy: Arc<Mutex<bool>>,
}
pub(crate) enum Command {
    Prompt(QueuedInput),
    Cancel,
    /// A durable outbox entry is waiting: deliver it at the next step boundary,
    /// or start a turn when idle.
    Wake,
    QueueRemove(String, oneshot::Sender<Result<()>>),
    QueueUpdate(String, Delivery, oneshot::Sender<Result<()>>),
    Update(otto_store::SessionUpdate, oneshot::Sender<Result<()>>),
    Shutdown(oneshot::Sender<()>),
    View(oneshot::Sender<Result<SessionView>>),
    Approval(String, ApprovalDecision, Option<String>, oneshot::Sender<Result<()>>),
    Tools(oneshot::Sender<Result<Vec<ToolInfo>>>),
    SetMode(ApprovalMode, oneshot::Sender<Result<()>>),
    Question(String, String, oneshot::Sender<Result<()>>),
    Compact(oneshot::Sender<Result<()>>),
}
impl SessionHandle {
    /// Uncoalesced display deltas. Do not also render durable delta events;
    /// item.done remains authoritative for the complete item.
    pub fn deltas(&self) -> broadcast::Receiver<Event> {
        self.deltas.subscribe()
    }
    pub async fn prompt(&self, input: Vec<InputPart>, delivery: Delivery) -> Result<String> {
        let id = crate::id();
        let permit = self.tx.reserve().await.map_err(|_| Error::Closed)?;
        let mut busy = self.busy.lock().map_err(|_| Error::Closed)?;
        *busy = true;
        permit.send(Command::Prompt(QueuedInput { id: id.clone(), input, delivery }));
        Ok(id)
    }
    pub async fn cancel(&self) -> Result<()> {
        self.tx.send(Command::Cancel).await.map_err(|_| Error::Closed)
    }
    /// Withdraw a queued input or a steer the model has not been handed yet.
    pub async fn queue_remove(&self, input_id: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Command::QueueRemove(input_id.into(), tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }
    /// Move a pending input between steer (next step), queue (after the turn)
    /// and interrupt (cancel the turn, run it now).
    pub async fn queue_update(&self, input_id: &str, delivery: Delivery) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Command::QueueUpdate(input_id.into(), delivery, tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }
    pub async fn view(&self) -> Result<SessionView> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Command::View(tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }
    pub async fn answer_approval(&self, id: &str, decision: ApprovalDecision) -> Result<()> {
        self.answer_approval_with_note(id, decision, None).await
    }
    pub async fn answer_approval_with_note(&self, id: &str, decision: ApprovalDecision, note: Option<String>) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Command::Approval(id.into(), decision, note, tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }
    pub async fn answer_question(&self, id: &str, answer: String) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Command::Question(id.into(), answer, tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }
    pub async fn compact(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Command::Compact(tx)).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }
}
pub(crate) struct Pending {
    pub approvals: HashMap<String, (PendingApproval, oneshot::Sender<ApprovalDecision>)>,
    pub questions: HashMap<String, (PendingQuestion, oneshot::Sender<String>)>,
    pub steer: Vec<QueuedInput>,
    /// Steering input handed to the live model stream and not yet committed by it.
    pub native: Vec<QueuedInput>,
    pub policy: otto_tools::policy::Policy,
    pub started: bool,
    /// Inputs waiting for the current turn to end.
    pub queue: VecDeque<QueuedInput>,
    /// Undelivered outbox entries should start a turn once idle.
    pub wake: bool,
    /// A hook asked to stop the turn (`continue: false`) after this step.
    pub halt: Option<String>,
    /// Hook context produced while tools ran, committed after their results.
    pub after_tools: Vec<String>,
}
impl Pending {
    /// What clients see as pending: steers first (next step), then the queue.
    pub fn snapshot(&self) -> Vec<QueuedInput> {
        self.native.iter().chain(&self.steer).chain(&self.queue).cloned().collect()
    }
    /// Take a pending input out, unless it was already handed to the model.
    fn take(&mut self, id: &str) -> Result<QueuedInput> {
        if self.native.iter().any(|q| q.id == id) {
            return Err(Error::Invalid("input already sent to the model".into()));
        }
        if let Some(index) = self.steer.iter().position(|q| q.id == id) {
            return Ok(self.steer.remove(index));
        }
        let index = self.queue.iter().position(|q| q.id == id).ok_or_else(|| Error::Invalid(format!("input {id} is not pending")))?;
        Ok(self.queue.remove(index).expect("index in range"))
    }
}
pub(crate) struct Context {
    pub runtime: Runtime,
    pub row: otto_store::Session,
    pub model: Arc<dyn otto_llm::Model>,
    pub system: Vec<String>,
    pub events: broadcast::Sender<SequencedEvent>,
    pub deltas: broadcast::Sender<Event>,
    /// Wakes a streaming step when steering input arrives.
    pub steer_wake: tokio::sync::Notify,
    state: mpsc::Sender<StateOp>,
    journal: mpsc::Sender<Journal>,
    pub root: std::path::PathBuf,
}
type StateOp = Box<dyn FnOnce(&mut Pending) + Send>;
struct Journal {
    event: Event,
    projection: Projection,
    turn: i64,
    step: i64,
    reply: oneshot::Sender<Result<i64>>,
}
impl Context {
    pub async fn state<T: Send + 'static>(&self, f: impl FnOnce(&mut Pending) -> T + Send + 'static) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.state
            .send(Box::new(move |state| {
                let _ = tx.send(f(state));
            }))
            .await
            .map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)
    }
    pub async fn emit(&self, event: Event, projection: Projection, turn: i64, step: i64) -> Result<i64> {
        let (reply, rx) = oneshot::channel();
        self.journal.send(Journal { event, projection, turn, step, reply }).await.map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }
    async fn commit(&self, event: Event, projection: Projection, turn: i64, step: i64) -> Result<i64> {
        self.commit_status(event, projection, turn, step, None).await
    }
    async fn commit_status(&self, event: Event, projection: Projection, turn: i64, step: i64, override_status: Option<&str>) -> Result<i64> {
        let record = crate::runtime::record(&event, turn, step)?;
        let id = self.row.id.clone();
        let status = match &event {
            Event::TurnStarted { .. } | Event::ApprovalResolved { .. } | Event::QuestionResolved { .. } => Some("busy"),
            Event::ApprovalRequested { .. } | Event::QuestionAsked { .. } => Some("waiting_approval"),
            Event::TurnFinished { .. } => Some("idle"),
            _ => None,
        }
        .map(String::from);
        let status = override_status.map(String::from).or(status);
        let delivered = match &event {
            Event::ItemDone { item } => item.id.strip_prefix("outbox-").map(String::from),
            _ => None,
        };
        let changed = status.is_some();
        let seq = self
            .runtime
            .db(move |s| {
                let update = otto_store::SessionUpdate { status, ..Default::default() };
                Ok(match delivered {
                    Some(outbox) => s.append_delivering(&id, record, projection, update, &outbox)?,
                    None => s.append_with_session(&id, record, projection, update)?,
                })
            })
            .await?;
        if changed {
            self.runtime.changed();
        }
        if !matches!(event, Event::TurnFinished { .. }) {
            let _ = self.events.send((seq, event));
        }
        Ok(seq)
    }
    /// Journal the current pending inputs (`queue.changed`).
    pub async fn queue_changed(&self, turn: i64, step: i64) -> Result<()> {
        let queued = self.state(|s| s.snapshot()).await?;
        self.emit(Event::QueueChanged { queued }, Projection::None, turn, step).await?;
        Ok(())
    }
    pub async fn item(&self, item: otto_llm::Item, turn: i64, step: i64) -> Result<()> {
        self.emit(Event::ItemDone { item: item.clone() }, Projection::PutItem(item), turn, step).await?;
        Ok(())
    }
    pub async fn items(&self) -> Result<Vec<otto_llm::Item>> {
        let id = self.row.id.clone();
        self.runtime.db(move |s| Ok(s.items(&id)?)).await
    }
}
pub(crate) async fn spawn(runtime: Runtime, row: otto_store::Session) -> Result<SessionHandle> {
    let model = runtime.0.config.models.resolve(&row.provider, &row.model)?;
    let project = row.project_id.clone();
    let root = runtime.db(move |s| Ok(s.project(&project)?.ok_or(Error::Closed)?.root)).await?;
    let root = std::path::PathBuf::from(root);
    let system = crate::prompt::assemble(&root, std::path::Path::new(&row.cwd), &runtime.0.config.user_config_dir, &row.agent);
    let mode = if row.agent == "plan" {
        otto_tools::policy::ApprovalMode::Plan
    } else {
        serde_json::from_value(row.meta["approval_mode"].clone()).unwrap_or(otto_tools::policy::ApprovalMode::Auto)
    };
    let policy = otto_tools::policy::Policy::load(&root.join(".otto/rules.json"), &runtime.0.config.user_config_dir.join("rules.json"), mode)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    let (tx, rx) = mpsc::channel(128);
    let (journal, journal_rx) = mpsc::channel(128);
    let (state, state_rx) = mpsc::channel(128);
    let pending = Pending {
        approvals: HashMap::new(),
        questions: HashMap::new(),
        steer: vec![],
        native: vec![],
        policy,
        started: false,
        queue: VecDeque::new(),
        wake: false,
        halt: None,
        after_tools: vec![],
    };
    let (events, _) = broadcast::channel(4096);
    let (deltas, _) = broadcast::channel(4096);
    let busy = Arc::new(Mutex::new(false));
    let handle = SessionHandle { id: row.id.clone(), tx, events: events.clone(), deltas: deltas.clone(), busy: busy.clone() };
    let cx = Arc::new(Context { runtime, row, model, system, events, deltas, steer_wake: tokio::sync::Notify::new(), state, journal, root });
    tokio::spawn(actor(cx, rx, journal_rx, state_rx, pending, busy));
    Ok(handle)
}
async fn actor(
    cx: Arc<Context>,
    mut rx: mpsc::Receiver<Command>,
    mut journal: mpsc::Receiver<Journal>,
    mut state_rx: mpsc::Receiver<StateOp>,
    mut state: Pending,
    busy: Arc<Mutex<bool>>,
) {
    let mut active: Option<tokio::task::JoinHandle<Result<()>>> = None;
    let mut cancel = CancellationToken::new();
    let mut turn = cx.row.last_seq;
    let mut wake_pending = false;
    let mut finished_event = None;
    let mut archived = cx.row.archived;
    loop {
        {
            let mut flag = busy.lock().expect("busy mutex");
            *flag = active.is_some() || !state.queue.is_empty() || (state.wake && !archived) || !rx.is_empty();
        }
        let next = if active.is_some() || archived {
            None
        } else if let Some(input) = state.queue.pop_front() {
            let _ = cx.commit(Event::QueueChanged { queued: state.snapshot() }, Projection::None, turn + 1, 0).await;
            Some(input)
        } else if std::mem::take(&mut state.wake) {
            // The outbox entries are the input; step::run delivers them.
            Some(QueuedInput { id: crate::id(), input: vec![], delivery: Delivery::Queue })
        } else {
            None
        };
        if let Some(input) = next {
            turn += 1;
            cancel = CancellationToken::new();
            let c = cx.clone();
            let token = cancel.clone();
            active = Some(tokio::spawn(async move { crate::step::run(c, input, turn, token).await }));
        }
        tokio::select! {
            Some(op)=state_rx.recv()=>op(&mut state),
            Some(entry)=journal.recv()=>{let finish=matches!(entry.event,Event::TurnFinished{..}).then(||entry.event.clone());let result=cx.commit(entry.event,entry.projection,entry.turn,entry.step).await;if let (Some(event),Ok(seq))=(finish,&result){finished_event=Some((*seq,event));}let _=entry.reply.send(result);}
            result=async{match &mut active {Some(task)=>Some(task.await),None=>std::future::pending().await}}=>{
                if let Some(Ok(Err(e)))=result {tracing::error!(session=%cx.row.id,error=%e,"turn failed");}
                active=None;state.approvals.clear();state.questions.clear();state.halt=None;state.after_tools.clear();
                // Steers the turn never reached run next, after any interrupt that ended it.
                let at=state.queue.iter().take_while(|q|q.delivery==Delivery::Interrupt).count();let leftover:Vec<_>=state.native.drain(..).chain(state.steer.drain(..)).collect();for (i,input) in leftover.into_iter().enumerate(){state.queue.insert(at+i,input);}
                if let Some(event)=finished_event.take(){let _=cx.events.send(event);}
                if wake_pending { wake_pending=false;let id=cx.row.id.clone();if cx.runtime.db(move|s|Ok(!s.outbox_pending(&id)?.is_empty())).await.unwrap_or(false){state.wake=true;} }
            }
            command=rx.recv()=>{let Some(command)=command else {cancel.cancel();break};match command {
                Command::Tools(reply)=>{let _=reply.send(crate::tools::infos(cx.clone()).await);}
                Command::SetMode(mode,reply)=>{
                    if cx.row.agent=="plan"&&mode!=ApprovalMode::Plan{let _=reply.send(Err(Error::Invalid("plan agents cannot leave plan mode".into())));continue;}
                    let id=cx.row.id.clone();
                    let result=cx.runtime.db(move|s|{
                        let mut row=s.session(&id)?.ok_or(Error::Closed)?;
                        if !row.meta.is_object(){row.meta=serde_json::json!({});}
                        row.meta["approval_mode"]=serde_json::to_value(mode)?;
                        let meta=row.meta.clone();let mut info=crate::runtime::info(s,row)?;info.last_seq+=1;
                        let event=Event::SessionChanged{info};
                        let seq=s.append_with_session(&id,crate::runtime::record(&event,turn,0)?,Projection::None,otto_store::SessionUpdate{meta:Some(meta),..Default::default()})?;
                        Ok((seq,event))
                    }).await;
                    let result=result.map(|event|{state.policy.mode=match mode{ApprovalMode::Auto=>otto_tools::policy::ApprovalMode::Auto,ApprovalMode::AskOnEffects=>otto_tools::policy::ApprovalMode::AskOnEffects,ApprovalMode::AskAll=>otto_tools::policy::ApprovalMode::AskAll,ApprovalMode::Plan=>otto_tools::policy::ApprovalMode::Plan};let _=cx.events.send(event);cx.runtime.changed();});
                    let _=reply.send(result);
                }
                Command::Update(update, reply)=>{
                    if update.archived==Some(true)&&(active.is_some()||!state.queue.is_empty()){let _=reply.send(Err(Error::Invalid("cannot archive a busy session; cancel and wait first".into())));continue;}
                    let archive=update.archived;
                    let state_dir=cx.runtime.0.config.state_dir.clone();
                    let id=cx.row.id.clone();
                    let result=cx.runtime.db(move|s|{
                        let row=s.session(&id)?.ok_or(Error::Closed)?;let mut info=crate::runtime::info(s,row)?;
                        if let Some(title)=&update.title{info.title=title.clone();}
                        if let Some(archived)=update.archived{info.status=if archived{SessionStatus::Archived}else{SessionStatus::Idle};}
                        info.last_seq+=1;let event=Event::SessionChanged{info};
                        if archive==Some(true){crate::spills::remove(&state_dir,&id)?;}
                        let seq=s.append_with_session(&id,crate::runtime::record(&event,0,0)?,Projection::None,update)?;
                        Ok((seq,event))
                    }).await;
                    let result=result.map(|event|{if let Some(value)=archive{archived=value;}let _=cx.events.send(event);});
                    if archive==Some(false)&&result.is_ok(){let id=cx.row.id.clone();state.wake|=cx.runtime.db(move|s|Ok(!s.outbox_pending(&id)?.is_empty())).await.unwrap_or(false);}
                    if archive==Some(true)&&result.is_ok(){let c=cx.clone();tokio::spawn(async move{let _=crate::hooks::run(&c,"SessionEnd","archive",serde_json::json!({"reason":"archive"})).await;});}
                    let _=reply.send(result);
                }
                Command::Prompt(input)=>{if archived{let _=cx.commit(Event::Error{class:"archived".into(),message:"Unarchive the session before prompting".into(),retryable:false},Projection::None,turn,0).await;continue;}enqueue(&cx,&mut state,&cancel,active.is_some(),input);let _=cx.commit(Event::QueueChanged{queued:state.snapshot()},Projection::None,turn,0).await;}
                Command::QueueRemove(id,reply)=>{let result=state.take(&id).map(|_|());if result.is_ok(){let _=cx.commit(Event::QueueChanged{queued:state.snapshot()},Projection::None,turn,0).await;}let _=reply.send(result);}
                Command::QueueUpdate(id,delivery,reply)=>{let result=state.take(&id).map(|mut input|{input.delivery=delivery;enqueue(&cx,&mut state,&cancel,active.is_some(),input);});if result.is_ok(){let _=cx.commit(Event::QueueChanged{queued:state.snapshot()},Projection::None,turn,0).await;}let _=reply.send(result);}
                Command::Wake=>{if active.is_some(){wake_pending=true;}else{state.wake=true;}}
                Command::Shutdown(reply)=>{cancel.cancel();if let Some(mut task)=active.take(){loop{tokio::select!{Some(op)=state_rx.recv()=>op(&mut state),_= &mut task=>break,Some(entry)=journal.recv()=>{let result=cx.commit(entry.event,entry.projection,entry.turn,entry.step).await;let _=entry.reply.send(result);}}}}let _=reply.send(());break;}
                Command::Cancel=>{cancel.cancel();wake_pending=false;state.wake=false;let queued=!state.snapshot().is_empty();state.queue.clear();state.steer.clear();state.native.clear();if queued{let _=cx.commit(Event::QueueChanged{queued:vec![]},Projection::None,turn,0).await;}crate::subagents::cancel_children(&cx).await;}
                Command::View(reply)=>{let id=cx.row.id.clone();let pending=&state;let approvals=pending.approvals.values().map(|p|p.0.clone()).collect();let questions=pending.questions.values().map(|p|p.0.clone()).collect();let queued=pending.snapshot();let busy=active.is_some();let waiting=!pending.approvals.is_empty()||!pending.questions.is_empty();let result=cx.runtime.db(move|s|{let row=s.session(&id)?.ok_or(Error::Closed)?;let mut info=crate::runtime::info(s,row)?;info.status=if waiting{SessionStatus::WaitingApproval}else if busy{SessionStatus::Busy}else{info.status};Ok(SessionView{seq:info.last_seq,info,items:s.items(&id)?,pending_approvals:approvals,pending_questions:questions,queue:queued,running_tool_calls:s.tool_calls(&id)?.into_iter().filter(|t|matches!(t.status,otto_store::ToolStatus::Intent|otto_store::ToolStatus::Running)).map(|t|RunningToolCall{call_id:t.call_id,name:t.name,input:t.input}).collect()})}).await;let _=reply.send(result);}
                Command::Approval(id,decision,note,reply)=>{if !state.approvals.contains_key(&id){let _=reply.send(Err(Error::Invalid("unknown approval".into())));continue;}else if let Some(note)=note{let item=otto_llm::Item::new(crate::id(),otto_llm::Role::System,vec![otto_llm::Part::System{text:format!("Approval {id} note: {note}")}]);if let Err(error)=cx.commit(Event::ItemDone{item:item.clone()},Projection::PutItem(item),turn,0).await{let _=reply.send(Err(error));continue;}}let pending=state.approvals.remove(&id);let result=if let Some((_,tx))=pending{match cx.commit_status(Event::ApprovalResolved{approval_id:id,decision},Projection::None,turn,0,Some(if state.approvals.is_empty()&&state.questions.is_empty(){"busy"}else{"waiting_approval"})).await{Ok(_)=>{let _=tx.send(decision);Ok(())},Err(e)=>Err(e)}}else{Err(Error::Invalid("unknown approval".into()))};let _=reply.send(result);}
                Command::Question(id,answer,reply)=>{let pending=state.questions.remove(&id);let result=if let Some((_,tx))=pending{match cx.commit_status(Event::QuestionResolved{question_id:id,answer:answer.clone()},Projection::None,turn,0,Some(if state.approvals.is_empty()&&state.questions.is_empty(){"busy"}else{"waiting_approval"})).await{Ok(_)=>{let _=tx.send(answer);Ok(())},Err(e)=>Err(e)}}else{Err(Error::Invalid("unknown question".into()))};let _=reply.send(result);}
                Command::Compact(reply)=>{*busy.lock().expect("busy mutex")=true;let result=if active.is_some(){Err(Error::Invalid("session busy".into()))}else{{let c=cx.clone();let token=CancellationToken::new();let mut task=tokio::spawn(async move{crate::compaction::compact(&c,&token,turn,"manual").await});loop{tokio::select!{Some(op)=state_rx.recv()=>op(&mut state),result=&mut task=>break result.unwrap_or(Err(Error::Closed)),Some(entry)=journal.recv()=>{let result=cx.commit(entry.event,entry.projection,entry.turn,entry.step).await;let _=entry.reply.send(result);}}}}};let _=reply.send(result);}
            }}
        }
    }
}
/// Place an input by its delivery: steer joins the running turn at its next
/// step boundary, interrupt cancels the turn and runs first, queue waits.
fn enqueue(cx: &Context, state: &mut Pending, cancel: &CancellationToken, active: bool, input: QueuedInput) {
    match input.delivery {
        Delivery::Steer if active => {
            state.steer.push(input);
            cx.steer_wake.notify_one();
        }
        Delivery::Interrupt => {
            cancel.cancel();
            state.queue.push_front(input);
        }
        _ => state.queue.push_back(input),
    }
}
