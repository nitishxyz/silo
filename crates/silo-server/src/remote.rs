//! Remote clients over iroh (`silo/ctl/1`): the first bi stream is the
//! control stream; every further one is a data stream, accepted only once
//! the control hello is admitted.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use silo_transport::{BiStream, CtlIncoming, EndpointId, RemoteConn};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
    Server,
    auth::Peer,
    conn::{self, Expect},
};

/// Concurrent data streams per remote connection; extra streams are reset.
pub const MAX_DATA_STREAMS: usize = 64;
/// Connections whose control hello is not yet admitted, in total and per
/// peer; extra connections are closed at once.
pub const MAX_PENDING: usize = 32;
pub const MAX_PENDING_PER_PEER: usize = 4;
/// How long a final reply (e.g. a hello rejection) may take to reach the
/// peer before the connection is closed under it.
const LINGER: Duration = Duration::from_secs(1);

/// Admission slots for not-yet-authenticated connections.
#[derive(Clone)]
struct Pending {
    total: Arc<Semaphore>,
    per_peer: Arc<Mutex<HashMap<EndpointId, usize>>>,
}

/// A held slot; released on drop.
struct Slot {
    _permit: OwnedSemaphorePermit,
    peer: EndpointId,
    per_peer: Arc<Mutex<HashMap<EndpointId, usize>>>,
}

impl Pending {
    fn take(&self, peer: EndpointId) -> Option<Slot> {
        let permit = self.total.clone().try_acquire_owned().ok()?;
        let mut counts = self.per_peer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = counts.entry(peer).or_default();
        if *count >= MAX_PENDING_PER_PEER {
            return None;
        }
        *count += 1;
        Some(Slot { _permit: permit, peer, per_peer: self.per_peer.clone() })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut counts = self.per_peer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = counts.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.peer);
            }
        }
    }
}

pub async fn accept_loop(server: Arc<Server>, mut incoming: CtlIncoming) {
    let pending = Pending { total: Arc::new(Semaphore::new(MAX_PENDING)), per_peer: Arc::default() };
    while let Some((rc, first)) = incoming.next().await {
        let Some(slot) = pending.take(rc.remote_id()) else {
            tracing::debug!(peer = %rc.remote_id(), "too many unauthenticated connections");
            rc.close(b"busy");
            continue;
        };
        tokio::spawn(connection(server.clone(), rc, first, slot));
    }
}

async fn connection(server: Arc<Server>, rc: RemoteConn, first: BiStream, slot: Slot) {
    let endpoint_id = rc.remote_id().to_string();
    let peer = Peer::Remote { endpoint_id: endpoint_id.clone(), conn_id: rc.id() };
    server.auth().add_live(endpoint_id, rc.clone());
    let (admitted_tx, admitted) = tokio::sync::oneshot::channel();
    let streams = tokio::spawn({
        let (server, rc, peer) = (server.clone(), rc.clone(), peer.clone());
        async move {
            if admitted.await.is_ok() {
                drop(slot);
                data_streams(server, rc, peer).await;
            }
        }
    });
    conn::serve(server.clone(), first, peer, Expect::Control, Some(admitted_tx)).await;
    // The control stream ended: its data streams end with the connection.
    streams.abort();
    let _ = tokio::time::timeout(LINGER, rc.closed()).await;
    rc.close(b"control stream closed");
    server.auth().remove_live(rc.id());
}

async fn data_streams(server: Arc<Server>, rc: RemoteConn, peer: Peer) {
    let limit = Arc::new(Semaphore::new(MAX_DATA_STREAMS));
    while let Ok(stream) = rc.accept_stream().await {
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            tracing::debug!("too many data streams; dropping one");
            continue;
        };
        let (server, peer) = (server.clone(), peer.clone());
        tokio::spawn(async move {
            conn::serve(server, stream, peer, Expect::Data, None).await;
            drop(permit);
        });
    }
}
