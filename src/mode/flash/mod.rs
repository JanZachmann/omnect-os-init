//! Flash modes: deploy a whole disk image from the initramfs, before any
//! rootfs is handed control.
//!
//! The mode is selected through the bootloader environment and runs at most
//! once — the trigger is cleared before any work starts, so a crash mid-flash
//! leads to a normal boot attempt rather than an endless re-entry.

pub mod config;
#[cfg(feature = "flash-mode-1")]
pub mod rawio;
#[cfg(feature = "flash-mode-1")]
pub mod sfdisk;
#[cfg(feature = "flash-mode-1")]
pub mod unmount;
