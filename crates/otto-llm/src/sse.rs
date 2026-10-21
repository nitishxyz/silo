//! Incremental SSE framing. Incomplete events at EOF are discarded.
use std::collections::VecDeque;

use bytes::Bytes;
use futures::{Stream, StreamExt};

use crate::LlmError;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
    /// Reconnection delay in milliseconds.
    pub retry: Option<u64>,
}

#[derive(Default)]
pub struct SseParser {
    line: Vec<u8>,
    first_line: bool,
    started: bool,
    skip_lf: bool,
    pending: SseEvent,
    has_data: bool,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &byte in bytes {
            if !self.started {
                self.started = true;
                self.first_line = true;
            }
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.finish_line(&mut events);
                self.skip_lf = byte == b'\r';
            } else {
                self.line.push(byte);
            }
        }
        events
    }

    fn finish_line(&mut self, events: &mut Vec<SseEvent>) {
        let bytes = std::mem::take(&mut self.line);
        let mut line = String::from_utf8_lossy(&bytes).as_ref().to_owned();
        if self.first_line {
            line = line.trim_start_matches('\u{feff}').to_owned();
            self.first_line = false;
        }
        if line.is_empty() {
            if self.has_data {
                self.pending.data.pop();
                events.push(self.pending.clone());
            }
            self.pending.event = None;
            self.pending.data.clear();
            self.has_data = false;
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((&line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.pending.event = if value.is_empty() { None } else { Some(value.into()) },
            "data" => {
                self.pending.data.push_str(value);
                self.pending.data.push('\n');
                self.has_data = true;
            }
            "id" if !value.contains('\0') => self.pending.id = Some(value.into()),
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                if let Ok(delay) = value.parse() {
                    self.pending.retry = Some(delay);
                }
            }
            _ => {}
        }
    }
}

pub fn sse_stream<S>(stream: S) -> impl Stream<Item = Result<SseEvent, LlmError>> + Send
where
    S: Stream<Item = Result<Bytes, LlmError>> + Send + 'static,
{
    futures::stream::unfold(
        (Box::pin(stream), SseParser::new(), VecDeque::new(), false),
        |(mut stream, mut parser, mut queue, mut done)| async move {
            loop {
                if let Some(event) = queue.pop_front() {
                    return Some((Ok(event), (stream, parser, queue, done)));
                }
                if done {
                    return None;
                }
                match stream.next().await {
                    Some(Ok(bytes)) => queue.extend(parser.feed(&bytes)),
                    Some(Err(error)) => {
                        done = true;
                        return Some((Err(error), (stream, parser, queue, done)));
                    }
                    None => return None,
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn arbitrary_boundaries() {
        let raw = "\u{feff}: comment\r\nid: 7\rretry: 200\nevent: delta\r\ndata: hé🙂\r\ndata: second\r\n\r\ndata:\n\nid: bad\0id\nretry: -1\ndata: last\n\ndata: unfinished";
        let expected = SseParser::new().feed(raw.as_bytes());
        assert_eq!(expected.len(), 3);
        assert_eq!(expected[0].data, "hé🙂\nsecond");
        assert_eq!(expected[1].data, "");
        assert_eq!(expected[2].id.as_deref(), Some("7"));
        assert_eq!(expected[2].retry, Some(200));
        assert_eq!(expected[1].event, None);
        for seed in 0..100 {
            let got: Vec<_> = sse_stream(crate::testing::chunked(raw.as_bytes(), seed)).collect().await;
            assert_eq!(got.into_iter().collect::<Result<Vec<_>, _>>().unwrap(), expected);
        }
        for split in 0..raw.len() {
            let mut parser = SseParser::new();
            let mut got = parser.feed(&raw.as_bytes()[..split]);
            got.extend(parser.feed(&raw.as_bytes()[split..]));
            assert_eq!(got, expected);
        }
    }

    #[tokio::test]
    async fn errors_terminate_after_queued_events() {
        let error = LlmError::new(crate::ErrorClass::Network, "fixture disconnect");
        let input = futures::stream::iter(vec![
            Ok(Bytes::from_static(b"data: one\n\ndata: two\n\n")),
            Err(error.clone()),
            Ok(Bytes::from_static(b"data: ignored\n\n")),
        ]);
        let events: Vec<_> = sse_stream(input).collect().await;
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].as_ref().unwrap().data, "two");
        assert_eq!(events[2], Err(error));
    }

    #[test]
    fn bytewise_and_unknown_fields() {
        let raw = b":comment\nunknown: ignored\nretry: 99999999999999999999999999\nevent: unused\n\nevent:\ndata:  leading\ndata\n\n";
        let mut parser = SseParser::new();
        let events: Vec<_> = raw.iter().flat_map(|byte| parser.feed(&[*byte])).collect();
        assert_eq!(events, vec![SseEvent { data: " leading\n".into(), ..SseEvent::default() }]);
    }

    #[test]
    fn id_reset_and_invalid_utf8() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"id: first\n\nid:\ndata: \xff\n\n");
        assert_eq!(events[0].id.as_deref(), Some(""));
        assert_eq!(events[0].data, "\u{fffd}");
    }
}
