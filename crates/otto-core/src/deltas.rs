use crate::{Result, session::Context};
use otto_proto::Event;
use otto_store::{Coalescer, Projection};
pub(crate) struct Deltas {
    buffer: Coalescer,
    start: std::time::Instant,
}
impl Deltas {
    pub fn new(id: &str) -> Self {
        Self { buffer: Coalescer::new(id), start: std::time::Instant::now() }
    }
    pub async fn push(&mut self, cx: &Context, event: Event, turn: i64, step: i64) -> Result<()> {
        let _ = cx.deltas.send(event.clone());
        let record = crate::runtime::record(&event, turn, step)?;
        for record in self.buffer.push(record, self.start.elapsed()) {
            Self::persist(cx, record).await?;
        }
        Ok(())
    }
    pub async fn flush(&mut self, cx: &Context) -> Result<()> {
        if let Some(record) = self.buffer.flush() {
            Self::persist(cx, record).await?;
        }
        Ok(())
    }
    async fn persist(cx: &Context, record: otto_store::EventRecord) -> Result<()> {
        let event = serde_json::from_value(serde_json::json!({"event":record.kind,"data":record.payload}))?;
        cx.emit(event, Projection::None, record.turn, record.step).await?;
        Ok(())
    }
}
