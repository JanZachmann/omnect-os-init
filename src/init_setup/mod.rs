//! Init setup: conditional one-time prep steps that run after core mount
//! and the bootloader env is open, but before mode dispatch.
//!
//! Steps are idempotent — guarded by bootloader env or filesystem state so each
//! runs at most once per trigger. Steps may be feature-gated.

pub mod extra_bootargs;
#[cfg(feature = "resize-data")]
pub mod resize_data;

use std::path::Path;

use crate::{Result, bootloader::BootEnvState, partition::PartitionLayout, runtime::OdsStatus};

/// Context passed to each init setup step.
#[non_exhaustive]
pub struct InitSetupCtx<'l, 'b, 's, 'r> {
    pub layout: &'l PartitionLayout,
    pub boot_env: &'b mut BootEnvState,
    pub ods_status: &'s mut OdsStatus,
    pub rootfs: &'r Path,
}

/// Run all enabled init setup steps in order.
///
/// Steps are independent and idempotent. Order is intentional: extra-bootargs
/// may reboot before resize-data touches any partition read-write.
pub fn run(mut ctx: InitSetupCtx<'_, '_, '_, '_>) -> Result<()> {
    extra_bootargs::run(&mut ctx)?;
    #[cfg(feature = "resize-data")]
    resize_data::run(&mut ctx)?;
    Ok(())
}
