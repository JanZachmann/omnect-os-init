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
    Rejected(crate::error::InitramfsError),
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
/// Used by the conflict refusal and as the flash mode's own first step. On a
/// release image a fatal error halts forever, so a trigger left set would mean
/// every power cycle repeats the same outcome.
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

/// Best-effort clear of both the flash and the factory-reset triggers ahead
/// of the conflict refusal, so a re-queue starts from a clean slate.
#[cfg(all(feature = "flash-mode", feature = "factory-reset"))]
fn clear_flash_and_reset_triggers(bl: &mut dyn BootEnv) {
    clear_flash_triggers(bl);
    if let Err(e) = bl.set_env(BootEnvKey::FactoryReset, None) {
        log::warn!("factory-reset: failed to clear the factory-reset trigger: {e}");
    }
}

/// Build the config for a recognised flash mode.
///
/// The devpath is read best-effort: an unusable value is not fatal here
/// because validating it is the mode's own job, not detection's.
#[cfg(feature = "flash-mode")]
fn build_flash_config(
    _bl: &mut dyn BootEnv,
    mode: flash::config::FlashMode,
) -> flash::config::FlashConfig {
    #[cfg(feature = "flash-mode-1")]
    let devpath = match _bl.get_env(BootEnvKey::FlashModeDevPath) {
        Ok(value) => match flash::config::parse_devpath(value.as_deref()) {
            Ok(path) => Some(path),
            Err(e) => {
                log::warn!(
                    "flash-mode-devpath: unusable value, continuing without a destination: {e}"
                );
                None
            }
        },
        Err(e) => {
            log::warn!(
                "flash-mode-devpath: failed to read env, continuing without a destination: {e}"
            );
            None
        }
    };

    flash::config::FlashConfig {
        mode,
        #[cfg(feature = "flash-mode-1")]
        devpath,
    }
}

impl BootMode {
    /// Detect the boot mode from the boot environment.
    ///
    /// A set `flash-mode` selects `Flash`; a set `factory-reset` selects
    /// `FactoryReset`, whether or not its value can be used — an unusable one
    /// is carried as `FactoryResetTrigger::Rejected`, so it is cleared and
    /// reported.
    /// Both triggers at once is refused — they act on different disks and
    /// single-mode dispatch cannot perform both, so dropping one silently would
    /// be the worse failure. Both triggers are cleared before that refusal is
    /// raised, because a release image halts on a fatal error and would
    /// otherwise repeat the refusal on every power cycle.
    ///
    /// A `flash-mode` value that selects nothing is logged and the device boots
    /// normally: an operator typo must not stop a device from booting.
    /// Falls back to `Normal` when an env read fails, since a conflict cannot
    /// be ruled out and the flash is the destructive, irreversible side. Never
    /// blocks boot.
    pub fn detect(_bl: Option<&mut dyn BootEnv>) -> Result<Self> {
        if let Some(_bl) = _bl {
            #[cfg(feature = "flash-mode")]
            match _bl.get_env(BootEnvKey::FlashMode) {
                Ok(Some(value)) => match flash::config::parse_mode(&value) {
                    Some(mode) => {
                        #[cfg(feature = "factory-reset")]
                        match _bl.get_env(BootEnvKey::FactoryReset) {
                            Ok(Some(_)) => {
                                clear_flash_and_reset_triggers(_bl);
                                return Err(crate::error::FlashError::ConflictingTriggers.into());
                            }
                            Ok(None) => {}
                            Err(e) => {
                                log::warn!(
                                    "factory-reset: failed to read env while checking for a flash conflict, booting normally: {e}"
                                );
                                return Ok(Self::Normal);
                            }
                        }

                        return Ok(Self::Flash(build_flash_config(_bl, mode)));
                    }
                    None => {
                        log::warn!("flash-mode: unrecognised value '{value}', booting normally");
                    }
                },
                Ok(None) => {}
                Err(e) => {
                    log::warn!("flash-mode: failed to read env, booting normally: {e}");
                }
            }

            #[cfg(feature = "factory-reset")]
            match _bl.get_env(BootEnvKey::FactoryReset) {
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
        let mode = BootMode::detect(Some(&mut mock)).unwrap();
        assert!(matches!(mode, BootMode::Normal));
    }

    #[test]
    fn detect_normal_degraded_boot_no_bootloader() {
        let mode = BootMode::detect(None).unwrap();
        assert!(matches!(mode, BootMode::Normal));
    }

    #[cfg(feature = "factory-reset")]
    mod factory_reset_detect_tests {
        use super::*;
        use crate::bootloader::BootEnvKey;

        #[test]
        fn detect_normal_when_factory_reset_key_absent() {
            let mut mock = create_mock_bootloader();
            let mode = BootMode::detect(Some(&mut mock)).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }

        #[test]
        fn detect_factory_reset_when_key_present_valid_json() {
            let mut mock = create_mock_bootloader()
                .with_env(BootEnvKey::FactoryReset, r#"{"mode":1,"preserve":[]}"#);
            let mode = BootMode::detect(Some(&mut mock)).unwrap();
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
                let mode = BootMode::detect(Some(&mut mock)).unwrap();
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
        fn detect_normal_when_bootloader_unavailable() {
            let mode = BootMode::detect(None).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }

        #[test]
        fn detect_normal_when_get_env_fails() {
            let mut mock = create_mock_bootloader().with_get_env_error();
            let mode = BootMode::detect(Some(&mut mock)).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }
    }

    #[cfg(feature = "flash-mode")]
    mod flash_detect_tests {
        use super::*;
        use crate::bootloader::BootEnvKey;

        #[cfg(feature = "flash-mode-1")]
        #[test]
        fn detect_flash_mode_1_with_a_destination() {
            let mut mock = create_mock_bootloader()
                .with_env(BootEnvKey::FlashMode, "1")
                .with_env(BootEnvKey::FlashModeDevPath, "/dev/mmcblk2");
            let mode = BootMode::detect(Some(&mut mock)).unwrap();
            let BootMode::Flash(config) = mode else {
                panic!("a set flash-mode must select the flash mode");
            };
            assert_eq!(config.mode, crate::mode::flash::config::FlashMode::Mode1);
            assert_eq!(
                config.devpath.as_deref(),
                Some(std::path::Path::new("/dev/mmcblk2"))
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
                let mode = BootMode::detect(Some(&mut mock)).unwrap();
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
            let err = BootMode::detect(Some(&mut mock)).unwrap_err();
            assert!(
                matches!(
                    err,
                    crate::error::InitramfsError::Flash(
                        crate::error::FlashError::ConflictingTriggers
                    )
                ),
                "the pair must be refused, not silently resolved: {err}"
            );
            // All three triggers must be gone, or a release image halts on every power cycle.
            assert!(mock.set_env_calls.contains(&BootEnvKey::FlashMode));
            assert!(mock.set_env_calls.contains(&BootEnvKey::FlashModeDevPath));
            assert!(mock.set_env_calls.contains(&BootEnvKey::FactoryReset));
            assert_eq!(mock.get_env(BootEnvKey::FlashMode).unwrap(), None);
            assert_eq!(mock.get_env(BootEnvKey::FactoryReset).unwrap(), None);
        }

        #[test]
        fn detect_normal_when_the_flash_mode_key_cannot_be_read() {
            let mut mock = create_mock_bootloader().with_get_env_error();
            let mode = BootMode::detect(Some(&mut mock)).unwrap();
            assert!(matches!(mode, BootMode::Normal));
        }
    }
}
