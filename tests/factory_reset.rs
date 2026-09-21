//! Integration tests for factory-reset: ODS JSON contract and detect fallback.

#![cfg(feature = "factory-reset")]

use omnect_os_init::MockBootEnv;
use omnect_os_init::bootloader::BootEnvKey;
use omnect_os_init::mode::factory_reset::config::ResetMode;
use omnect_os_init::mode::{BootMode, FactoryResetTrigger};
use omnect_os_init::runtime::{FactoryResetStatus, FactoryResetStatusCode, OdsStatus};

#[test]
fn factory_reset_success_status_json() {
    let status = FactoryResetStatus {
        status: FactoryResetStatusCode::Success,
        error: None,
        context: None,
        paths: vec!["/etc/omnect/factory-reset.d/".into()],
        data_wiped: true,
    };
    let mut ods = OdsStatus::new();
    ods.set_factory_reset(status);
    let json: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&ods).unwrap()).unwrap();
    let fr = &json["factory_reset"];

    assert!(!fr.is_null(), "missing factory_reset key: {json}");
    assert_eq!(fr["status"], 0, "status must be integer 0: {json}");
    assert_eq!(fr["data_wiped"], true, "data_wiped must be true: {json}");
    assert_eq!(fr["paths"][0], "/etc/omnect/factory-reset.d/");
    assert!(
        fr.get("error").is_none(),
        "error must be skipped when None: {json}"
    );
    assert!(
        fr.get("context").is_none(),
        "context must be skipped when None: {json}"
    );
}

#[test]
fn factory_reset_error_status_json() {
    let status = FactoryResetStatus {
        status: FactoryResetStatusCode::Error,
        error: Some("mkfs retry exhausted".into()),
        context: Some("etc reformatted twice: initial remount failed".into()),
        paths: vec!["/etc/omnect/factory-reset.d/".into()],
        data_wiped: true,
    };
    let mut ods = OdsStatus::new();
    ods.set_factory_reset(status);
    let json = serde_json::to_string(&ods).unwrap();

    assert!(
        json.contains("\"status\":2"),
        "status must be integer 2: {json}"
    );
    assert!(json.contains("\"error\":"), "error missing: {json}");
    assert!(json.contains("\"context\":"), "context missing: {json}");
    assert!(
        json.contains("\"data_wiped\":true"),
        "data_wiped missing: {json}"
    );
}

#[test]
fn factory_reset_status_code_serializes_as_integer() {
    let status = FactoryResetStatus {
        status: FactoryResetStatusCode::Success,
        error: None,
        context: None,
        paths: vec![],
        data_wiped: false,
    };
    let mut ods = OdsStatus::new();
    ods.set_factory_reset(status);
    let json = serde_json::to_string(&ods).unwrap();

    assert!(
        json.contains("\"status\":0"),
        "status must serialize as bare integer, not a string: {json}"
    );
}

#[test]
fn factory_reset_warning_status_serializes_as_four() {
    let status = FactoryResetStatus {
        status: FactoryResetStatusCode::Warning,
        error: None,
        context: Some("etc reformatted twice".into()),
        paths: vec![],
        data_wiped: true,
    };
    let mut ods = OdsStatus::new();
    ods.set_factory_reset(status);
    let json: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&ods).unwrap()).unwrap();

    assert_eq!(
        json["factory_reset"]["status"], 4,
        "Warning must serialize as 4 (wire contract): {json}"
    );
}

#[test]
fn detect_reports_an_unsupported_mode_instead_of_booting_normally() {
    for mode_value in ["0", "4", "5"] {
        let trigger = format!(r#"{{"mode":{mode_value},"preserve":[]}}"#);
        let mock = MockBootEnv::new().with_env(BootEnvKey::FactoryReset, &trigger);
        let mode = BootMode::detect(Some(&mock)).unwrap();
        assert!(
            matches!(
                mode,
                BootMode::FactoryReset(FactoryResetTrigger::Rejected(_))
            ),
            "unsupported mode {mode_value} must be reported, not ignored"
        );
    }
}

#[test]
fn detect_supported_mode_selects_factory_reset() {
    for (mode_value, expected) in [
        ("1", ResetMode::Mode1),
        ("2", ResetMode::Mode2),
        ("3", ResetMode::Mode3),
    ] {
        let trigger = format!(r#"{{"mode":{mode_value},"preserve":["applications"]}}"#);
        let mock = MockBootEnv::new().with_env(BootEnvKey::FactoryReset, &trigger);
        let mode = BootMode::detect(Some(&mock)).unwrap();
        let BootMode::FactoryReset(FactoryResetTrigger::Accepted(config)) = mode else {
            panic!("supported mode {mode_value} must select FactoryReset");
        };
        assert_eq!(config.mode, expected, "mode {mode_value} mapped wrongly");
        assert_eq!(config.preserve, vec!["applications".to_string()]);
    }
}
