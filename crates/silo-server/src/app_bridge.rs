//! `app.*` methods, forwarded to a connected Silo app (Phase 7 §4.2).

use serde_json::Value;
use silo_proto::{
    api::method_name,
    control::{Method, RpcError},
};

use crate::Server;

pub async fn handle(_server: &Server, _client_id: u64, method: Method) -> Result<Value, RpcError> {
    Err(RpcError::method_not_found(&method_name(&method)))
}
