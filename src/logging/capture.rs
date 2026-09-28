//! Optional in-memory copy of everything the logger emits.
//!
//! The design assumes a single execution stream: nothing in the crate spawns a
//! thread, and a mode's `run` calls `start_capture` and `take_capture` once
//! each. That is what makes the `Relaxed` flag enough — introducing a thread
//! would need the ordering between the flag and the buffer behind it to be
//! reconsidered.

use std::cmp::Ordering;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use log::Record;

/// Captured lines kept before the capture stops with one marker line, so a
/// step that logs in a loop cannot grow the buffer without limit.
const CAPTURE_MAX_LINES: usize = 4096;

/// Whether a capture is running. Checked before the lock, so a boot that never
/// captures pays one atomic load per record instead of a lock acquisition.
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// `Some` while a mode is capturing.
static CAPTURE: Mutex<Option<Vec<String>>> = Mutex::new(None);

fn push_bounded(buffer: &mut Vec<String>, line: String) {
    match buffer.len().cmp(&CAPTURE_MAX_LINES) {
        Ordering::Less => buffer.push(line),
        Ordering::Equal => buffer.push(format!(
            "log capture stopped after {CAPTURE_MAX_LINES} lines"
        )),
        Ordering::Greater => {}
    }
}

/// Copy one record into the capture, if one is running.
///
/// Every failure path here is a silent return: a poisoned lock costs the log,
/// and losing the log must never cost the boot.
pub(crate) fn capture_record(record: &Record) {
    if !CAPTURING.load(AtomicOrdering::Relaxed) {
        return;
    }

    // Formatting runs the call site's Display impls, so it happens before the
    // lock is taken. A capture taken in between only costs this one line.
    let line = format!("[{}] {}", record.level(), record.args());

    let Ok(mut capture) = CAPTURE.lock() else {
        return;
    };
    let Some(buffer) = capture.as_mut() else {
        return;
    };
    push_bounded(buffer, line);
}

/// Begin capturing, discarding anything an earlier capture left behind.
#[cfg(any(test, feature = "flash-mode"))]
pub(crate) fn start_capture() {
    if let Ok(mut capture) = CAPTURE.lock() {
        *capture = Some(Vec::new());
        // Last, so the flag is never true without a buffer behind it.
        CAPTURING.store(true, AtomicOrdering::Relaxed);
    }
}

/// Stop capturing and hand back what was collected.
///
/// Empty when no capture was running or the lock is poisoned.
#[cfg(any(test, feature = "flash-mode"))]
pub(crate) fn take_capture() -> Vec<String> {
    CAPTURING.store(false, AtomicOrdering::Relaxed);
    CAPTURE
        .lock()
        .ok()
        .and_then(|mut capture| capture.take())
        .unwrap_or_default()
}

/// The capture is process-global, so every test that switches it on and off
/// holds this lock.
#[cfg(test)]
pub(crate) static SERIALIZE: Mutex<()> = Mutex::new(());

/// Feeds the capture without needing write access to `/dev/kmsg`, which
/// constructing the real logger requires.
#[cfg(test)]
struct CaptureOnlyLogger;

#[cfg(test)]
impl log::Log for CaptureOnlyLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        capture_record(record);
    }

    fn flush(&self) {}
}

/// Route the `log` macros into the capture for the rest of the test binary.
///
/// Every test that logs can then reach a running capture, so a test asserts
/// only that its own lines are in it, never on the whole content.
#[cfg(test)]
pub(crate) fn install_test_logger() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let _ = log::set_boxed_logger(Box::new(CaptureOnlyLogger));
        log::set_max_level(log::LevelFilter::Debug);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_at_info(message: &str) {
        capture_record(
            &Record::builder()
                .level(log::Level::Info)
                .args(format_args!("{message}"))
                .build(),
        );
    }

    #[test]
    fn a_record_is_dropped_when_no_capture_is_running() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        log_at_info("before any capture");
        assert!(take_capture().is_empty());
    }

    #[test]
    fn a_running_capture_keeps_the_level_and_the_message() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        start_capture();
        log_at_info("cloning /dev/sda onto /dev/sdb");
        let captured = take_capture();
        assert!(
            captured.contains(&"[INFO] cloning /dev/sda onto /dev/sdb".to_string()),
            "got {captured:?}"
        );
    }

    #[test]
    fn a_cleared_flag_stops_the_capture_before_the_lock_is_reached() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        start_capture();
        CAPTURING.store(false, AtomicOrdering::Relaxed);
        log_at_info("must not be captured");
        assert!(take_capture().is_empty());
    }

    #[test]
    fn taking_the_capture_switches_it_off_again() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        start_capture();
        let _ = take_capture();
        log_at_info("after the mode finished");
        assert!(take_capture().is_empty());
    }

    #[test]
    fn the_log_macros_reach_a_running_capture() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        install_test_logger();

        start_capture();
        log::warn!("destination {} did not appear", "/dev/sdb");
        let captured = take_capture();
        assert!(
            captured.contains(&"[WARN] destination /dev/sdb did not appear".to_string()),
            "got {captured:?}"
        );
    }

    #[test]
    fn a_full_buffer_stops_growing_and_says_so() {
        // On a local buffer: the bound must hold regardless of what else in the
        // test binary is logging at the time.
        let mut buffer = Vec::new();
        for i in 0..CAPTURE_MAX_LINES + 10 {
            push_bounded(&mut buffer, format!("line {i}"));
        }
        assert_eq!(buffer.len(), CAPTURE_MAX_LINES + 1);
        assert_eq!(
            buffer[CAPTURE_MAX_LINES],
            format!("log capture stopped after {CAPTURE_MAX_LINES} lines")
        );
    }
}
