use std::path::Path;

use crate::{
    BootEnv, BootEnvState, Result, config::Config, partition::PartitionLayout, runtime::OdsStatus,
};

#[cfg(any(feature = "factory-reset", feature = "flash-mode"))]
use crate::bootloader::BootEnvKey;

pub mod normal;

#[cfg(feature = "factory-reset")]
pub mod factory_reset;

#[cfg(feature = "flash-mode")]
pub mod flash;

/// An image recipe adds the flag to the initramfs, so it is checked at run time.
#[cfg(feature = "flash-mode-2")]
const ENFORCE_FLASH_MODE_FLAG: &str = "etc/enforce_flash_mode";

#[cfg(feature = "flash-mode-2")]
const INITRAMFS_ROOT: &str = "/";

/// Runtime context passed to the active boot-mode handler.
pub struct BootContext<'a> {
    pub(crate) config: &'a Config,
    pub(crate) layout: &'a PartitionLayout,
    pub(crate) rootfs: &'a Path,
    pub(crate) boot_env: BootEnvState,
    pub(crate) ods_status: OdsStatus,
}

impl<'a> BootContext<'a> {
    pub(crate) fn new(
        config: &'a Config,
        layout: &'a PartitionLayout,
        rootfs: &'a Path,
        boot_env: BootEnvState,
        ods_status: OdsStatus,
    ) -> Self {
        Self {
            config,
            layout,
            rootfs,
            boot_env,
            ods_status,
        }
    }
}

/// A factory-reset trigger that was set in the boot environment.
///
/// A trigger the init cannot use still has to be answered: it is cleared and
/// its failure is reported, so the caller learns the reset did not run
/// instead of waiting for a result that never arrives.
#[cfg(feature = "factory-reset")]
#[derive(Debug)]
pub enum FactoryResetTrigger {
    Accepted(factory_reset::config::FactoryResetConfig),
    Rejected(crate::error::FactoryResetError),
}

/// The detected boot mode to execute.
#[derive(Debug)]
pub enum BootMode {
    Normal,
    #[cfg(feature = "factory-reset")]
    FactoryReset(FactoryResetTrigger),
    #[cfg(feature = "flash-mode")]
    Flash(flash::config::FlashConfig),
}

/// Best-effort clear of the flash trigger keys.
///
/// On a release image a fatal error halts forever, so a trigger left set would
/// mean every power cycle repeats the same outcome.
#[cfg(feature = "flash-mode")]
pub(crate) fn clear_flash_triggers(bl: &mut dyn BootEnv) {
    if let Err(e) = bl.set_env(BootEnvKey::FlashMode, None) {
        log::warn!("flash-mode: failed to clear the flash-mode trigger: {e}");
    }
    #[cfg(feature = "flash-mode-1")]
    if let Err(e) = bl.set_env(BootEnvKey::FlashModeDevPath, None) {
        log::warn!("flash-mode: failed to clear the flash-mode-devpath trigger: {e}");
    }
}

#[cfg(all(feature = "flash-mode", feature = "factory-reset"))]
fn clear_flash_and_reset_triggers(bl: &mut dyn BootEnv) {
    clear_flash_triggers(bl);
    if let Err(e) = bl.set_env(BootEnvKey::FactoryReset, None) {
        log::warn!("factory-reset: failed to clear the factory-reset trigger: {e}");
    }
}

/// Whether the running initramfs carries the enforce flag file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforceFlag {
    Absent,
    Present,
}

#[cfg(feature = "flash-mode-2")]
fn enforce_flag(root: &Path) -> EnforceFlag {
    if root.join(ENFORCE_FLASH_MODE_FLAG).is_file() {
        EnforceFlag::Present
    } else {
        EnforceFlag::Absent
    }
}

#[cfg(feature = "flash-mode-2")]
pub(crate) fn mode_2_config() -> flash::config::FlashConfig {
    flash::config::FlashConfig {
        mode: flash::config::FlashMode::Mode2,
        #[cfg(feature = "flash-mode-1")]
        devpath: flash::config::Devpath::NotSet,
    }
}

