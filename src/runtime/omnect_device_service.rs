//! omnect-device-service integration
//!
//! Creates runtime files that omnect-device-service reads at startup.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path;

use nix::unistd::{Gid, Uid, chown};
use serde::Serialize;

use crate::bootloader::{BootEnv, BootEnvKey};
use crate::error::{InitramfsError, Result};
use crate::filesystem::{FsckExitCode, mount_points, paths};
use crate::partition::PartitionName;

/// Directory for ODS runtime files.
/// Written to the initramfs /run tmpfs; switch_root moves /run into the new
/// root via MS_MOVE, so these files appear at the same path after boot.
pub const ODS_RUNTIME_DIR: &str = "/run/omnect-device-service";

/// Main status file name
const ODS_STATUS_FILE: &str = "omnect-os-initramfs.json";

/// Update validation trigger file
const UPDATE_VALIDATE_FILE: &str = "omnect_validate_update";

/// Failed update validation marker
const UPDATE_VALIDATE_FAILED_FILE: &str = "omnect_validate_update_failed";

/// BootEnv updated marker
const BOOTLOADER_UPDATED_FILE: &str = "omnect_bootloader_updated";

/// Config of a local update validation, relative to `/var/lib` on the data partition
const UPDATE_VALIDATION_CONF: &str = "omnect-device-service/update_validation_conf.json";

/// Name of the omnect-device-service user and group in the rootfs
const ODS_USER: &str = "omnect_device_service";
const ODS_GROUP: &str = "omnect_device_service";

/// File and directory permission modes for ODS runtime files.
#[derive(Debug, Clone, Copy)]
enum FilePermission {
    /// `rwxrwxr-x` (0o775) — readable and executable by all, writable by owner and group
    DirStandard,
    /// `rw-------` (0o600) — readable and writable only by owner (ODS)
    FileRestricted,
    /// `rw-r--r--` (0o644) — readable by all, writable only by owner (ODS)
    FileReadable,
}

impl FilePermission {
    fn bits(self) -> u32 {
        match self {
            Self::DirStandard => 0o775,
            Self::FileRestricted => 0o600,
            Self::FileReadable => 0o644,
        }
    }
}

/// Content of the trigger files. Consumers only test for existence, so the
/// value carries no meaning.
const TRIGGER_FILE_CONTENT: &str = "1";

/// Outcome codes for a factory reset operation.
///
/// Serialized as a plain integer for the `omnect-device-service` JSON contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactoryResetStatusCode {
    Success = 0,
    Invalid = 1,
    Error = 2,
    ConfigError = 3,
    Warning = 4,
}

impl serde::Serialize for FactoryResetStatusCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_u32(*self as u32)
    }
}

impl fmt::Display for FactoryResetStatusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::Invalid => write!(f, "invalid"),
            Self::Error => write!(f, "error"),
            Self::ConfigError => write!(f, "config_error"),
            Self::Warning => write!(f, "warning"),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OdsStatus {
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub fsck: HashMap<PartitionName, FsckStatus>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory_reset: Option<FactoryResetStatus>,

    /// Carries the boot-env error cause when the env was unavailable on this
    /// boot, so ODS consumers can diagnose which tool or file failed rather
    /// than receiving an opaque signal. `None` when the env was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_boot: Option<DegradedBootStatus>,

    /// `true` if this boot is the first boot since flashing, i.e. the
    /// `omnect_first_boot_done` marker did not hold its value at run_init time
    /// and no update flag was set.
    /// Always serialized — absence of the key would itself be a bug.
    pub first_boot: bool,

    /// Carries the resize-data failure cause when resize did not complete on
    /// this boot, so ODS consumers can diagnose and notify the cloud. `None`
    /// when resize completed (succeeded or guard already present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resize_data: Option<ResizeStatus>,

    /// Extra-bootargs sync failure on this boot. `None` on success, no-op, or
    /// when the step did not run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_bootargs: Option<ExtraBootArgsStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FsckStatus {
    pub code: i32,
    /// fsck output; may be stored compressed in the bootloader env.
    pub output: String,
}

/// Reason the boot env was unavailable on this boot.
///
/// Present only on release images where env-unavailable is treated as a
/// degraded-continue rather than a fatal abort. Debug images abort immediately,
/// so this struct is never populated there.
#[derive(Debug, Clone, Serialize)]
pub struct DegradedBootStatus {
    /// The `Display` of the underlying `BootEnvError`.
    pub reason: String,
}

/// Why resize-data did not complete on this boot.
///
/// Serialized as snake_case so ODS and cloud consumers can match exact strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeOutcome {
    /// Data partition fsck reported uncorrected errors before resize.
    SkippedFsck,
    /// An external tool (parted / sgdisk / resize2fs / sync) failed.
    ToolError,
    /// Layout problem (missing data partition, non-UTF-8 path, …) —
    /// the init setup step could not run at all.
    InvalidLayout,
}

/// Reason resize-data did not complete on this boot.
///
/// Present only when resize was attempted and could not finish.
/// `None` means resize succeeded or the guard was already present.
#[derive(Debug, Clone, Serialize)]
pub struct ResizeStatus {
    /// Why resize did not complete.
    pub outcome: ResizeOutcome,
    /// The `Display` of the underlying error — one line for operator diagnosis.
    pub reason: String,
}

/// Why the extra-bootargs sync failed on this boot.
///
/// Serialized as snake_case so ODS and cloud consumers can match exact strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtraBootArgsOutcome {
    /// Reading a boot-partition argument file failed.
    FileReadFailed,
    /// Reading the current value failed (before any write).
    ReadFailed,
    /// Writing the env value failed.
    SetEnvFailed,
    /// The read-back after a successful write failed — the value is written
    /// but unverified.
    ReadBackFailed,
    /// The stored value read back different from what was written.
    ReadBackMismatch,
}

