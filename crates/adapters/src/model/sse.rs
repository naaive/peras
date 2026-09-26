//! Incremental Server-Sent Events parser (vendor neutral).

/// One dispatched SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// `event:` field (empty when absent).
    pub event: String,
    /// Joined `data:` lines.
    pub data: String,
}

/// Feed bytes in arbitrary chunks; complete events come out.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a chunk; returns events completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line).into_owned();
            self.line(&line, &mut out);
        }
        out
    }

    /// End of stream: flush a trailing event lacking the final blank line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            let rest = rest.trim_end_matches('\r').to_string();
            self.line(&rest, &mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return; // comment / keep-alive
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let v = &line[i + 1..];
                (&line[..i], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (line, ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event.clear();
            return;
        }
        out.push(SseEvent { event: std::mem::take(&mut self.event), data: self.data.join("\n") });
        self.data.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_across_chunks() {
        let mut p = SseParser::new();
        let mut all = p.push(b"event: a\r\ndata: {\"x\"");
        all.extend(p.push(b":1}\r\n\r\n: ping\n\ndata: one\ndata: two\n\n"));
        all.extend(p.push(b"data: tail"));
        all.extend(p.finish());
        assert_eq!(
            all,
            vec![
                SseEvent { event: "a".into(), data: "{\"x\":1}".into() },
                SseEvent { event: "".into(), data: "one\ntwo".into() },
                SseEvent { event: "".into(), data: "tail".into() },
            ]
        );
    }
}
