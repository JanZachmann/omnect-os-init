//! Runtime setup and integration modules
//!
//! This module handles:
//! - omnect-device-service runtime file creation
//! - fs-link symbolic link creation
//! - switch_root to final rootfs

mod fs_link;
mod omnect_device_service;
mod switch_root;

pub use self::fs_link::create_fs_links;
#[cfg(all(test, feature = "factory-reset"))]
pub(crate) use self::omnect_device_service::assert_factory_reset_contract;
pub use self::omnect_device_service::{
    ExtraBootArgsOutcome, ExtraBootArgsStatus, FactoryResetStatus, FactoryResetStatusCode,
    FsckStatus, ODS_RUNTIME_DIR, OdsStatus, ResizeOutcome, ResizeStatus, create_ods_runtime_files,
};
pub use self::switch_root::switch_root;