/// Extra-bootargs sync failure, for ODS diagnosis.
#[derive(Debug, Clone, Serialize)]
pub struct ExtraBootArgsStatus {
    /// The failure kind, for exact matching by consumers.
    pub outcome: ExtraBootArgsOutcome,
    /// One-line detail for operator diagnosis.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FactoryResetStatus {
    pub status: FactoryResetStatusCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// `true` once the destructive phase (partition reformat) began. On
    /// `status: Error` this distinguishes a safe no-op abort (nothing touched)
    /// from a failure after data was already wiped. Always serialized — absence
    /// of the key would itself be a bug.
    pub data_wiped: bool,
}

impl OdsStatus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clean results are left out, so the JSON names only the partitions that
    /// needed attention.
    pub fn record_fsck_result(
        &mut self,
        partition: PartitionName,
        code: FsckExitCode,
        output: String,
    ) {
        if code.is_clean() {
            return;
        }
        self.fsck.insert(
            partition,
            FsckStatus {
                code: code.bits(),
                output,
            },
        );
    }

    pub fn set_factory_reset(&mut self, status: FactoryResetStatus) {
        self.factory_reset = Some(status);
    }

    /// Stores the boot-env error cause for ODS. `reason` is the `Display`
    /// of the `BootEnvError` returned by `open_boot_env()`.
    pub fn set_degraded_boot(&mut self, reason: String) {
        self.degraded_boot = Some(DegradedBootStatus { reason });
    }

    /// Record a resize-data failure indicator for ODS.
    pub fn set_resize_status(&mut self, status: ResizeStatus) {
        self.resize_data = Some(status);
    }

    /// Record an extra-bootargs sync failure for ODS.
    pub fn set_extra_bootargs_status(&mut self, status: ExtraBootArgsStatus) {
        self.extra_bootargs = Some(status);
    }
}

/// Create all runtime files for omnect-device-service
///
/// Files are written directly to the initramfs `/run` tmpfs. `switch_root`
/// moves that mount into the new root via `MS_MOVE`, so they remain visible
/// to ODS at the same path after the root pivot.
///
/// Ownership and permissions:
/// - dir: omnect_device_service:omnect_device_service, 775
/// - status JSON: 600
/// - trigger files: 644
/// - bootloader_updated: 600
pub fn create_ods_runtime_files(
    status: &OdsStatus,
    bootloader: Option<&mut dyn BootEnv>,
    rootfs_dir: &Path,
    ods_dir: &Path,
) -> Result<()> {
    let uid = lookup_uid(rootfs_dir, ODS_USER)?;
    let gid = lookup_gid(rootfs_dir, ODS_GROUP)?;

    fs::create_dir_all(ods_dir).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to create ODS runtime dir: {}",
            e
        )))
    })?;
    set_ownership(ods_dir, uid, gid)?;
    set_mode(ods_dir, FilePermission::DirStandard)?;

    write_status_file(ods_dir, status)?;
    set_ownership(&ods_dir.join(ODS_STATUS_FILE), uid, gid)?;
    set_mode(
        &ods_dir.join(ODS_STATUS_FILE),
        FilePermission::FileRestricted,
    )?;

    // Skipped if the bootloader failed to initialise at runtime (e.g. corrupted boot partition).
    if let Some(bl) = bootloader {
        handle_update_validation(ods_dir, rootfs_dir, bl, uid, gid)?;
    } else {
        log::warn!(
            "update validation skipped — bootloader unavailable; \
             any in-flight A/B update will roll back on timer expiry"
        );
    }

    log::info!("Created ODS runtime files in {}", ods_dir.display());

    Ok(())
}

fn write_status_file(ods_dir: &Path, status: &OdsStatus) -> Result<()> {
    let status_path = ods_dir.join(ODS_STATUS_FILE);
    let json = serde_json::to_string_pretty(status).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to serialize ODS status: {}",
            e
        )))
    })?;

    fs::write(&status_path, json).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to write ODS status to {}: {}",
            status_path.display(),
            e
        )))
    })?;
    log::debug!("Wrote ODS status to {}", status_path.display());

    Ok(())
}

/// Names the key on failure — `BootEnvError` carries the failing command, not
/// the variable.
fn read_flag(bootloader: &dyn BootEnv, key: BootEnvKey) -> Result<bool> {
    bootloader
        .is_flag_set(key)
        .inspect_err(|e| log::error!("failed to read {}: {e}", key.as_str()))
        .map_err(Into::into)
}

/// Clear a boot-env flag whose trigger file was just created.
///
/// Best-effort: on failure the trigger file reappears on the next boot, which
/// is not worth aborting a boot that is otherwise complete.
fn clear_flag(bootloader: &mut dyn BootEnv, key: BootEnvKey) {
    if let Err(e) = bootloader.set_env(key, None) {
        log::warn!("failed to clear {}: {e}", key.as_str());
    }
}

fn write_trigger_file(
    ods_dir: &Path,
    name: &str,
    mode: FilePermission,
    uid: Uid,
    gid: Gid,
) -> Result<()> {
    let path = ods_dir.join(name);
    fs::write(&path, TRIGGER_FILE_CONTENT).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to write {}: {}",
            path.display(),
            e
        )))
    })?;
    set_ownership(&path, uid, gid)?;
    set_mode(&path, mode)
}

/// Write the first-boot marker unless it is set; `true` if it is set afterwards.
///
/// A read error counts as not set: writing the marker again is harmless.
fn ensure_first_boot_marker(bootloader: &mut dyn BootEnv) -> bool {
    if bootloader.is_first_boot_done().is_ok_and(|done| done) {
        return true;
    }
    match bootloader.set_first_boot_done() {
        Ok(()) => {
            log::info!("first-boot marker set on a boot after an update");
            true
        }
        Err(e) => {
            log::warn!("first-boot marker write failed: {e}");
            false
        }
    }
}

