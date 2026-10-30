//! Server-sent events: `id`, `data` and `retry` fields of `text/event-stream`.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub id: Option<String>,
    pub data: String,
    pub retry: Option<u64>,
}

/// Incremental parser; feed it response chunks as they arrive.
#[derive(Default)]
pub struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend(chunk.iter().filter(|b| **b != b'\r'));
        let mut events = Vec::new();
        while let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let block: Vec<u8> = self.buffer.drain(..end + 2).collect();
            let mut event = SseEvent::default();
            let mut data = Vec::new();
            for line in String::from_utf8_lossy(&block).lines() {
                if line.starts_with(':') {
                    continue;
                }
                let (field, value) = line.split_once(':').map_or((line, ""), |(f, v)| (f, v.strip_prefix(' ').unwrap_or(v)));
                match field {
                    "data" => data.push(value.to_owned()),
                    "id" if !value.contains('\0') => event.id = Some(value.to_owned()),
                    "retry" => event.retry = value.parse().ok(),
                    _ => {}
                }
            }
            event.data = data.join("\n");
            if !data.is_empty() || event.id.is_some() || event.retry.is_some() {
                events.push(event);
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_split_chunks_and_comments() {
        let mut parser = Parser::default();
        assert!(parser.push(b": hello\r\n\r\nid: 7\r\ndata: {\"a\":").is_empty());
        let events = parser.push(b"1}\r\ndata: x\r\n\r\nretry: 250\n\n");
        assert_eq!(events[0], SseEvent { id: Some("7".into()), data: "{\"a\":1}\nx".into(), retry: None });
        assert_eq!(events[1].retry, Some(250));
    }
}
