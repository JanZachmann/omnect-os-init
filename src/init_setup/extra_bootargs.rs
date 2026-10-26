//! Init setup step: sync `omnect_extra_bootargs` to the bootloader env.
//!
//! On the fresh-flash boot only, this reads the boot-partition argument files
//! and, if they differ from the stored value, writes the value, verifies it,
//! flushes it to disk, and requests a reboot so the bootloader applies the
//! arguments from the next boot.

use std::{io::ErrorKind, path::Path};

use crate::Result;
use crate::bootloader::BootEnvKey;
use crate::error::InitramfsError;
use crate::filesystem::mount_points;
use crate::init_setup::InitSetupCtx;
use crate::runtime::{ExtraBootArgsOutcome, ExtraBootArgsStatus};

/// Boot-partition file for distro-managed extra boot arguments.
const BOOTARGS_OMNECT_FILE: &str = "omnect_extra_bootargs_omnect";
/// Boot-partition file for user-managed extra boot arguments.
const BOOTARGS_CUSTOM_FILE: &str = "omnect_extra_bootargs_custom";

/// Build the combined bootargs value from the two boot-partition files: the
/// distro file plus the optional custom file. Whitespace runs are squeezed to
/// single spaces to normalize irregular whitespace in hand-edited files, so the
/// built value is stable across boots.
fn read_extra_bootargs(boot_dir: &Path) -> std::io::Result<String> {
    let omnect = read_bootargs_file(&boot_dir.join(BOOTARGS_OMNECT_FILE))?;
    let custom = read_bootargs_file(&boot_dir.join(BOOTARGS_CUSTOM_FILE))?;
    let combined = match (omnect.as_deref(), custom.as_deref()) {
        (Some(a), Some(b)) => format!("{a} {b}"),
        (Some(a), None) => a.to_string(),
        (None, Some(b)) => b.to_string(),
        (None, None) => String::new(),
    };
    Ok(combined.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Read one bootargs file, trimmed. `Ok(None)` if empty or absent. A real read
/// error (not NotFound) propagates: the args are security-relevant, so the
/// caller must treat it as a failure, not as "no args".
fn read_bootargs_file(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let trimmed = s.trim().to_string();
            Ok(if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            })
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Sync `omnect_extra_bootargs` on the fresh-flash boot, then request a reboot.
///
/// Returns `Err(InitramfsError::ExtraBootArgsUpdated)` when the value changed
/// and was written, verified and flushed — the caller reboots. Every other
/// path returns `Ok(())`: the step is best-effort and never blocks boot. ODS
/// status is set only on failure (`None` otherwise).
pub fn run(ctx: &mut InitSetupCtx<'_, '_, '_, '_>) -> Result<()> {
    if !ctx.ods_status.first_boot {
        log::debug!("extra-bootargs: skipping (not first boot)");
        return Ok(());
    }

    let boot_dir = ctx.rootfs.join(mount_points::BOOT);
    let new_args = match read_extra_bootargs(&boot_dir) {
        Ok(args) => args,
        Err(e) => {
            log::warn!("extra-bootargs: reading bootargs files failed: {e}");
            ctx.ods_status
                .set_extra_bootargs_status(ExtraBootArgsStatus {
                    outcome: ExtraBootArgsOutcome::FileReadFailed,
                    reason: format!("reading bootargs files failed: {e}"),
                });
            return Ok(());
        }
    };

    // `first_boot == true` implies the env is available (`detect_first_boot`
    // returns false on a degraded env), so this None arm cannot occur in
    // production. Kept as a defensive no-op rather than an unwrap.
    let bl = match ctx.boot_env.available_mut() {
        Some(bl) => bl,
        None => {
            log::warn!("extra-bootargs: unexpected degraded env despite first_boot; skipping");
            return Ok(());
        }
    };

    let current = match bl.get_env(BootEnvKey::ExtraBootArgs) {
        Ok(v) => v.unwrap_or_default(),
        Err(e) => {
            log::warn!("extra-bootargs: read current value failed: {e}");
            ctx.ods_status
                .set_extra_bootargs_status(ExtraBootArgsStatus {
                    outcome: ExtraBootArgsOutcome::ReadFailed,
                    reason: format!("read current value failed: {e}"),
                });
            return Ok(());
        }
    };

    if current == new_args {
        log::debug!("extra-bootargs: already up to date");
        return Ok(());
    }

    let value = if new_args.is_empty() {
        None
    } else {
        Some(new_args.as_str())
    };
    if let Err(e) = bl.set_env(BootEnvKey::ExtraBootArgs, value) {
        log::warn!("extra-bootargs: set_env failed: {e}");
        ctx.ods_status
            .set_extra_bootargs_status(ExtraBootArgsStatus {
                outcome: ExtraBootArgsOutcome::SetEnvFailed,
                reason: format!("set_env failed: {e}"),
            });
        return Ok(());
    }

    // Read-back verify: if the stored value reads back different from what was
    // written, rebooting could never converge (current never equals new_args).
    // The value is not rolled back — a mismatch is a genuine write fault.
    let readback = match bl.get_env(BootEnvKey::ExtraBootArgs) {
        Ok(v) => v.unwrap_or_default(),
        Err(e) => {
            log::warn!("extra-bootargs: read-back failed: {e}; not rebooting");
            ctx.ods_status
                .set_extra_bootargs_status(ExtraBootArgsStatus {
                    outcome: ExtraBootArgsOutcome::ReadBackFailed,
                    reason: format!("read-back failed: {e}"),
                });
            return Ok(());
        }
    };
    if readback != new_args {
        log::warn!("extra-bootargs: read-back mismatch; not rebooting");
        ctx.ods_status
            .set_extra_bootargs_status(ExtraBootArgsStatus {
                outcome: ExtraBootArgsOutcome::ReadBackMismatch,
                reason: "read-back verify mismatch".to_string(),
            });
        return Ok(());
    }

    // Without this the write is lost across the reboot and the sync retries forever.
    crate::bootloader::sync_filesystems();

    // Success is not recorded in ODS: we reboot before normal::run writes the
    // JSON, and the next boot is a no-op.
    log::info!("extra-bootargs: applied {new_args:?}; rebooting to apply");
    Err(InitramfsError::ExtraBootArgsUpdated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::{BootEnvKey, BootEnvState, MockBootEnv};
    use crate::partition::{PartitionLayout, RootDevice};
    use crate::runtime::OdsStatus;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn empty_layout() -> PartitionLayout {
        PartitionLayout {
            partitions: HashMap::new(),
            device: RootDevice {
                base: PathBuf::from("/dev/sda"),
                partition_sep: "",
                root_partition: PathBuf::from("/dev/sda2"),
            },
        }
    }

    fn make_ctx<'l, 'b, 's, 'r>(
        layout: &'l PartitionLayout,
        env: &'b mut BootEnvState,
        ods: &'s mut OdsStatus,
        rootfs: &'r Path,
        first_boot: bool,
    ) -> InitSetupCtx<'l, 'b, 's, 'r> {
        ods.first_boot = first_boot;
        InitSetupCtx {
            layout,
            boot_env: env,
            ods_status: ods,
            rootfs,
        }
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    // ---- read_extra_bootargs -------------------------------------------

    #[test]
    fn no_files_yields_empty_string() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(read_extra_bootargs(tmp.path()).unwrap(), "");
    }

    #[test]
    fn omnect_only_returns_its_content() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), BOOTARGS_OMNECT_FILE, "quiet loglevel=3\n");
        assert_eq!(read_extra_bootargs(tmp.path()).unwrap(), "quiet loglevel=3");
    }

    #[test]
    fn both_files_are_joined_with_space() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), BOOTARGS_OMNECT_FILE, "quiet loglevel=3");
        write_file(tmp.path(), BOOTARGS_CUSTOM_FILE, "myarg=1");
        assert_eq!(
            read_extra_bootargs(tmp.path()).unwrap(),
            "quiet loglevel=3 myarg=1"
        );
    }

    #[test]
    fn empty_files_yield_empty_string() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), BOOTARGS_OMNECT_FILE, "   \n");
        write_file(tmp.path(), BOOTARGS_CUSTOM_FILE, "\n");
        assert_eq!(read_extra_bootargs(tmp.path()).unwrap(), "");
    }

    #[test]
    fn internal_whitespace_is_squeezed() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), BOOTARGS_OMNECT_FILE, "quiet    loglevel=3");
        write_file(tmp.path(), BOOTARGS_CUSTOM_FILE, "a\tb");
        assert_eq!(
            read_extra_bootargs(tmp.path()).unwrap(),
            "quiet loglevel=3 a b"
        );
    }

    // ---- run() ---------------------------------------------------------

    #[test]
    fn skips_when_not_first_boot() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        write_file(&boot_dir, BOOTARGS_OMNECT_FILE, "quiet loglevel=3");

        let layout = empty_layout();
        let mock = MockBootEnv::new();
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), false);
        assert!(run(&mut ctx).is_ok());
        assert!(ctx.ods_status.extra_bootargs.is_none());
        let bl = ctx.boot_env.available_mut().unwrap();
        assert_eq!(bl.get_env(BootEnvKey::ExtraBootArgs).unwrap(), None);
    }

    #[test]
    fn already_current_is_noop() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        write_file(&boot_dir, BOOTARGS_OMNECT_FILE, "quiet loglevel=3");

        let layout = empty_layout();
        let mock = MockBootEnv::new().with_env(BootEnvKey::ExtraBootArgs, "quiet loglevel=3");
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), true);
        assert!(run(&mut ctx).is_ok());
        assert!(
            ctx.ods_status.extra_bootargs.is_none(),
            "no-op must not record an ODS entry"
        );
    }

    #[test]
    fn changed_value_applies_and_requests_reboot() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        write_file(&boot_dir, BOOTARGS_OMNECT_FILE, "quiet loglevel=3");

        let layout = empty_layout();
        let mock = MockBootEnv::new();
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), true);
        let result = run(&mut ctx);
        assert!(matches!(result, Err(InitramfsError::ExtraBootArgsUpdated)));
        assert!(
            ctx.ods_status.extra_bootargs.is_none(),
            "success is not recorded in ODS (reboot before JSON write)"
        );
        let bl = ctx.boot_env.available_mut().unwrap();
        assert_eq!(
            bl.get_env(BootEnvKey::ExtraBootArgs).unwrap().as_deref(),
            Some("quiet loglevel=3")
        );
    }

    #[test]
    fn read_back_mismatch_records_failed_without_reboot() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        write_file(&boot_dir, BOOTARGS_OMNECT_FILE, "quiet loglevel=3");

        let layout = empty_layout();
        let mock = MockBootEnv::new().with_set_env_normalize("mangled");
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), true);
        let result = run(&mut ctx);
        assert!(
            result.is_ok(),
            "must not request reboot on read-back mismatch"
        );
        assert_eq!(
            ctx.ods_status.extra_bootargs.as_ref().unwrap().outcome,
            ExtraBootArgsOutcome::ReadBackMismatch
        );
    }

    #[test]
    fn file_read_error_records_failed_without_touching_env() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        // A directory where a bootargs file is expected makes read_to_string
        // fail with a non-NotFound error, standing in for flaky boot media.
        std::fs::create_dir(boot_dir.join(BOOTARGS_OMNECT_FILE)).unwrap();

        let layout = empty_layout();
        let mock = MockBootEnv::new().with_env(BootEnvKey::ExtraBootArgs, "quiet loglevel=3");
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), true);
        let result = run(&mut ctx);
        assert!(result.is_ok(), "read error must not request a reboot");
        assert_eq!(
            ctx.ods_status.extra_bootargs.as_ref().unwrap().outcome,
            ExtraBootArgsOutcome::FileReadFailed
        );
        // The stored value must not be deleted or changed on a read error.
        let bl = ctx.boot_env.available_mut().unwrap();
        assert_eq!(
            bl.get_env(BootEnvKey::ExtraBootArgs).unwrap().as_deref(),
            Some("quiet loglevel=3")
        );
    }

    #[test]
    fn read_extra_bootargs_propagates_read_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(BOOTARGS_OMNECT_FILE)).unwrap();
        assert!(read_extra_bootargs(tmp.path()).is_err());
    }

    #[test]
    fn custom_only_returns_its_content() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), BOOTARGS_CUSTOM_FILE, "myarg=1\n");
        assert_eq!(read_extra_bootargs(tmp.path()).unwrap(), "myarg=1");
    }

    #[test]
    fn get_env_error_records_read_failed_without_reboot() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        write_file(&boot_dir, BOOTARGS_OMNECT_FILE, "quiet loglevel=3");

        let layout = empty_layout();
        let mock = MockBootEnv::new().with_get_env_error();
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), true);
        let result = run(&mut ctx);
        assert!(result.is_ok(), "get_env failure must not block boot");
        assert_eq!(
            ctx.ods_status.extra_bootargs.as_ref().unwrap().outcome,
            ExtraBootArgsOutcome::ReadFailed
        );
    }

    #[test]
    fn empty_config_is_noop_without_reboot() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        // No bootargs files, env unset — the common fresh-flash case.

        let layout = empty_layout();
        let mock = MockBootEnv::new();
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), true);
        let result = run(&mut ctx);
        assert!(
            result.is_ok(),
            "empty config must not reboot on fresh flash"
        );
        assert!(ctx.ods_status.extra_bootargs.is_none());
        let bl = ctx.boot_env.available_mut().unwrap();
        assert_eq!(bl.get_env(BootEnvKey::ExtraBootArgs).unwrap(), None);
    }

    #[test]
    fn empty_args_delete_stale_value_and_request_reboot() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        // No bootargs files → new args empty, but the env holds a stale value.

        let layout = empty_layout();
        let mock = MockBootEnv::new().with_env(BootEnvKey::ExtraBootArgs, "stale=1");
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), true);
        let result = run(&mut ctx);
        assert!(matches!(result, Err(InitramfsError::ExtraBootArgsUpdated)));
        let bl = ctx.boot_env.available_mut().unwrap();
        assert_eq!(bl.get_env(BootEnvKey::ExtraBootArgs).unwrap(), None);
    }

    #[test]
    fn set_env_error_records_failed_without_reboot() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        write_file(&boot_dir, BOOTARGS_OMNECT_FILE, "quiet loglevel=3");

        let layout = empty_layout();
        let mock = MockBootEnv::new().with_set_env_error();
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), true);
        let result = run(&mut ctx);
        assert!(
            result.is_ok(),
            "transient set_env failure must not block boot"
        );
        assert_eq!(
            ctx.ods_status.extra_bootargs.as_ref().unwrap().outcome,
            ExtraBootArgsOutcome::SetEnvFailed
        );
    }

    #[test]
    fn read_back_error_records_read_back_failed_without_reboot() {
        let tmp = TempDir::new().unwrap();
        let boot_dir = tmp.path().join("boot");
        std::fs::create_dir_all(&boot_dir).unwrap();
        write_file(&boot_dir, BOOTARGS_OMNECT_FILE, "quiet loglevel=3");

        let layout = empty_layout();
        // First get_env (read current) succeeds; the read-back call fails.
        let mock = MockBootEnv::new().with_get_env_error_after(1);
        let mut env = BootEnvState::Available(Box::new(mock));
        let mut ods = OdsStatus::new();
        let mut ctx = make_ctx(&layout, &mut env, &mut ods, tmp.path(), true);
        let result = run(&mut ctx);
        assert!(result.is_ok(), "read-back failure must not reboot");
        assert_eq!(
            ctx.ods_status.extra_bootargs.as_ref().unwrap().outcome,
            ExtraBootArgsOutcome::ReadBackFailed
        );
    }
}
