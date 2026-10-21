//! Deterministic recorded-stream helpers.
use bytes::Bytes;
use futures::{Stream, StreamExt};
use rand::{Rng, SeedableRng};

use crate::{EventStream, LlmError, StreamEvent};

pub fn chunked(bytes: &[u8], seed: u64) -> impl Stream<Item = Result<Bytes, LlmError>> + Send + 'static {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut chunks = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + rng.random_range(1..=23)).min(bytes.len());
        chunks.push(Ok(Bytes::copy_from_slice(&bytes[offset..end])));
        offset = end;
    }
    futures::stream::iter(chunks)
}

/// Collect a successful fixture stream, failing the test on transport errors.
///
/// # Panics
/// Panics if the fixture emits an error.
pub async fn collect_events(stream: EventStream) -> Vec<StreamEvent> {
    stream.map(|event| event.expect("fixture stream failed")).collect().await
}
