//! A small decoder for the server-sent event streams `relish logs -f` reads.
//!
//! Both ends of a cluster-wide follow speak SSE: the node the CLI talks to
//! reads each peer's stream and re-emits the lines, and the CLI reads the
//! merged stream. The decoder is incremental because a network read can end
//! anywhere, including halfway through a UTF-8 character.

/// One decoded event: its `event:` type, if any, and its `data:` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field; `None` for the default `message` type.
    pub event: Option<String>,
    /// Every `data:` line of the event, joined with `\n`.
    pub data: String,
}

/// The event type a cluster-wide log follow uses for "a node's stream
/// stopped" notices, which the CLI prints to stderr instead of stdout.
pub const WARNING_EVENT: &str = "warning";

/// Incremental SSE decoder: feed it bytes, collect complete events.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    /// Add bytes from the stream and return every event they completed.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.windows(2).position(|pair| pair == b"\n\n") {
            let block: Vec<u8> = self.buffer.drain(..end + 2).collect();
            if let Some(event) = parse_block(&block[..end]) {
                events.push(event);
            }
        }
        events
    }

    /// Decode whatever is left when the stream ends without a final blank line.
    pub fn finish(self) -> Option<SseEvent> {
        parse_block(&self.buffer)
    }
}

fn parse_block(block: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(block);
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in text.lines() {
        // The spec strips exactly one space after the colon, so a log line's
        // own indentation survives.
        let field = |name: &str| {
            line.strip_prefix(name)
                .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
        };
        if let Some(value) = field("data:") {
            data.push(value);
        } else if let Some(value) = field("event:") {
            event = Some(value.to_string());
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseEvent {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_reads_are_reassembled() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(b"data: [node-2 default__web-0] hel")
                .is_empty()
        );
        let events = decoder.push(b"lo\n\ndata: second\n\n");
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: None,
                    data: "[node-2 default__web-0] hello".to_string(),
                },
                SseEvent {
                    event: None,
                    data: "second".to_string(),
                },
            ]
        );
    }

    #[test]
    fn warning_events_keep_their_type() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push(b"event: warning\ndata: node-3 stopped streaming\n\n");
        assert_eq!(events[0].event.as_deref(), Some(WARNING_EVENT));
        assert_eq!(events[0].data, "node-3 stopped streaming");
    }

    #[test]
    fn indentation_after_the_single_separator_space_survives() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push(b"data:   at frame 3\n\n");
        assert_eq!(events[0].data, "  at frame 3");
    }

    #[test]
    fn a_trailing_event_without_a_blank_line_is_not_lost() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: last words").is_empty());
        assert_eq!(decoder.finish().unwrap().data, "last words");
    }

    #[test]
    fn comments_and_keepalives_are_not_events() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b": keep-alive\n\n").is_empty());
    }
}
