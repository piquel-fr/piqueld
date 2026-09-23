use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct Recorded {
    bytes: Vec<u8>,
    flushes: usize,
}
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Recorded>>);
impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().bytes.clone()).unwrap()
    }
    fn flushes(&self) -> usize {
        self.0.lock().unwrap().flushes
    }
    fn writer(&self) -> Writer {
        Box::new(anstream::StripStream::new(Box::new(self.clone()) as Writer))
    }
}
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flushes += 1;
        Ok(())
    }
}

struct Sample;
impl Report for Sample {
    type Json = str;
    fn json(&self) -> &'static str {
        "result"
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        out.label("Result", "result")
    }
}
impl DiagnosticReport for Sample {
    fn render(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        out.label("Error", "failure")?;
        out.label("Hint", "retry")
    }
}

#[test]
fn roles_route_and_flush_before_returning_in_every_mode() {
    for json in [false, true] {
        for quiet in [false, true] {
            let stdout = Capture::default();
            let stderr = Capture::default();
            let mut console =
                Console::with_writers(json, quiet, false, stdout.writer(), stderr.writer());
            console.emit(&Sample).unwrap();
            assert_eq!(
                stdout.text(),
                if json {
                    "\"result\"\n"
                } else if quiet {
                    ""
                } else {
                    "Result: result\n"
                }
            );
            assert_eq!(stdout.flushes(), usize::from(json || !quiet));
            console.info("routine").unwrap();
            assert_eq!(stderr.text().contains("routine"), !quiet);
            console.warning("incomplete").unwrap();
            assert!(stderr.text().contains("Warning: incomplete\n"));
            let before = stderr.flushes();
            console.warning_report(&Sample).unwrap();
            assert_eq!(stderr.flushes(), before + 1, "context is one flushed event");
            console.prompt("Continue? ").unwrap();
            assert!(stderr.text().ends_with("Continue? "));
            console.error(&Sample);
            let task = console.start_task("task");
            task.update("running");
            task.finish(TaskOutcome::Succeeded, "done");
            assert_eq!(stderr.text().contains("task:"), !quiet);
            assert!(!stdout.text().contains("task"));
        }
    }
}

#[test]
fn dynamic_controls_are_escaped_and_log_whitespace_is_preserved() {
    let output = Capture::default();
    let mut writer = output.writer();
    let mut human = HumanWriter::new(writer.as_mut());
    human.label("Value", "café\n\t\r\x1b[31m\u{85}").unwrap();
    human.change('+', "name\x1b[2J", "value\nnext").unwrap();
    human.log_text("one\n\ttwo\r\x1b[2J").unwrap();
    assert_eq!(
        output.text(),
        "Value: café\\n\\t\\r\\u{1b}[31m\\u{85}\n  + name\\u{1b}[2J   value\\nnext\none\n\ttwo\\r\\u{1b}[2J"
    );
}

#[test]
fn json_keeps_original_log_values_without_human_escaping() {
    let stdout = Capture::default();
    let mut console =
        Console::with_writers(true, false, false, stdout.writer(), Box::new(io::sink()));
    let page = piqueld_client::BuildLogPage {
        items: vec![piqueld_client::BuildLogChunk {
            offset: 42,
            timestamp_ms: 1000,
            stream: piqueld_client::LogStream::Stdout,
            text: "café\n\t\r\x1b[31m".into(),
        }],
        previous_offset: Some(42),
        truncated: true,
        expired: false,
    };
    console.emit(&page).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stdout.text()).unwrap(),
        serde_json::to_value(&page).unwrap()
    );
    assert_eq!(stdout.flushes(), 1);
}

#[test]
fn task_clones_share_completion_and_plain_updates_are_deduplicated() {
    let stderr = Capture::default();
    let mut console =
        Console::with_writers(false, false, false, Box::new(io::sink()), stderr.writer());
    let a = console.start_task("a\x1b[2J");
    let b = console.start_task("b");
    let child = a.clone();
    std::thread::spawn(move || {
        child.update("first");
        child.update("first");
    })
    .join()
    .unwrap();
    assert!(
        stderr.text().contains("first"),
        "updates are visible before completion"
    );
    console.warning("between tasks").unwrap();
    b.finish(TaskOutcome::Skipped, "already done");
    a.finish(TaskOutcome::Succeeded, "done");
    a.finish(TaskOutcome::Failed, "must not appear");
    a.update("must not appear");
    drop(a);
    drop(b);
    let abandoned = console.start_task("abandoned");
    abandoned.update("working");
    drop(abandoned);
    let text = stderr.text();
    assert_eq!(text.matches("first").count(), 1);
    assert_eq!(text.matches("succeeded").count(), 1);
    assert_eq!(text.matches("skipped").count(), 1);
    assert!(!text.contains("must not appear"));
    assert!(!text.contains("cancelled"));
    assert!(!text.contains('\x1b'));
    assert!(text.contains("a\\u{1b}[2J"));
}

