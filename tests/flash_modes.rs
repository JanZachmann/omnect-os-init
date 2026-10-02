//! Integration tests for the flash modes, through the public API.

#![cfg(all(feature = "flash-mode", feature = "test-utils"))]

use omnect_os_init::MockBootEnv;
use omnect_os_init::bootloader::BootEnvKey;
use omnect_os_init::mode::{BootMode, EnforceFlag};

#[test]
fn a_boot_env_read_failure_falls_back_to_normal_boot() {
    let mut env = MockBootEnv::new()
        .with_env(BootEnvKey::FlashMode, "1")
        .with_get_env_error();
    assert!(
        matches!(
            BootMode::detect_with(Some(&mut env), EnforceFlag::Absent).unwrap(),
            BootMode::Normal
        ),
        "an unreadable env must not stop the device from booting"
    );
}

#[cfg(feature = "flash-mode-2")]
mod mode_2_detection {
    use omnect_os_init::MockBootEnv;
    use omnect_os_init::bootloader::{BootEnv, BootEnvKey};
    use omnect_os_init::mode::flash::config::FlashMode;
    use omnect_os_init::mode::{BootMode, EnforceFlag};

    fn detected_mode(bl: Option<&mut dyn BootEnv>, flag: EnforceFlag) -> Option<FlashMode> {
        match BootMode::detect_with(bl, flag).unwrap() {
            BootMode::Flash(config) => Some(config.mode),
            _ => None,
        }
    }

    #[cfg(feature = "flash-mode-1")]
    #[test]
    fn key_1_selects_mode_1_with_or_without_the_flag() {
        for flag in [EnforceFlag::Present, EnforceFlag::Absent] {
            let mut env = MockBootEnv::new()
                .with_env(BootEnvKey::FlashMode, "1")
                .with_env(BootEnvKey::FlashModeDevPath, "/dev/mmcblk2");
            assert_eq!(
                detected_mode(Some(&mut env), flag),
                Some(FlashMode::Mode1),
                "flag {flag:?}"
            );
        }

        // Key 1 keeps mode 1's rule: a conflict that cannot be ruled out
        // boots normally, even with the flag.
        #[cfg(feature = "factory-reset")]
        {
            let mut env = MockBootEnv::new()
                .with_env(BootEnvKey::FlashMode, "1")
                .with_env(BootEnvKey::FlashModeDevPath, "/dev/mmcblk2")
                .with_get_env_error_for(BootEnvKey::FactoryReset);
            assert!(matches!(
                BootMode::detect_with(Some(&mut env), EnforceFlag::Present).unwrap(),
                BootMode::Normal
            ));
        }
    }

    #[test]
    fn the_flag_selects_mode_2_whatever_the_key_says() {
        for value in [Some("2"), Some("3"), Some("9"), Some(""), Some(" "), None] {
            let mut env = MockBootEnv::new();
            if let Some(value) = value {
                env = env.with_env(BootEnvKey::FlashMode, value);
            }
            assert_eq!(
                detected_mode(Some(&mut env), EnforceFlag::Present),
                Some(FlashMode::Mode2),
                "key {value:?}"
            );
        }
    }

    #[test]
    fn key_2_selects_mode_2_without_the_flag() {
        let mut env = MockBootEnv::new().with_env(BootEnvKey::FlashMode, "2");
        assert_eq!(
            detected_mode(Some(&mut env), EnforceFlag::Absent),
            Some(FlashMode::Mode2)
        );
        // The success path clears nothing: clearing is the mode's own first step.
        assert!(env.set_env_calls.is_empty());
    }

    #[test]
    fn without_the_flag_an_unknown_key_boots_normally() {
        let mut env = MockBootEnv::new().with_env(BootEnvKey::FlashMode, "3");
        assert_eq!(detected_mode(Some(&mut env), EnforceFlag::Absent), None);
    }

    #[cfg(feature = "factory-reset")]
    #[test]
    fn the_flag_or_key_2_with_a_factory_reset_clears_both_and_is_refused() {
        use omnect_os_init::error::{FlashError, InitramfsError};

        for (key, flag) in [
            (Some("2"), EnforceFlag::Absent),
            (None, EnforceFlag::Present),
            (Some("2"), EnforceFlag::Present),
        ] {
            let mut env = MockBootEnv::new()
                .with_env(BootEnvKey::FactoryReset, r#"{"mode":1,"preserve":[]}"#);
            if let Some(key) = key {
                env = env.with_env(BootEnvKey::FlashMode, key);
            }
            let err = BootMode::detect_with(Some(&mut env), flag).unwrap_err();
            assert!(
                matches!(err, InitramfsError::Flash(FlashError::ConflictingTriggers)),
                "key {key:?}, flag {flag:?}: {err}"
            );
            assert!(env.set_env_calls.contains(&BootEnvKey::FlashMode));
            assert!(env.set_env_calls.contains(&BootEnvKey::FactoryReset));
            assert_eq!(env.get_env(BootEnvKey::FactoryReset).unwrap(), None);
        }
    }

    #[test]
    fn the_flag_selects_mode_2_when_the_boot_env_is_unavailable() {
        assert_eq!(
            detected_mode(None, EnforceFlag::Present),
            Some(FlashMode::Mode2)
        );
        assert_eq!(detected_mode(None, EnforceFlag::Absent), None);
    }

    #[test]
    fn the_flag_selects_mode_2_when_the_key_cannot_be_read() {
        let mut env = MockBootEnv::new().with_get_env_error();
        assert_eq!(
            detected_mode(Some(&mut env), EnforceFlag::Present),
            Some(FlashMode::Mode2)
        );

        let mut env = MockBootEnv::new()
            .with_env(BootEnvKey::FlashMode, "2")
            .with_get_env_error();
        assert_eq!(detected_mode(Some(&mut env), EnforceFlag::Absent), None);
    }

    #[cfg(feature = "factory-reset")]
    #[test]
    fn the_flag_selects_mode_2_when_the_factory_reset_key_cannot_be_read() {
        let mut env = MockBootEnv::new().with_get_env_error_for(BootEnvKey::FactoryReset);
        assert_eq!(
            detected_mode(Some(&mut env), EnforceFlag::Present),
            Some(FlashMode::Mode2)
        );
        assert!(env.set_env_calls.is_empty());

        // Without the flag a conflict cannot be ruled out, so mode 2 does not run.
        let mut env = MockBootEnv::new()
            .with_env(BootEnvKey::FlashMode, "2")
            .with_get_env_error_for(BootEnvKey::FactoryReset);
        assert_eq!(detected_mode(Some(&mut env), EnforceFlag::Absent), None);
    }
}
