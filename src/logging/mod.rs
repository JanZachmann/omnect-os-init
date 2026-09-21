//! Logging infrastructure for initramfs
//!
//! This module provides logging to /dev/kmsg with kernel log levels.

mod capture;
mod kmsg;

pub(crate) use self::capture::capture_record;
#[cfg(feature = "flash-mode")]
pub(crate) use self::capture::{start_capture, take_capture};
pub use self::kmsg::{
    KmsgLogger, KmsgRatelimitGuard, disable_kmsg_ratelimit, disable_printk_ratelimit, log_direct,
    log_fatal,
};