/// Build the config for a recognised flash mode.
///
/// An unusable devpath is carried in the config rather than raised: detection
/// runs before the mode starts its log capture, and the mode is where the
/// reason has to reach the persisted log.
#[cfg(feature = "flash-mode")]
fn build_flash_config(
    #[cfg_attr(not(feature = "flash-mode-1"), allow(unused_variables))] bl: &mut dyn BootEnv,
    mode: flash::config::FlashMode,
) -> flash::config::FlashConfig {
    match mode {
        #[cfg(feature = "flash-mode-1")]
        flash::config::FlashMode::Mode1 => {
            let devpath = match bl.get_env(BootEnvKey::FlashModeDevPath) {
                Ok(value) => flash::config::parse_devpath(value.as_deref()),
                Err(e) => flash::config::Devpath::Unreadable(e.to_string()),
            };
            flash::config::FlashConfig { mode, devpath }
        }
        #[cfg(feature = "flash-mode-2")]
        flash::config::FlashMode::Mode2 => mode_2_config(),
    }
}

impl BootMode {
    /// Detect the boot mode from the boot environment and, with flash mode 2
    /// built in, the enforce flag file in the running initramfs.
    pub fn detect(bl: Option<&mut dyn BootEnv>) -> Result<Self> {
        #[cfg(feature = "flash-mode-2")]
        let flag = enforce_flag(Path::new(INITRAMFS_ROOT));
        #[cfg(not(feature = "flash-mode-2"))]
        let flag = EnforceFlag::Absent;
        Self::detect_with(bl, flag)
    }

