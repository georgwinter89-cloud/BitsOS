//! Server-sent events (the framing all three provider kinds stream with): `event:` and `data:` lines, an empty
//! line ends one event. Bytes are cut into lines before they are decoded, so a character split across two reads
//! stays whole.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    /// Empty when the stream names none.
    pub event: String,
    pub data: String,
}

#[derive(Default)]
pub struct Sse {
    line: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
}

impl Sse {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for &b in bytes {
            if b != b'\n' {
                self.line.push(b);
                continue;
            }
            let mut line = std::mem::take(&mut self.line);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&String::from_utf8_lossy(&line), &mut out);
        }
        out
    }

    /// The stream ended: an event whose lines are complete counts even without its empty line (some local
    /// servers close right after the last one); a line without its end is cut data and dropped.
    pub fn end(&mut self) -> Vec<SseEvent> {
        self.line.clear();
        let mut out = Vec::new();
        self.line("", &mut out);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if self.has_data {
                out.push(SseEvent { event: std::mem::take(&mut self.event), data: std::mem::take(&mut self.data) });
            }
            self.event.clear();
            self.has_data = false;
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_survive_any_split_of_the_bytes() {
        let stream = "event: a\r\ndata: {\"t\":\"Grüße\"}\r\n\r\n: keep-alive\n\ndata: one\ndata: two\n\ndata:[DONE]\n\n";
        let want = vec![
            SseEvent { event: "a".into(), data: "{\"t\":\"Grüße\"}".into() },
            SseEvent { event: String::new(), data: "one\ntwo".into() },
            SseEvent { event: String::new(), data: "[DONE]".into() },
        ];
        for step in 1..stream.len() {
            let mut s = Sse::default();
            let mut got = Vec::new();
            for piece in stream.as_bytes().chunks(step) {
                got.extend(s.feed(piece));
            }
            assert_eq!(got, want, "step {step}");
            assert!(s.end().is_empty());
        }
    }

    #[test]
    fn the_end_keeps_whole_lines_and_drops_a_cut_one() {
        let mut s = Sse::default();
        assert!(s.feed(b"data: whole\ndata: {\"cut").is_empty());
        assert_eq!(s.end(), vec![SseEvent { event: String::new(), data: "whole".into() }]);
    }
}
