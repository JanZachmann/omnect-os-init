//! Logging infrastructure for initramfs
//!
//! This module provides logging to /dev/kmsg with kernel log levels.

pub(crate) mod capture;
mod kmsg;

pub(crate) use self::capture::capture_record;
#[cfg(feature = "flash-mode")]
pub(crate) use self::capture::{start_capture, take_capture};
pub use self::kmsg::{
    KmsgLogger, KmsgRatelimitGuard, disable_kmsg_ratelimit, disable_printk_ratelimit, log_direct,
    log_fatal,
};

/// The last `count` non-empty lines of a child's output, joined for a log line
/// or an error reason. A carriage return ends a line, as in progress output.
pub(crate) fn output_tail(output: &[u8], count: usize) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text
        .split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return "(no output)".to_string();
    }
    lines[lines.len().saturating_sub(count)..].join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_tail_keeps_the_last_non_empty_lines() {
        let output = b"first\n\nsecond\r\nprogress 10%\rprogress 100%\n  last  \n";
        assert_eq!(output_tail(output, 2), "progress 100%; last");
        assert_eq!(output_tail(b"only\n", 3), "only");
    }

    #[test]
    fn output_tail_names_an_empty_output() {
        assert_eq!(output_tail(b"", 3), "(no output)");
        assert_eq!(output_tail(b" \n\r\n", 3), "(no output)");
    }
}
