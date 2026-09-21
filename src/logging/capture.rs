//! Optional in-memory copy of everything the logger emits.
//!
//! A flash mode ends in a power off, so the kernel ring buffer is gone the
//! moment it finishes. Turning the capture on lets the mode write the whole run
//! to a disk it did not touch, which is the only post-mortem a failed flash
//! leaves behind.

use std::cmp::Ordering;
use std::sync::Mutex;

use log::Record;

/// Upper bound on captured lines.
///
/// The buffer sits in initramfs RAM until the mode writes it out, so a step
/// that logs in a retry loop must not be able to grow it without limit. A clone
/// logs on the order of tens of lines, so the bound only ever bites on a run
/// that has already gone wrong.
const CAPTURE_MAX_LINES: usize = 4096;

/// `Some` while a mode is capturing. Never held across a call that logs, so the
/// logger cannot deadlock on it.
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
    let Ok(mut capture) = CAPTURE.lock() else {
        return;
    };
    let Some(buffer) = capture.as_mut() else {
        return;
    };
    push_bounded(buffer, format!("[{}] {}", record.level(), record.args()));
}

/// Begin capturing, discarding anything an earlier capture left behind.
pub fn start_capture() {
    if let Ok(mut capture) = CAPTURE.lock() {
        *capture = Some(Vec::new());
    }
}

/// Stop capturing and hand back what was collected.
///
/// Empty when no capture was running or the lock is poisoned — the caller
/// persists what it gets and does not treat emptiness as an error.
pub fn take_capture() -> Vec<String> {
    CAPTURE
        .lock()
        .ok()
        .and_then(|mut capture| capture.take())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture is process-global, so the tests that switch it on and off
    /// run one at a time.
    static SERIALIZE: Mutex<()> = Mutex::new(());

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
    fn taking_the_capture_switches_it_off_again() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        start_capture();
        let _ = take_capture();
        log_at_info("after the mode finished");
        assert!(take_capture().is_empty());
    }

    /// Feeds the capture without needing write access to `/dev/kmsg`, which
    /// constructing the real logger requires.
    struct CaptureOnlyLogger;

    impl log::Log for CaptureOnlyLogger {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &Record) {
            capture_record(record);
        }

        fn flush(&self) {}
    }

    #[test]
    fn the_log_macros_reach_a_running_capture() {
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            log::set_boxed_logger(Box::new(CaptureOnlyLogger)).is_ok(),
            "this is the only test in the binary that installs a logger"
        );
        log::set_max_level(log::LevelFilter::Debug);

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