struct Broken;
impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::ErrorKind::BrokenPipe.into())
    }
}
struct FlushFailure;
impl Write for FlushFailure {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::ErrorKind::BrokenPipe.into())
    }
}

struct MustNotRender;
impl Report for MustNotRender {
    type Json = str;
    fn json(&self) -> &str {
        panic!("hidden")
    }
    fn render_human(&self, _: &mut HumanWriter<'_>) -> io::Result<()> {
        panic!("hidden")
    }
}

#[test]
fn writes_and_flushes_fail_without_panicking_and_hidden_results_do_not_render() {
    for json in [false, true] {
        for stdout in [Box::new(Broken) as Writer, Box::new(FlushFailure) as Writer] {
            let mut console = Console::with_writers(json, false, false, stdout, Box::new(Broken));
            assert!(
                console
                    .emit(&Sample)
                    .unwrap_err()
                    .to_string()
                    .contains("could not write output")
            );
            assert!(console.info("info").is_err());
            assert!(console.warning("warning").is_err());
            assert!(console.warning_report(&Sample).is_err());
            assert!(console.prompt("prompt").is_err());
            console.error(&Sample);
            let task = console.start_task("task");
            task.update("running");
            task.finish(TaskOutcome::Failed, "failed");
        }
    }
    let mut quiet = Console::with_writers(false, true, false, Box::new(Broken), Box::new(Broken));
    quiet.emit(&MustNotRender).unwrap();
    quiet.info("hidden").unwrap();
}

/// Tracks whether a transient row occupies the terminal, without depending on
/// spinner frames, elapsed time, or platform-specific cursor escape sequences.
#[derive(Clone, Debug, Default)]
struct FakeTerminal(Arc<AtomicBool>);
impl indicatif::TermLike for FakeTerminal {
    fn width(&self) -> u16 {
        120
    }
    fn move_cursor_up(&self, _: usize) -> io::Result<()> {
        Ok(())
    }
    fn move_cursor_down(&self, _: usize) -> io::Result<()> {
        Ok(())
    }
    fn move_cursor_left(&self, _: usize) -> io::Result<()> {
        Ok(())
    }
    fn move_cursor_right(&self, _: usize) -> io::Result<()> {
        Ok(())
    }
    fn write_line(&self, text: &str) -> io::Result<()> {
        self.write_str(text)
    }
    fn write_str(&self, text: &str) -> io::Result<()> {
        if !text.trim().is_empty() {
            self.0.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn clear_line(&self) -> io::Result<()> {
        self.0.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}
struct SuspendedWriter {
    terminal: FakeTerminal,
    capture: Capture,
}
impl Write for SuspendedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        assert!(
            !self.terminal.0.load(Ordering::SeqCst),
            "progress must be cleared before ordinary output"
        );
        self.capture.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        assert!(
            !self.terminal.0.load(Ordering::SeqCst),
            "progress must remain suspended through flush"
        );
        self.capture.flush()
    }
}

#[test]
fn terminal_progress_is_suspended_for_every_role_and_cleared_on_final_drop() {
    let terminal = FakeTerminal::default();
    let capture = Capture::default();
    let writer = || {
        Box::new(SuspendedWriter {
            terminal: terminal.clone(),
            capture: capture.clone(),
        }) as Writer
    };
    let mut console = Console::with_writers(false, false, true, writer(), writer());
    console
        .progress
        .set_draw_target(indicatif::ProgressDrawTarget::term_like(Box::new(
            terminal.clone(),
        )));
    let first = console.start_task("first");
    let second = console.start_task("second");
    first.update("working");
    second.update("waiting");
    console.emit(&Sample).unwrap();
    assert!(
        terminal.0.load(Ordering::SeqCst),
        "active progress is redrawn after a result"
    );
    console.info("information").unwrap();
    console.warning("warning").unwrap();
    console.warning_report(&Sample).unwrap();
    console.prompt("Continue? ").unwrap();
    console.error(&Sample);
    first.finish(TaskOutcome::Succeeded, "done");
    drop(first);
    let clone = second.clone();
    drop(second);
    assert!(terminal.0.load(Ordering::SeqCst));
    drop(clone);
    assert!(
        !terminal.0.load(Ordering::SeqCst),
        "final drop clears the abandoned row"
    );
    console.warning("after tasks").unwrap();
    assert_eq!(capture.text().matches("first: succeeded").count(), 1);
    assert!(
        !capture.text().contains("second:"),
        "abandoning a task does not invent an outcome"
    );
}
