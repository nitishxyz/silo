use crate::{ApplyResult, ClientError, Connection, Result, SessionMirror};
use otto_proto::ServerMessage;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};

#[derive(Clone, Debug)]
pub struct FollowerState {
    pub mirror: SessionMirror,
    pub connected: bool,
    pub error: Option<ClientError>,
}
/// A watch value always contains the latest complete projection. Local connections
/// reconnect automatically; tunnels can replace their transport with `reconnect`.
pub struct SessionFollower {
    state: watch::Receiver<FollowerState>,
    replace: mpsc::Sender<Connection>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for SessionFollower {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl SessionFollower {
    /// # Errors
    /// Initial snapshot or subscription failure.
    pub async fn new(connection: Connection, session_id: impl Into<String>) -> Result<Self> {
        let session_id = session_id.into();
        let events = connection.events();
        let mirror = SessionMirror::new(connection.session_view(&session_id).await?);
        connection.session_subscribe(&session_id, Some(mirror.last_seq)).await?;
        let (tx, state) = watch::channel(FollowerState { mirror, connected: true, error: None });
        let (replace, replacements) = mpsc::channel(1);
        let task = tokio::spawn(run(connection, session_id, events, tx, replacements));
        Ok(Self { state, replace, task })
    }
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<FollowerState> {
        self.state.clone()
    }
    #[must_use]
    pub fn current(&self) -> FollowerState {
        self.state.borrow().clone()
    }
    /// Install a freshly handshaken tunnel connection and resume from `last_seq`.
    /// # Errors
    /// The follower has stopped.
    pub async fn reconnect(&self, connection: Connection) -> Result<()> {
        self.replace.send(connection).await.map_err(|_| ClientError::Closed)
    }
}
async fn run(
    mut connection: Connection,
    id: String,
    mut events: crate::Events,
    tx: watch::Sender<FollowerState>,
    mut replacements: mpsc::Receiver<Connection>,
) {
    let mut state = tx.borrow().clone();
    loop {
        let incoming = tokio::select! {
            replacement = replacements.recv() => {
                let Some(c) = replacement else { return; };
                connection = c; events = connection.events(); state.mirror.clear_live();
                match connection.session_subscribe(&id, Some(state.mirror.last_seq)).await {
                    Ok(_) => { state.connected = true; state.error = None; tx.send_replace(state.clone()); }
                    Err(e) => { state.connected = false; state.error = Some(e); tx.send_replace(state.clone()); }
                }
                continue;
            }
            incoming = events.recv(), if state.connected => incoming,
            _ = tokio::time::sleep(Duration::from_millis(250)), if !state.connected => {
                match connection.reconnect().await {
                    Ok(c) => {
                        connection = c; events = connection.events(); state.mirror.clear_live();
                        match connection.session_subscribe(&id, Some(state.mirror.last_seq)).await {
                            Ok(_) => { state.connected = true; state.error = None; tx.send_replace(state.clone()); }
                            Err(e) => { state.error = Some(e); tx.send_replace(state.clone()); }
                        }
                    }
                    Err(e) => { state.error = Some(e); tx.send_replace(state.clone()); }
                }
                continue;
            }
        };
        let resync = match incoming {
            Ok(Ok(ServerMessage::Live { after_seq, session_id, event, .. })) if session_id == id => {
                let _ = state.mirror.apply_live(after_seq, &event);
                false
            }
            Ok(Ok(ServerMessage::Event { seq, session_id: Some(session), event })) if session == id => {
                matches!(state.mirror.apply(seq, &event), ApplyResult::Gap { .. })
            }
            Ok(Ok(ServerMessage::UnknownEvent { seq, session_id: Some(session) })) if session == id => {
                matches!(state.mirror.advance(seq), ApplyResult::Gap { .. })
            }
            Ok(Ok(ServerMessage::MalformedEvent { session_id: Some(session), .. })) if session == id => true,
            Err(broadcast::error::RecvError::Lagged(_)) => true,
            Ok(Err(error)) => {
                state.connected = false;
                state.error = Some(error);
                false
            }
            Err(broadcast::error::RecvError::Closed) => {
                state.connected = false;
                state.error = Some(ClientError::Closed);
                false
            }
            _ => continue,
        };
        if resync {
            // Subscribe the receiver before fetching: replay may precede the RPC acknowledgement.
            events = connection.events();
            let refreshed = async {
                let mirror = SessionMirror::new(connection.session_view(&id).await?);
                connection.session_subscribe(&id, Some(mirror.last_seq)).await?;
                Ok::<_, ClientError>(mirror)
            }
            .await;
            match refreshed {
                Ok(mirror) => {
                    state.mirror = mirror;
                    state.error = None;
                }
                Err(e) => {
                    state.error = Some(e);
                    state.connected = false;
                }
            }
        }
        tx.send_replace(state.clone());
    }
}