/// Clear an update flag once the first-boot marker is set.
fn clear_update_flag(bootloader: &mut dyn BootEnv, key: BootEnvKey, marker_set: bool) {
    if marker_set {
        clear_flag(bootloader, key);
    } else {
        log::error!(
            "{} kept set: its trigger file is written again on the next boot",
            key.as_str()
        );
    }
}

/// Handle update validation workflow; applies ownership and permissions to any
/// trigger files it creates.
///
/// `omnect_validate_update_failed` and `omnect_bootloader_updated` are cleared
/// once their trigger file exists. No other component resets them, so without
/// this the file would reappear on every later boot.
///
/// A boot after an update first sets the first-boot marker. The flags cleared
/// here stay set until it is written, so the next boot is not taken for a first
/// boot. ODS clears `omnect_validate_update`: if the marker write fails on a
/// validation boot, the boot after the validation runs the first-boot steps.
fn handle_update_validation(
    ods_dir: &Path,
    rootfs_dir: &Path,
    bootloader: &mut dyn BootEnv,
    uid: Uid,
    gid: Gid,
) -> Result<()> {
    let validate_update = read_flag(bootloader, BootEnvKey::ValidateUpdate)?;
    let validate_update_failed = read_flag(bootloader, BootEnvKey::ValidateUpdateFailed)?;

    // The bootloader clears omnect_validate_update when it sets the failed flag,
    // so both at once means the env is inconsistent. Either trigger file would
    // send ODS down the wrong update path, so refuse to guess.
    if validate_update && validate_update_failed {
        return Err(InitramfsError::ConflictingUpdateFlags);
    }

    if !validate_update {
        remove_update_validation_conf(rootfs_dir);
    }

    let bootloader_updated = read_flag(bootloader, BootEnvKey::BootloaderUpdated)?;
    let after_update = validate_update || validate_update_failed || bootloader_updated;
    let marker_set = after_update && ensure_first_boot_marker(bootloader);

    if validate_update {
        write_trigger_file(
            ods_dir,
            UPDATE_VALIDATE_FILE,
            FilePermission::FileReadable,
            uid,
            gid,
        )?;
        log::info!("Update validation requested - created trigger file");
    }

    if validate_update_failed {
        write_trigger_file(
            ods_dir,
            UPDATE_VALIDATE_FAILED_FILE,
            FilePermission::FileReadable,
            uid,
            gid,
        )?;
        clear_update_flag(bootloader, BootEnvKey::ValidateUpdateFailed, marker_set);
        log::warn!("Update validation failed marker created");
    }

    if bootloader_updated {
        write_trigger_file(
            ods_dir,
            BOOTLOADER_UPDATED_FILE,
            FilePermission::FileRestricted,
            uid,
            gid,
        )?;
        clear_update_flag(bootloader, BootEnvKey::BootloaderUpdated, marker_set);
        log::info!("BootEnv update marker created");
    }

    Ok(())
}

/// ODS writes the config before the reboot into the validation boot of an
/// update it runs itself, and removes it after a successful validation. A failed
/// validation leaves it behind. An ADU update does not write it but its
/// validation reads it, so a leftover `"local": true` skips the IoT Hub check.
///
/// Best-effort: a boot is not aborted for a file ODS treats as optional.
fn remove_update_validation_conf(rootfs_dir: &Path) {
    let path = rootfs_dir
        .join(mount_points::DATA_PARTITION)
        .join(paths::VAR_LIB)
        .join(UPDATE_VALIDATION_CONF);
    match fs::remove_file(&path) {
        Ok(()) => log::info!("removed stale {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("failed to remove {}: {e}", path.display()),
    }
}

/// Look up the UID for a user in the rootfs /etc/passwd.
fn lookup_uid(rootfs_dir: &Path, username: &str) -> Result<Uid> {
    let passwd = rootfs_dir.join("etc/passwd");
    let content = fs::read_to_string(&passwd).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to read {}: {}",
            passwd.display(),
            e
        )))
    })?;
    for line in content.lines() {
        let mut fields = line.splitn(7, ':');
        let name = fields.next().unwrap_or("");
        if name != username {
            continue;
        }
        let _password = fields.next();
        if let Some(uid_str) = fields.next() {
            return uid_str.parse::<u32>().map(Uid::from_raw).map_err(|e| {
                InitramfsError::Io(std::io::Error::other(format!(
                    "Invalid UID for {}: {}",
                    username, e
                )))
            });
        }
    }
    Err(InitramfsError::Io(std::io::Error::other(format!(
        "user {} not found in {}",
        username,
        passwd.display()
    ))))
}

/// Look up the numeric GID for a group in the rootfs /etc/group.
fn lookup_gid(rootfs_dir: &Path, groupname: &str) -> Result<Gid> {
    let group = rootfs_dir.join("etc/group");
    let content = fs::read_to_string(&group).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to read {}: {}",
            group.display(),
            e
        )))
    })?;
    for line in content.lines() {
        let mut fields = line.splitn(4, ':');
        let name = fields.next().unwrap_or("");
        if name != groupname {
            continue;
        }
        let _password = fields.next();
        if let Some(gid_str) = fields.next() {
            return gid_str.parse::<u32>().map(Gid::from_raw).map_err(|e| {
                InitramfsError::Io(std::io::Error::other(format!(
                    "Invalid GID for {}: {}",
                    groupname, e
                )))
            });
        }
    }
    Err(InitramfsError::Io(std::io::Error::other(format!(
        "group {} not found in {}",
        groupname,
        group.display()
    ))))
}

