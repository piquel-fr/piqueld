//! Presentation shared by the runtime and build terminals.
use piqueld_client::{BuildLogChunk, LogRecord, LogStream};

/// One rendered line, with metadata kept separate from its message.
#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    /// Original timestamp or Unix milliseconds.
    pub timestamp: String,
    /// Logical service name.
    pub service: String,
    /// Captured output stream.
    pub stream: String,
    /// Plain text without terminal control sequences.
    pub message: String,
}
impl LogLine {
    /// Converts runtime records into display lines.
    #[must_use]
    pub fn runtime(records: Vec<LogRecord>) -> Vec<Self> {
        records
            .into_iter()
            .map(|record| Self {
                timestamp: record.timestamp,
                service: record.service,
                stream: record.stream,
                message: LogRecord::clean_message(&record.message),
            })
            .collect()
    }

    // Reassemble partial lines independently for each stream: concurrent process
    // reads may split a line or interleave stderr before stdout's next chunk.
    /// Reassembles captured build chunks into display lines.
    #[must_use]
    pub fn build(chunks: &[BuildLogChunk], service: &str) -> Vec<Self> {
        let mut lines = Vec::new();
        for stream in [LogStream::Stdout, LogStream::Stderr] {
            let mut text = String::new();
            let mut starts = Vec::new();
            for chunk in chunks.iter().filter(|chunk| chunk.stream == stream) {
                starts.push((text.len(), chunk.offset, chunk.timestamp_ms));
                text.push_str(&chunk.text);
            }
            let mut position = 0;
            let mut chunk_index = 0;
            for line in text.split_inclusive('\n') {
                // Both line and chunk positions only advance; visit each
                // chunk boundary once, including partial and interleaved lines.
                while chunk_index + 1 < starts.len() && starts[chunk_index + 1].0 <= position {
                    chunk_index += 1;
                }
                let Some((_, offset, timestamp)) = starts.get(chunk_index) else {
                    continue;
                };
                lines.push((
                    *offset,
                    position,
                    Self {
                        timestamp: timestamp.to_string(),
                        service: service.into(),
                        stream: stream.as_str().into(),
                        message: LogRecord::clean_message(line.trim_end_matches('\n')),
                    },
                ));
                position += line.len();
            }
        }
        lines.sort_by_key(|(offset, position, _)| (*offset, *position));
        lines.into_iter().map(|(_, _, line)| line).collect()
    }

    /// CSS severity based on explicit error tokens, then stream identity.
    #[must_use]
    pub fn severity(&self) -> &'static str {
        if self
            .message
            .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .any(|word| word.eq_ignore_ascii_case("error") || word.eq_ignore_ascii_case("fatal"))
        {
            "error"
        } else if self.stream == "stderr" {
            "stderr"
        } else {
            "normal"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interleaved_chunks_keep_partial_lines_and_severity() {
        let chunk = |offset, stream, text: &str| BuildLogChunk {
            offset,
            stream,
            timestamp_ms: 1000,
            text: text.into(),
        };
        let lines = LogLine::build(
            &[
                chunk(0, LogStream::Stdout, "hel"),
                chunk(3, LogStream::Stderr, "progress\n"),
                chunk(12, LogStream::Stdout, "lo\n\x1b[31mERROR\x1b[0m failed\n"),
            ],
            "web",
        );
        assert_eq!(
            lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
            ["hello", "progress", "ERROR failed"]
        );
        assert_eq!(lines[1].severity(), "stderr");
        assert_eq!(lines[2].severity(), "error");
    }

    #[test]
    fn build_lines_keep_the_timestamp_of_their_first_chunk() {
        let chunks = [
            (0, LogStream::Stdout, "first\nse"),
            (8, LogStream::Stderr, "warn"),
            (12, LogStream::Stdout, "co"),
            (14, LogStream::Stdout, "nd\nthird\nfour"),
            (27, LogStream::Stderr, "ing\n"),
            (31, LogStream::Stdout, "th\nfifth"),
        ]
        .map(|(offset, stream, text)| BuildLogChunk {
            offset,
            stream,
            timestamp_ms: offset,
            text: text.into(),
        });
        let lines = LogLine::build(&chunks, "web");
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.message.as_str(), line.timestamp.as_str()))
                .collect::<Vec<_>>(),
            [
                ("first", "0"),
                ("second", "0"),
                ("warning", "8"),
                ("third", "14"),
                ("fourth", "14"),
                ("fifth", "31")
            ],
        );
    }
}
