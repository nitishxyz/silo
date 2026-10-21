//! Confirm before closing busy blocks (Phase 7 §7.5): `blocks.busy` and
//! the check run before every close.

use serde_json::Value;
use silo_proto::{
    api::method_name,
    control::{Method, RpcError},
};

use crate::Server;

/// May `method` (a close) from `client_id` go ahead?
///
/// # Errors
/// `busy` when it would end running programs (not yet enforced).
pub fn check(_server: &Server, _client_id: u64, _method: &Method) -> Result<(), RpcError> {
    Ok(())
}

pub async fn handle(_server: &Server, _client_id: u64, method: Method) -> Result<Value, RpcError> {
    Err(RpcError::method_not_found(&method_name(&method)))
}