    /// Detect the boot mode, with the enforce flag state given.
    ///
    /// A set `flash-mode` selects `Flash`; a `factory-reset` with a non-blank
    /// value selects `FactoryReset`. A blank value of either key is no trigger.
    /// The enforce flag selects mode 2 unless `flash-mode` selects mode 1.
    /// Both triggers at once is refused — they act on different disks and
    /// single-mode dispatch cannot perform both, so dropping one silently would
    /// be the worse failure. The refusal is the only error, and it is fatal.
    ///
    /// A `flash-mode` value that selects nothing is logged and the device boots
    /// normally: an operator typo must not stop a device from booting.
    /// Falls back to `Normal` when either trigger key cannot be read while a
    /// flash mode may be set, since a conflict cannot be ruled out and both
    /// modes are destructive. The enforce flag is the exception: it selects
    /// mode 2 even when the environment cannot be read.
    #[cfg_attr(
        not(any(feature = "factory-reset", feature = "flash-mode")),
        allow(unused_variables)
    )]
    pub fn detect_with(
        bl: Option<&mut dyn BootEnv>,
        #[cfg_attr(not(feature = "flash-mode-2"), allow(unused_variables))] flag: EnforceFlag,
    ) -> Result<Self> {
        #[cfg(feature = "flash-mode-2")]
        let flag_present = flag == EnforceFlag::Present;
        let Some(bl) = bl else {
            #[cfg(feature = "flash-mode-2")]
            if flag_present {
                return Ok(Self::Flash(mode_2_config()));
            }
            return Ok(Self::Normal);
        };

        #[cfg(feature = "flash-mode")]
        {
            let value = match bl.get_env(BootEnvKey::FlashMode) {
                // Blank is no trigger, for the same backend reason as the
                // factory-reset key below.
                Ok(value) => value.filter(|value| !value.trim().is_empty()),
                Err(e) => {
                    #[cfg(feature = "flash-mode-2")]
                    if flag_present {
                        log::warn!(
                            "flash-mode: failed to read env, the enforce flag selects mode 2: {e}"
                        );
                        return Ok(Self::Flash(mode_2_config()));
                    }
                    log::warn!("flash-mode: failed to read env, booting normally: {e}");
                    return Ok(Self::Normal);
                }
            };

            let parsed = value.as_deref().and_then(flash::config::parse_mode);
            #[cfg(feature = "flash-mode-2")]
            let parsed = match parsed {
                #[cfg(feature = "flash-mode-1")]
                Some(flash::config::FlashMode::Mode1) => parsed,
                _ if flag_present => Some(flash::config::FlashMode::Mode2),
                _ => parsed,
            };

            match (parsed, value) {
                (Some(mode), _) => {
                    #[cfg(feature = "factory-reset")]
                    match bl.get_env(BootEnvKey::FactoryReset) {
                        Ok(Some(json)) if !json.trim().is_empty() => {
                            clear_flash_and_reset_triggers(bl);
                            return Err(crate::error::FlashError::ConflictingTriggers.into());
                        }
                        Ok(Some(_)) | Ok(None) => {}
                        Err(e) => {
                            #[cfg(feature = "flash-mode-2")]
                            if flag_present && mode == flash::config::FlashMode::Mode2 {
                                log::warn!(
                                    "factory-reset: failed to read env while checking for a flash conflict, the enforce flag selects mode 2: {e}"
                                );
                                return Ok(Self::Flash(mode_2_config()));
                            }
                            log::warn!(
                                "factory-reset: failed to read env while checking for a flash conflict, booting normally: {e}"
                            );
                            return Ok(Self::Normal);
                        }
                    }

                    return Ok(Self::Flash(build_flash_config(bl, mode)));
                }
                (None, Some(value)) => {
                    log::warn!("flash-mode: unrecognised value '{value}', booting normally");
                }
                (None, None) => {}
            }
        }

        #[cfg(feature = "factory-reset")]
        match bl.get_env(BootEnvKey::FactoryReset) {
            // A trigger can be cleared by unsetting the key or by writing
            // an empty value. The backends disagree about the second:
            // fw_printenv reports an empty variable as unset, grub-editenv
            // still lists the key. Treat blank as no trigger so both behave
            // the same.
            Ok(Some(json)) if json.trim().is_empty() => {}
            Ok(Some(json)) => match factory_reset::config::FactoryResetConfig::parse(&json) {
                Ok(config) => {
                    return Ok(Self::FactoryReset(FactoryResetTrigger::Accepted(config)));
                }
                Err(e) => {
                    log::warn!("factory-reset: unusable trigger, reporting it: {e}");
                    return Ok(Self::FactoryReset(FactoryResetTrigger::Rejected(e)));
                }
            },
            Ok(None) => {}
            Err(e) => {
                log::warn!("factory-reset: failed to read env, booting normally: {e}");
            }
        }

        Ok(Self::Normal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::create_mock_bootloader;

    #[test]
    fn detect_normal_with_live_bootloader() {
        let mut mock = create_mock_bootloader();
        let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
        assert!(matches!(mode, BootMode::Normal));
    }

    #[test]
    fn detect_normal_degraded_boot_no_bootloader() {
        let mode = BootMode::detect_with(None, EnforceFlag::Absent).unwrap();
        assert!(matches!(mode, BootMode::Normal));
    }

    #[cfg(feature = "factory-reset")]
    mod factory_reset_detect_tests {
        use super::*;
        use crate::bootloader::BootEnvKey;

        #[test]
        fn detect_normal_when_factory_reset_key_absent() {
            let mut mock = create_mock_bootloader();
            let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }

        #[test]
        fn detect_factory_reset_when_key_present_valid_json() {
            let mut mock = create_mock_bootloader()
                .with_env(BootEnvKey::FactoryReset, r#"{"mode":1,"preserve":[]}"#);
            let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
            let BootMode::FactoryReset(FactoryResetTrigger::Accepted(config)) = mode else {
                panic!("a usable trigger must be accepted");
            };
            assert_eq!(
                config.mode,
                crate::mode::factory_reset::config::ResetMode::Mode1
            );
            assert!(config.preserve.is_empty());
        }

        #[test]
        fn detect_rejects_an_unusable_trigger_instead_of_ignoring_it() {
            for trigger in [
                "not-json",
                r#"{ "mode": "#,
                "{}",
                r#"{"mode":4,"preserve":[]}"#,
                r#"{"mode":1}"#,
                r#"{"mode":1,"preserve":""}"#,
            ] {
                let mut mock = create_mock_bootloader().with_env(BootEnvKey::FactoryReset, trigger);
                let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
                assert!(
                    matches!(
                        mode,
                        BootMode::FactoryReset(FactoryResetTrigger::Rejected(_))
                    ),
                    "trigger {trigger} must be rejected, not ignored"
                );
            }
        }

        #[test]
        fn detect_normal_when_the_trigger_is_blank() {
            for trigger in ["", " ", "\n"] {
                let mut mock = create_mock_bootloader().with_env(BootEnvKey::FactoryReset, trigger);
                let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
                assert!(
                    matches!(mode, BootMode::Normal),
                    "a blank trigger must not start a reset: {trigger:?}"
                );
            }
        }

        #[test]
        fn detect_normal_when_bootloader_unavailable() {
            let mode = BootMode::detect_with(None, EnforceFlag::Absent).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }

        #[test]
        fn detect_normal_when_get_env_fails() {
            let mut mock = create_mock_bootloader().with_get_env_error();
            let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }
    }

    #[cfg(feature = "flash-mode-2")]
    mod enforce_flag_tests {
        use super::*;

        #[test]
        fn the_enforce_flag_is_a_file_under_etc() {
            let root = tempfile::tempdir().unwrap();
            assert_eq!(enforce_flag(root.path()), EnforceFlag::Absent);

            let flag = root.path().join(ENFORCE_FLASH_MODE_FLAG);
            std::fs::create_dir_all(flag.parent().unwrap()).unwrap();
            std::fs::create_dir(&flag).unwrap();
            assert_eq!(
                enforce_flag(root.path()),
                EnforceFlag::Absent,
                "a directory is not the flag"
            );

            std::fs::remove_dir(&flag).unwrap();
            std::fs::write(&flag, "").unwrap();
            assert_eq!(enforce_flag(root.path()), EnforceFlag::Present);
        }
    }

    #[cfg(all(
        feature = "flash-mode",
        any(feature = "flash-mode-1", feature = "factory-reset")
    ))]
    mod flash_detect_tests {
        use super::*;
        use crate::bootloader::BootEnvKey;

        #[cfg(feature = "flash-mode-1")]
        #[test]
        fn detect_flash_mode_1_with_a_destination() {
            let mut mock = create_mock_bootloader()
                .with_env(BootEnvKey::FlashMode, "1")
                .with_env(BootEnvKey::FlashModeDevPath, "/dev/mmcblk2");
            let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
            let BootMode::Flash(config) = mode else {
                panic!("a set flash-mode must select the flash mode");
            };
            assert_eq!(config.mode, crate::mode::flash::config::FlashMode::Mode1);
            assert_eq!(
                config.devpath,
                crate::mode::flash::config::Devpath::Set("/dev/mmcblk2".into())
            );
            // The success path clears nothing: clearing is the mode's own first step.
            assert!(mock.set_env_calls.is_empty());
        }

        #[cfg(feature = "factory-reset")]
        #[test]
        fn detect_falls_through_to_factory_reset_for_an_unknown_flash_mode_value() {
            for unknown in ["", "0", "9", "yes"] {
                let mut mock = create_mock_bootloader()
                    .with_env(BootEnvKey::FlashMode, unknown)
                    .with_env(BootEnvKey::FactoryReset, r#"{"mode":1,"preserve":[]}"#);
                let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
                assert!(
                    matches!(
                        mode,
                        BootMode::FactoryReset(FactoryResetTrigger::Accepted(_))
                    ),
                    "{unknown} must not short-circuit; it must reach the factory-reset handling"
                );
            }
        }

        #[cfg(all(feature = "flash-mode-1", feature = "factory-reset"))]
        #[test]
        fn detect_refuses_a_flash_mode_queued_together_with_a_factory_reset() {
            let mut mock = create_mock_bootloader()
                .with_env(BootEnvKey::FlashMode, "1")
                .with_env(BootEnvKey::FlashModeDevPath, "/dev/mmcblk2")
                .with_env(BootEnvKey::FactoryReset, r#"{"mode":1,"preserve":[]}"#);
            let err = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap_err();
            assert!(
                matches!(
                    err,
                    crate::error::InitramfsError::Flash(
                        crate::error::FlashError::ConflictingTriggers
                    )
                ),
                "the pair must be refused, not silently resolved: {err}"
            );
            assert!(mock.set_env_calls.contains(&BootEnvKey::FlashMode));
            assert!(mock.set_env_calls.contains(&BootEnvKey::FlashModeDevPath));
            assert!(mock.set_env_calls.contains(&BootEnvKey::FactoryReset));
            assert_eq!(mock.get_env(BootEnvKey::FlashMode).unwrap(), None);
            assert_eq!(mock.get_env(BootEnvKey::FactoryReset).unwrap(), None);
        }

        #[cfg(all(feature = "flash-mode-1", feature = "factory-reset"))]
        #[test]
        fn detect_flashes_when_the_queued_factory_reset_is_blank() {
            for blank in ["", " ", "\n"] {
                let mut mock = create_mock_bootloader()
                    .with_env(BootEnvKey::FlashMode, "1")
                    .with_env(BootEnvKey::FlashModeDevPath, "/dev/mmcblk2")
                    .with_env(BootEnvKey::FactoryReset, blank);
                let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
                assert!(
                    matches!(mode, BootMode::Flash(_)),
                    "a blank reset key is no trigger, so there is nothing to conflict with: {blank:?}"
                );
            }
        }

        #[cfg(all(feature = "flash-mode-1", feature = "factory-reset"))]
        #[test]
        fn detect_normal_when_the_conflict_check_cannot_read_the_reset_key() {
            // The flash-mode read succeeds, the factory-reset read fails: a
            // conflict cannot be ruled out, so neither mode runs.
            let mut mock = create_mock_bootloader()
                .with_env(BootEnvKey::FlashMode, "1")
                .with_env(BootEnvKey::FlashModeDevPath, "/dev/mmcblk2")
                .with_get_env_error_after(1);
            let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
            assert!(matches!(mode, BootMode::Normal));
            assert!(mock.set_env_calls.is_empty());
        }

        #[cfg(feature = "factory-reset")]
        #[test]
        fn detect_normal_when_the_flash_mode_key_cannot_be_read() {
            // The reset key reads fine, but a flash mode set next to it cannot
            // be ruled out, so the reset must not run either.
            let mut mock = create_mock_bootloader()
                .with_env(BootEnvKey::FactoryReset, r#"{"mode":1,"preserve":[]}"#)
                .with_get_env_error_for(BootEnvKey::FlashMode);
            let mode = BootMode::detect_with(Some(&mut mock), EnforceFlag::Absent).unwrap();
            assert!(matches!(mode, BootMode::Normal));
            assert!(mock.set_env_calls.is_empty());
        }

        #[cfg(feature = "flash-mode-1")]
        #[test]
        fn detect_carries_the_reason_an_unusable_destination_was_dropped() {
            use crate::mode::flash::config::Devpath;

            let mut unset = create_mock_bootloader().with_env(BootEnvKey::FlashMode, "1");
            let Ok(BootMode::Flash(config)) =
                BootMode::detect_with(Some(&mut unset), EnforceFlag::Absent)
            else {
                panic!("an unset destination must still select the flash mode");
            };
            assert_eq!(config.devpath, Devpath::NotSet);

            let mut unreadable = create_mock_bootloader()
                .with_env(BootEnvKey::FlashMode, "1")
                .with_get_env_error_for(BootEnvKey::FlashModeDevPath);
            let Ok(BootMode::Flash(config)) =
                BootMode::detect_with(Some(&mut unreadable), EnforceFlag::Absent)
            else {
                panic!("an unreadable destination must still select the flash mode");
            };
            assert!(matches!(config.devpath, Devpath::Unreadable(_)));
        }
    }
}
