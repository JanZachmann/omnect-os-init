//! Kernel message buffer (kmsg) logging
//!
//! This module provides a logger that writes to /dev/kmsg with proper
//! kernel log level prefixes.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

use log::{Level, Log, Metadata, Record, SetLoggerError};

/// Log message prefix for all omnect-os-init messages
const LOG_PREFIX: &str = "omnect-os-initramfs: ";

/// Path to kernel message buffer
const KMSG_PATH: &str = "/dev/kmsg";

/// Logger that writes to /dev/kmsg
pub struct KmsgLogger {
    kmsg: Mutex<File>,
}

impl KmsgLogger {
    /// Create a new kmsg logger
    ///
    /// # Errors
    /// Returns an error if /dev/kmsg cannot be opened for writing
    pub fn new() -> std::io::Result<Self> {
        let file = OpenOptions::new().write(true).open(KMSG_PATH)?;

        Ok(Self {
            kmsg: Mutex::new(file),
        })
    }

    /// Initialize the global logger with kmsg output
    ///
    /// Convenience method that creates a new logger and sets it as global.
    ///
    /// # Errors
    /// Returns an error if /dev/kmsg cannot be opened or a logger is already set
    pub fn init_global() -> std::result::Result<(), String> {
        let logger = Self::new().map_err(|e| format!("Failed to open kmsg: {}", e))?;
        logger
            .init()
            .map_err(|e| format!("Failed to set logger: {}", e))
    }

    /// Initialize this logger as the global logger
    ///
    /// # Errors
    /// Returns an error if a logger has already been set
    pub fn init(self) -> std::result::Result<(), SetLoggerError> {
        log::set_max_level(log::LevelFilter::Debug);
        log::set_boxed_logger(Box::new(self))
    }

    // Kernel syslog level prefixes: <3>=err, <4>=warning, <6>=info, <7>=debug/trace.
    fn level_to_kernel_prefix(level: Level) -> &'static str {
        match level {
            Level::Error => "<3>",
            Level::Warn => "<4>",
            Level::Info => "<6>",
            Level::Debug => "<7>",
            Level::Trace => "<7>",
        }
    }
}

impl Log for KmsgLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let prefix = Self::level_to_kernel_prefix(record.level());
        let message = format!("{}{}{}\n", prefix, LOG_PREFIX, record.args());

        {
            let mut kmsg = self.kmsg.lock().unwrap_or_else(|p| p.into_inner());
            let _ = kmsg.write_all(message.as_bytes());
        }

        crate::logging::capture_record(record);
    }

    fn flush(&self) {
        let mut kmsg = self.kmsg.lock().unwrap_or_else(|p| p.into_inner());
        let _ = kmsg.flush();
    }
}

/// Write a fatal message to kmsg and prepare for system halt
///
/// This function is used when a fatal error occurs and we need to
/// log before potentially halting the system.
pub fn log_fatal(message: &str) {
    if let Ok(mut file) = OpenOptions::new().write(true).open(KMSG_PATH) {
        let _ = writeln!(file, "<2>{}FATAL: {}", LOG_PREFIX, message);
    }
}

/// Write directly to kmsg without going through the logger
///
/// Useful for early initialization before the logger is set up.
pub fn log_direct(message: &str) {
    if let Ok(mut file) = OpenOptions::new().write(true).open(KMSG_PATH) {
        let _ = writeln!(file, "<6>{}{}", LOG_PREFIX, message);
    }
}

/// Path to kernel printk rate limit control files
const PRINTK_RATELIMIT_PATH: &str = "/proc/sys/kernel/printk_ratelimit";
const PRINTK_RATELIMIT_BURST_PATH: &str = "/proc/sys/kernel/printk_ratelimit_burst";

/// Path to the printk devkmsg control file
const PRINTK_DEVKMSG_PATH: &str = "/proc/sys/kernel/printk_devkmsg";

/// Saved rate limit values for restoration
static SAVED_RATELIMIT: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

/// RAII guard that re-enables kmsg rate limiting when dropped.
///
/// Guarantees restoration on all exit paths including early error returns.
pub struct KmsgRatelimitGuard;

impl Drop for KmsgRatelimitGuard {
    fn drop(&mut self) {
        enable_kmsg_ratelimit();
    }
}

/// Disable kernel message rate limiting for the duration of an operation.
///
/// Saves the current ratelimit and burst values and zeroes them out so
/// that high-volume output (e.g. fsck) is not suppressed in dmesg.
/// Call `enable_kmsg_ratelimit` (or drop `KmsgRatelimitGuard`) to restore.
pub fn disable_kmsg_ratelimit() {
    let ratelimit = match std::fs::read_to_string(PRINTK_RATELIMIT_PATH) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            log::warn!("Failed to read {PRINTK_RATELIMIT_PATH}: {e}; skipping ratelimit save");
            return;
        }
    };
    let burst = match std::fs::read_to_string(PRINTK_RATELIMIT_BURST_PATH) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            log::warn!(
                "Failed to read {PRINTK_RATELIMIT_BURST_PATH}: {e}; skipping ratelimit save"
            );
            return;
        }
    };

    if let Ok(mut saved) = SAVED_RATELIMIT.lock() {
        *saved = Some((ratelimit, burst));
    } else {
        log::debug!(
            "SAVED_RATELIMIT mutex poisoned; original ratelimit values will not be restored"
        );
    }

    let _ = std::fs::write(PRINTK_RATELIMIT_PATH, "0");
    let _ = std::fs::write(PRINTK_RATELIMIT_BURST_PATH, "0");
}

/// Re-enable kernel message rate limiting by restoring previously saved values.
fn enable_kmsg_ratelimit() {
    if let Ok(mut saved) = SAVED_RATELIMIT.lock()
        && let Some((ratelimit, burst)) = saved.take()
    {
        let _ = std::fs::write(PRINTK_RATELIMIT_PATH, ratelimit);
        let _ = std::fs::write(PRINTK_RATELIMIT_BURST_PATH, burst);
    }
}

/// Disable per-message rate limiting for /dev/kmsg writes.
///
/// The kernel rate-limits messages written to /dev/kmsg by default. During
/// early init we want all messages logged without suppression.
/// Best-effort: failures are silently ignored.
pub fn disable_printk_ratelimit() {
    let _ = std::fs::write(PRINTK_DEVKMSG_PATH, "on\n");
}
