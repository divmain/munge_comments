//! The whole logging surface of the library: a two-method trait.
//!
//! Library code never writes to the console; progress lines and warnings go
//! through an injected [`Log`] sink. The CLI binds it to the injected stderr;
//! tests and library consumers capture or discard it.

use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A logging sink. Implementations must be usable from any thread.
pub trait Log: Send + Sync {
    /// A progress/status line.
    fn info(&self, message: &str);
    /// A warning line.
    fn warn(&self, message: &str);
}

/// A [`Log`] writing to any [`io::Write`](std::io::Write) sink behind a mutex.
///
/// Warnings are prefixed with `warning: ` so they stand out in the stream.
/// Write errors are ignored (a broken progress sink must never fail a run),
/// and a poisoned lock still holds usable data, so logging recovers from a
/// panic elsewhere in the process.
pub struct WriterLog<W: Write> {
    writer: Mutex<W>,
}

impl<W: Write> WriterLog<W> {
    /// Wraps `writer`.
    pub fn new(writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }

    fn lock(&self) -> MutexGuard<'_, W> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<W: Write + Send> Log for WriterLog<W> {
    fn info(&self, message: &str) {
        let _ = writeln!(self.lock(), "{message}");
    }

    fn warn(&self, message: &str) {
        let _ = writeln!(self.lock(), "warning: {message}");
    }
}

/// A [`Log`] recording lines in memory, for tests and library consumers.
#[derive(Default)]
pub struct CapturingLog {
    lines: Mutex<Vec<String>>,
}

impl CapturingLog {
    /// A new, empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// The lines recorded so far, in order.
    pub fn lines(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Log for CapturingLog {
    fn info(&self, message: &str) {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message.to_string());
    }

    fn warn(&self, message: &str) {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message.to_string());
    }
}

/// A [`Log`] that discards everything.
#[derive(Default)]
pub struct NullLog;

impl Log for NullLog {
    fn info(&self, _message: &str) {}

    fn warn(&self, _message: &str) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn writer_log_writes_info_and_prefixed_warnings() {
        let log = WriterLog::new(Vec::<u8>::new());
        log.info("scanning src/lib.rs");
        log.warn("drift detected");

        let buffer: Vec<u8> = log.lock().clone();
        let text = String::from_utf8(buffer).expect("log output is UTF-8");
        assert_eq!(text, "scanning src/lib.rs\nwarning: drift detected\n");
    }

    #[test]
    fn writer_log_ignores_write_errors() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("sink closed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("sink closed"))
            }
        }
        let log = WriterLog::new(Failing);
        log.info("must not panic");
        log.warn("must not panic either");
    }

    #[test]
    fn writer_log_recovers_from_a_poisoned_lock() {
        let log = Arc::new(WriterLog::new(Vec::<u8>::new()));
        let poisoned = Arc::clone(&log);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock();
            panic!("poison the writer mutex");
        })
        .join();

        // The mutex is poisoned now; logging must still work and use the
        // data the poisoned guard left behind.
        log.info("after poison");
        let buffer: Vec<u8> = log.lock().clone();
        let text = String::from_utf8(buffer).expect("log output is UTF-8");
        assert_eq!(text, "after poison\n");
    }

    #[test]
    fn capturing_log_records_lines_in_order() {
        let log = CapturingLog::new();
        log.info("first");
        log.warn("second");
        log.info("third");
        assert_eq!(log.lines(), ["first", "second", "third"]);
    }

    #[test]
    fn null_log_discards_everything() {
        let log = NullLog;
        log.info("gone");
        log.warn("also gone");
    }
}