fn set_ownership(path: &Path, uid: Uid, gid: Gid) -> Result<()> {
    chown(path, Some(uid), Some(gid)).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to chown {}: {}",
            path.display(),
            e
        )))
    })
}

fn set_mode(path: &Path, mode: FilePermission) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode.bits())).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to chmod {}: {}",
            path.display(),
            e
        )))
    })
}

/// Compares the ODS JSON of `status` with
/// `tests/contract/status-factory-reset-<name>.json`.
#[cfg(test)]
pub(crate) fn assert_factory_reset_contract(name: &str, status: FactoryResetStatus) {
    let mut ods = OdsStatus::new();
    ods.set_factory_reset(status);
    let file = format!(
        "{}/tests/contract/status-factory-reset-{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let expected: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(&ods).unwrap(), expected, "{file}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootloader::BootEnvState;
    use crate::partition::PartitionName;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn extra_bootargs_failure_serializes_kind_and_reason() {
        let mut ods = OdsStatus::new();
        ods.set_extra_bootargs_status(ExtraBootArgsStatus {
            outcome: ExtraBootArgsOutcome::SetEnvFailed,
            reason: "boom".to_string(),
        });
        let json = serde_json::to_string(&ods).unwrap();
        assert!(json.contains(r#""extra_bootargs""#));
        assert!(json.contains(r#""outcome":"set_env_failed""#));
        assert!(json.contains(r#""reason":"boom""#));
    }

    #[test]
    fn extra_bootargs_absent_is_not_serialized() {
        let ods = OdsStatus::new();
        let json = serde_json::to_string(&ods).unwrap();
        assert!(!json.contains("extra_bootargs"));
    }

    fn current_uid() -> Uid {
        nix::unistd::getuid()
    }

    fn current_gid() -> Gid {
        nix::unistd::getgid()
    }

    /// Create a minimal rootfs with /etc/passwd and /etc/group for ODS user,
    /// using the current process's uid/gid so chown succeeds without root.
    fn make_fake_rootfs(uid: Uid, gid: Gid) -> TempDir {
        let rootfs = TempDir::new().unwrap();
        let etc = rootfs.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(
            etc.join("passwd"),
            format!(
                "root:x:0:0:root:/root:/bin/sh\nomnect_device_service:x:{}:{}::/:/bin/sh\n",
                uid.as_raw(),
                gid.as_raw()
            ),
        )
        .unwrap();
        fs::write(
            etc.join("group"),
            format!("root:x:0:\nomnect_device_service:x:{}:\n", gid.as_raw()),
        )
        .unwrap();
        rootfs
    }

    #[test]
    fn test_ods_status_default() {
        let status = OdsStatus::default();
        assert!(status.fsck.is_empty());
        assert!(status.factory_reset.is_none());
    }

    #[test]
    fn test_ods_status_add_fsck() {
        let mut status = OdsStatus::new();
        status.record_fsck_result(PartitionName::Boot, FsckExitCode::OK, "clean".to_string());
        status.record_fsck_result(
            PartitionName::Data,
            FsckExitCode::CORRECTED,
            "errors corrected".to_string(),
        );

        assert_eq!(status.fsck.len(), 1, "clean results must not be recorded");
        assert!(!status.fsck.contains_key(&PartitionName::Boot));
        assert_eq!(status.fsck.get(&PartitionName::Data).unwrap().code, 1);
    }

    #[test]
    fn test_ods_status_serialization() {
        let mut status = OdsStatus::new();
        status.record_fsck_result(
            PartitionName::Boot,
            FsckExitCode::CORRECTED,
            "errors corrected".to_string(),
        );

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"boot\""));
        assert!(json.contains("\"code\":1"));
        assert!(json.contains("\"errors corrected\""));
    }

    #[test]
    fn test_ods_status_serialization_omits_clean_fsck() {
        let mut status = OdsStatus::new();
        status.record_fsck_result(PartitionName::Boot, FsckExitCode::OK, "clean".to_string());

        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("fsck"), "got: {json}");
    }

    #[test]
    fn test_write_status_file() {
        let temp = TempDir::new().unwrap();
        let status = OdsStatus::new();

        write_status_file(temp.path(), &status).unwrap();

        let status_path = temp.path().join(ODS_STATUS_FILE);
        assert!(status_path.exists());

        let content = fs::read_to_string(status_path).unwrap();
        assert!(content.contains("{"));
    }

    #[test]
    fn test_factory_reset_status_serialization() {
        let status = FactoryResetStatus {
            status: FactoryResetStatusCode::Success,
            error: None,
            context: Some("normal".to_string()),
            paths: vec!["/etc/hostname".to_string()],
            data_wiped: true,
        };

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"status\":0"));
        assert!(json.contains("\"paths\""));
    }

    #[test]
    fn test_factory_reset_status_code_serializes_as_integer() {
        use serde_json::Value;
        let cases: &[(FactoryResetStatusCode, u64)] = &[
            (FactoryResetStatusCode::Success, 0),
            (FactoryResetStatusCode::Invalid, 1),
            (FactoryResetStatusCode::Error, 2),
            (FactoryResetStatusCode::ConfigError, 3),
        ];
        for (variant, expected) in cases {
            let s = FactoryResetStatus {
                status: *variant,
                error: None,
                context: None,
                paths: vec![],
                data_wiped: false,
            };
            let json: Value = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
            assert_eq!(json["status"], *expected, "variant {:?}", variant);
        }
    }

    /// The files in `tests/contract` are the factory reset results that
    /// omnect-device-service must parse. The factory reset module checks the
    /// files it can build from its own status functions.
    #[test]
    fn factory_reset_status_matches_the_contract_files() {
        use FactoryResetStatusCode::*;
        let cases = [
            ("success", Success, None, None, &["/etc/hostname"][..], true),
            (
                "invalid",
                Invalid,
                Some("unknown factory reset mode 7"),
                None,
                &[][..],
                false,
            ),
            (
                "error-before-wipe",
                Error,
                Some("backup of /etc/hostname failed"),
                None,
                &[][..],
                false,
            ),
            (
                "config-error",
                ConfigError,
                Some("unknown preserve topic foo"),
                None,
                &[][..],
                false,
            ),
        ];

        for (name, status, error, context, paths, data_wiped) in cases {
            assert_factory_reset_contract(
                name,
                FactoryResetStatus {
                    status,
                    error: error.map(str::to_string),
                    context: context.map(str::to_string),
                    paths: paths.iter().map(|p| p.to_string()).collect(),
                    data_wiped,
                },
            );
        }
    }

    #[test]
    fn factory_reset_data_wiped_always_serialized() {
        // Absence of the key would itself be a bug (mirrors first_boot_always_serialized) —
        // ODS/cloud must be able to distinguish a safe pre-reformat abort (false) from a
        // failure after data was already wiped (true).
        for wiped in [false, true] {
            let s = FactoryResetStatus {
                status: FactoryResetStatusCode::Error,
                error: Some("test".to_string()),
                context: None,
                paths: vec![],
                data_wiped: wiped,
            };
            let json = serde_json::to_string(&s).unwrap();
            assert!(
                json.contains(&format!("\"data_wiped\":{wiped}")),
                "data_wiped={wiped} must be serialized; got: {json}"
            );
        }
    }

    #[test]
    fn test_factory_reset_status_code_display() {
        assert_eq!(FactoryResetStatusCode::Success.to_string(), "success");
        assert_eq!(FactoryResetStatusCode::Invalid.to_string(), "invalid");
        assert_eq!(FactoryResetStatusCode::Error.to_string(), "error");
        assert_eq!(
            FactoryResetStatusCode::ConfigError.to_string(),
            "config_error"
        );
    }

    fn update_validation_conf(rootfs: &Path) -> std::path::PathBuf {
        rootfs
            .join(mount_points::DATA_PARTITION)
            .join(paths::VAR_LIB)
            .join(UPDATE_VALIDATION_CONF)
    }

    fn write_update_validation_conf(rootfs: &Path) -> std::path::PathBuf {
        let conf = update_validation_conf(rootfs);
        fs::create_dir_all(conf.parent().unwrap()).unwrap();
        fs::write(&conf, r#"{"local": true}"#).unwrap();
        conf
    }

    #[test]
    fn test_update_validation_conf_removed_without_validation() {
        let temp = TempDir::new().unwrap();
        let rootfs = TempDir::new().unwrap();
        let conf = write_update_validation_conf(rootfs.path());
        let mut bl = crate::bootloader::create_mock_bootloader();

        handle_update_validation(
            temp.path(),
            rootfs.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(!conf.exists());
    }

    #[test]
    fn test_update_validation_conf_missing_is_not_an_error() {
        let temp = TempDir::new().unwrap();
        let rootfs = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader();

        handle_update_validation(
            temp.path(),
            rootfs.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(!update_validation_conf(rootfs.path()).exists());
    }

    #[test]
    fn test_update_validation_conf_kept_on_validation_boot() {
        let temp = TempDir::new().unwrap();
        let rootfs = TempDir::new().unwrap();
        let conf = write_update_validation_conf(rootfs.path());
        let mut bl =
            crate::bootloader::create_mock_bootloader().with_env(BootEnvKey::ValidateUpdate, "1");

        handle_update_validation(
            temp.path(),
            rootfs.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(conf.exists());
    }

    #[test]
    fn test_update_validation_conf_removed_after_failed_validation() {
        let temp = TempDir::new().unwrap();
        let rootfs = TempDir::new().unwrap();
        let conf = write_update_validation_conf(rootfs.path());
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::ValidateUpdateFailed, "1");

        handle_update_validation(
            temp.path(),
            rootfs.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(!conf.exists());
    }

    #[test]
    fn test_handle_update_validation_value_1() {
        let temp = TempDir::new().unwrap();
        let mut bl =
            crate::bootloader::create_mock_bootloader().with_env(BootEnvKey::ValidateUpdate, "1");

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(temp.path().join(UPDATE_VALIDATE_FILE).exists());
        assert!(!temp.path().join(UPDATE_VALIDATE_FAILED_FILE).exists());
        assert!(!temp.path().join(BOOTLOADER_UPDATED_FILE).exists());
    }

    #[test]
    fn test_handle_update_validation_keeps_validate_update_set() {
        // ODS unsets omnect_validate_update itself once validation completes;
        // clearing it here would lose the in-flight update on the next boot.
        let temp = TempDir::new().unwrap();
        let mut bl =
            crate::bootloader::create_mock_bootloader().with_env(BootEnvKey::ValidateUpdate, "1");

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert_eq!(
            bl.get_env(BootEnvKey::ValidateUpdate).unwrap().as_deref(),
            Some("1")
        );
    }

    #[test]
    fn test_handle_update_validation_any_non_empty_value_triggers() {
        // Flags carry meaning in presence, not value.
        for value in ["1", "0", "true", "unexpected"] {
            let temp = TempDir::new().unwrap();
            let mut bl = crate::bootloader::create_mock_bootloader()
                .with_env(BootEnvKey::ValidateUpdate, value);

            handle_update_validation(
                temp.path(),
                temp.path(),
                &mut bl,
                current_uid(),
                current_gid(),
            )
            .unwrap();

            assert!(
                temp.path().join(UPDATE_VALIDATE_FILE).exists(),
                "value {value:?} must create the trigger file"
            );
        }
    }

    #[test]
    fn test_handle_update_validation_empty_value_creates_nothing() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::ValidateUpdate, "")
            .with_env(BootEnvKey::ValidateUpdateFailed, "")
            .with_env(BootEnvKey::BootloaderUpdated, "");

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(!temp.path().join(UPDATE_VALIDATE_FILE).exists());
        assert!(!temp.path().join(UPDATE_VALIDATE_FAILED_FILE).exists());
        assert!(!temp.path().join(BOOTLOADER_UPDATED_FILE).exists());
    }

    #[test]
    fn test_handle_update_validation_failed_clears_env() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::ValidateUpdateFailed, "1");

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(!temp.path().join(UPDATE_VALIDATE_FILE).exists());
        assert!(temp.path().join(UPDATE_VALIDATE_FAILED_FILE).exists());
        assert_eq!(
            bl.get_env(BootEnvKey::ValidateUpdateFailed).unwrap(),
            None,
            "marker would reappear on every later boot if the flag stays set"
        );
    }

    #[test]
    fn test_handle_update_validation_both_flags_set_is_fatal() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::ValidateUpdate, "1")
            .with_env(BootEnvKey::ValidateUpdateFailed, "1");

        let result = handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        );

        assert!(matches!(
            result,
            Err(InitramfsError::ConflictingUpdateFlags)
        ));
        // Neither trigger file may exist — ODS must not act on a guess.
        assert!(!temp.path().join(UPDATE_VALIDATE_FILE).exists());
        assert!(!temp.path().join(UPDATE_VALIDATE_FAILED_FILE).exists());
    }

    #[test]
    fn test_handle_update_validation_bootloader_updated_clears_env() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::BootloaderUpdated, "1");

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(temp.path().join(BOOTLOADER_UPDATED_FILE).exists());
        assert_eq!(
            bl.get_env(BootEnvKey::BootloaderUpdated).unwrap(),
            None,
            "marker would reappear on every later boot if the flag stays set"
        );
    }

    /// The flags `handle_update_validation` clears; `ValidateUpdate` is cleared by ODS.
    const CLEARED_FLAGS: [BootEnvKey; 2] = [
        BootEnvKey::ValidateUpdateFailed,
        BootEnvKey::BootloaderUpdated,
    ];

    fn capture_update_validation(
        bl: &mut crate::bootloader::MockBootEnv,
    ) -> (TempDir, Vec<String>) {
        let temp = TempDir::new().unwrap();
        crate::logging::capture::start_capture();
        handle_update_validation(temp.path(), temp.path(), bl, current_uid(), current_gid())
            .unwrap();
        (temp, crate::logging::capture::take_capture())
    }

    #[test]
    fn test_handle_update_validation_sets_marker_for_every_update_flag() {
        for key in crate::bootloader::UPDATE_FLAGS {
            let temp = TempDir::new().unwrap();
            let mut bl = crate::bootloader::create_mock_bootloader().with_env(key, "1");

            handle_update_validation(
                temp.path(),
                temp.path(),
                &mut bl,
                current_uid(),
                current_gid(),
            )
            .unwrap();

            assert!(bl.is_first_boot_done().unwrap(), "{} set", key.as_str());
        }
    }

    #[test]
    fn test_handle_update_validation_writes_marker_before_the_clear() {
        for key in CLEARED_FLAGS {
            let temp = TempDir::new().unwrap();
            let mut bl = crate::bootloader::create_mock_bootloader().with_env(key, "1");
            let calls = bl.shared_set_env_calls();

            handle_update_validation(
                temp.path(),
                temp.path(),
                &mut bl,
                current_uid(),
                current_gid(),
            )
            .unwrap();

            assert_eq!(*calls.lock().unwrap(), [BootEnvKey::FirstBootDone, key]);
        }
    }

    #[test]
    fn test_handle_update_validation_marker_set_writes_only_the_clear() {
        for key in CLEARED_FLAGS {
            let temp = TempDir::new().unwrap();
            let mut bl = crate::bootloader::create_mock_bootloader().with_env(key, "1");
            bl.set_first_boot_done().unwrap();
            let calls = bl.shared_set_env_calls();
            calls.lock().unwrap().clear();

            handle_update_validation(
                temp.path(),
                temp.path(),
                &mut bl,
                current_uid(),
                current_gid(),
            )
            .unwrap();

            assert_eq!(*calls.lock().unwrap(), [key]);
        }
    }

    #[test]
    fn test_handle_update_validation_rewrites_another_marker_value() {
        for value in ["", "yes"] {
            let temp = TempDir::new().unwrap();
            let mut bl = crate::bootloader::create_mock_bootloader()
                .with_env(BootEnvKey::FirstBootDone, value)
                .with_env(BootEnvKey::BootloaderUpdated, "1");
            let calls = bl.shared_set_env_calls();

            handle_update_validation(
                temp.path(),
                temp.path(),
                &mut bl,
                current_uid(),
                current_gid(),
            )
            .unwrap();

            assert_eq!(
                *calls.lock().unwrap(),
                [BootEnvKey::FirstBootDone, BootEnvKey::BootloaderUpdated],
                "{value:?}"
            );
        }
    }

    #[test]
    fn test_handle_update_validation_rewrites_an_unreadable_marker() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::BootloaderUpdated, "1")
            .with_get_env_error_for(BootEnvKey::FirstBootDone);
        let calls = bl.shared_set_env_calls();

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            [BootEnvKey::FirstBootDone, BootEnvKey::BootloaderUpdated]
        );
    }

    #[test]
    fn test_handle_update_validation_no_flag_writes_nothing() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader();
        let calls = bl.shared_set_env_calls();

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_handle_update_validation_failed_marker_write_keeps_flags() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();

        for (key, file) in CLEARED_FLAGS
            .into_iter()
            .zip([UPDATE_VALIDATE_FAILED_FILE, BOOTLOADER_UPDATED_FILE])
        {
            let mut bl = crate::bootloader::create_mock_bootloader()
                .with_env(key, "1")
                .with_set_env_error_for(BootEnvKey::FirstBootDone);
            let calls = bl.shared_set_env_calls();

            let (temp, lines) = capture_update_validation(&mut bl);

            assert!(temp.path().join(file).exists(), "{file}");
            assert!(
                calls.lock().unwrap().is_empty(),
                "{} must stay set so the next boot is no first boot",
                key.as_str()
            );
            let expected = format!("[ERROR] {} kept set", key.as_str());
            assert!(
                lines.iter().any(|line| line.starts_with(&expected)),
                "got {lines:?}"
            );
        }
    }

    #[test]
    fn test_handle_update_validation_failed_marker_write_on_validation_boot_is_ok() {
        // A fatal error here would reboot the validation boot into a rollback.
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::ValidateUpdate, "1")
            .with_set_env_error_for(BootEnvKey::FirstBootDone);

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(temp.path().join(UPDATE_VALIDATE_FILE).exists());
    }

    #[test]
    fn test_handle_update_validation_logs_the_marker_write() {
        let _guard = crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::logging::capture::install_test_logger();

        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::BootloaderUpdated, "1");
        let (_temp, lines) = capture_update_validation(&mut bl);
        assert!(
            lines
                .iter()
                .any(|line| line
                    .starts_with("[INFO] first-boot marker set on a boot after an update")),
            "got {lines:?}"
        );

        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::BootloaderUpdated, "1")
            .with_set_env_error_for(BootEnvKey::FirstBootDone);
        let (_temp, lines) = capture_update_validation(&mut bl);
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("[WARN] first-boot marker write failed: ")),
            "got {lines:?}"
        );
    }

    /// One boot as far as the first-boot marker goes: detection, the ODS
    /// runtime files, the marker write at the end of `normal::run`.
    fn boot(env: &mut BootEnvState, rootfs: &Path, ods_dir: &Path) -> bool {
        let mut status = OdsStatus::new();
        status.first_boot = crate::detect_first_boot(env);
        create_ods_runtime_files(&status, env.available_mut(), rootfs, ods_dir).unwrap();
        crate::mode::normal::write_first_boot_marker(status.first_boot, env);
        status.first_boot
    }

    #[test]
    fn test_first_boot_ends_after_one_boot() {
        let rootfs = make_fake_rootfs(current_uid(), current_gid());
        let fresh = (None, true, vec![BootEnvKey::FirstBootDone]);
        let validation = (
            Some(BootEnvKey::ValidateUpdate),
            false,
            vec![BootEnvKey::FirstBootDone],
        );
        let rollback = (
            Some(BootEnvKey::ValidateUpdateFailed),
            false,
            vec![BootEnvKey::FirstBootDone, BootEnvKey::ValidateUpdateFailed],
        );
        let bootloader_update = (
            Some(BootEnvKey::BootloaderUpdated),
            false,
            vec![BootEnvKey::FirstBootDone, BootEnvKey::BootloaderUpdated],
        );

        for (flag, first_boot, writes) in [fresh, validation, rollback, bootloader_update] {
            let mut mock = crate::bootloader::create_mock_bootloader();
            if let Some(flag) = flag {
                mock = mock.with_env(flag, "1");
            }
            let calls = mock.shared_set_env_calls();
            let mut env = BootEnvState::Available(Box::new(mock));
            let ods_dir = TempDir::new().unwrap();

            assert_eq!(boot(&mut env, rootfs.path(), ods_dir.path()), first_boot);
            assert_eq!(*calls.lock().unwrap(), writes, "{flag:?}");

            // ODS clears omnect_validate_update once the update is validated.
            let bl = env.available_mut().unwrap();
            bl.set_env(BootEnvKey::ValidateUpdate, None).unwrap();
            assert!(!boot(&mut env, rootfs.path(), ods_dir.path()), "{flag:?}");
        }
    }

    #[test]
    fn test_handle_update_validation_clear_failure_does_not_abort() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::BootloaderUpdated, "1")
            .with_set_env_error_for(BootEnvKey::BootloaderUpdated);

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(temp.path().join(BOOTLOADER_UPDATED_FILE).exists());
        assert!(bl.is_first_boot_done().unwrap());
    }

    #[test]
    fn test_handle_update_validation_read_error_is_typed_as_bootloader() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader().with_get_env_error();

        let result = handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        );

        assert!(
            matches!(result, Err(InitramfsError::Bootloader(_))),
            "a boot-env read failure must name the bootloader, not report an IO error"
        );
    }

    #[test]
    fn test_trigger_file_modes_match_the_documented_contract() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(BootEnvKey::ValidateUpdate, "1")
            .with_env(BootEnvKey::BootloaderUpdated, "1");

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        let mode = |name: &str| {
            fs::metadata(temp.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(UPDATE_VALIDATE_FILE), 0o644, "ODS reads this file");
        assert_eq!(
            mode(BOOTLOADER_UPDATED_FILE),
            0o600,
            "owner-only, per the contract on create_ods_runtime_files"
        );
    }

    #[test]
    fn test_handle_update_validation_no_env_creates_nothing() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader();

        handle_update_validation(
            temp.path(),
            temp.path(),
            &mut bl,
            current_uid(),
            current_gid(),
        )
        .unwrap();

        assert!(!temp.path().join(UPDATE_VALIDATE_FILE).exists());
        assert!(!temp.path().join(UPDATE_VALIDATE_FAILED_FILE).exists());
        assert!(!temp.path().join(BOOTLOADER_UPDATED_FILE).exists());
    }

    #[test]
    fn test_lookup_uid_and_gid() {
        let uid = current_uid();
        let gid = current_gid();
        let rootfs = make_fake_rootfs(uid, gid);

        assert_eq!(lookup_uid(rootfs.path(), ODS_USER).unwrap(), uid);
        assert_eq!(lookup_gid(rootfs.path(), ODS_GROUP).unwrap(), gid);
    }

    #[test]
    fn test_lookup_uid_missing_user() {
        let rootfs = TempDir::new().unwrap();
        fs::create_dir_all(rootfs.path().join("etc")).unwrap();
        fs::write(
            rootfs.path().join("etc/passwd"),
            "root:x:0:0::/root:/bin/sh\n",
        )
        .unwrap();

        assert!(lookup_uid(rootfs.path(), ODS_USER).is_err());
    }

    #[test]
    fn test_lookup_gid_missing_group() {
        let rootfs = TempDir::new().unwrap();
        fs::create_dir_all(rootfs.path().join("etc")).unwrap();
        fs::write(rootfs.path().join("etc/group"), "root:x:0:\n").unwrap();

        assert!(lookup_gid(rootfs.path(), ODS_GROUP).is_err());
    }

    #[test]
    fn test_create_ods_runtime_files_end_to_end() {
        let uid = current_uid();
        let gid = current_gid();
        let rootfs = make_fake_rootfs(uid, gid);
        let ods_dir = TempDir::new().unwrap();

        let mut status = OdsStatus::new();
        status.record_fsck_result(
            PartitionName::Boot,
            FsckExitCode::CORRECTED,
            "errors corrected".to_string(),
        );

        let mut bl =
            crate::bootloader::create_mock_bootloader().with_env(BootEnvKey::ValidateUpdate, "1");

        create_ods_runtime_files(&status, Some(&mut bl), rootfs.path(), ods_dir.path()).unwrap();

        // Status JSON written and non-empty
        let status_file = ods_dir.path().join(ODS_STATUS_FILE);
        assert!(status_file.exists());
        let content = fs::read_to_string(&status_file).unwrap();
        assert!(content.contains("\"boot\""));

        // Update validation trigger created
        assert!(ods_dir.path().join(UPDATE_VALIDATE_FILE).exists());

        // No bootloader-updated marker
        assert!(!ods_dir.path().join(BOOTLOADER_UPDATED_FILE).exists());

        // Remaining modes from the contract on create_ods_runtime_files
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(ods_dir.path()), 0o775);
        assert_eq!(mode(&status_file), 0o600);
    }

    #[test]
    fn test_create_ods_runtime_files_no_bootloader() {
        let uid = current_uid();
        let gid = current_gid();
        let rootfs = make_fake_rootfs(uid, gid);
        let ods_dir = TempDir::new().unwrap();

        create_ods_runtime_files(&OdsStatus::new(), None, rootfs.path(), ods_dir.path()).unwrap();

        assert!(ods_dir.path().join(ODS_STATUS_FILE).exists());
        assert!(!ods_dir.path().join(UPDATE_VALIDATE_FILE).exists());
    }

    #[test]
    fn degraded_boot_serializes_only_when_set() {
        let status_normal = OdsStatus::new();
        let json_normal = serde_json::to_string(&status_normal).unwrap();
        assert!(
            !json_normal.contains("degraded_boot"),
            "degraded_boot must be absent when None; got: {json_normal}"
        );

        let mut status_degraded = OdsStatus::new();
        status_degraded.set_degraded_boot("grubenv missing".to_string());
        let json_degraded = serde_json::to_string(&status_degraded).unwrap();
        assert!(
            json_degraded.contains("\"degraded_boot\":{\"reason\":\"grubenv missing\"}"),
            "degraded_boot must be a nested object with reason; got: {json_degraded}"
        );
    }

    #[test]
    fn resize_status_absent_on_clean_run() {
        let status = OdsStatus::new();
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("resize_data"),
            "resize_data must be absent when None; got: {json}"
        );
    }

    #[test]
    fn resize_status_present_with_outcome_and_reason() {
        let mut status = OdsStatus::new();
        status.set_resize_status(ResizeStatus {
            outcome: ResizeOutcome::SkippedFsck,
            reason: "data partition fsck reported uncorrected errors".to_string(),
        });
        let json = serde_json::to_string(&status).unwrap();
        assert!(
            json.contains("\"resize_data\""),
            "resize_data must be present when Some; got: {json}"
        );
        assert!(
            json.contains("\"outcome\":\"skipped_fsck\""),
            "outcome must serialize as snake_case; got: {json}"
        );
        assert!(
            json.contains("\"reason\""),
            "reason must be present; got: {json}"
        );
    }

    #[test]
    fn resize_outcome_variants_serialize_snake_case() {
        let cases: &[(ResizeOutcome, &str)] = &[
            (ResizeOutcome::SkippedFsck, "\"skipped_fsck\""),
            (ResizeOutcome::ToolError, "\"tool_error\""),
            (ResizeOutcome::InvalidLayout, "\"invalid_layout\""),
        ];
        for (variant, expected) in cases {
            let s = serde_json::to_string(variant).unwrap();
            assert_eq!(&s, expected, "{variant:?} must serialize as {expected}");
        }
    }

    #[test]
    fn first_boot_always_serialized() {
        // Plain bool, always in the JSON. Absence of the key would itself
        // be diagnostic of a bug.
        let s = OdsStatus::new();
        let j = serde_json::to_string(&s).unwrap();
        assert!(
            j.contains("\"first_boot\":false"),
            "default first_boot must be false and serialized; got: {j}"
        );

        let mut s = OdsStatus::new();
        s.first_boot = true;
        let j = serde_json::to_string(&s).unwrap();
        assert!(
            j.contains("\"first_boot\":true"),
            "first_boot=true must be serialized; got: {j}"
        );
    }
}
