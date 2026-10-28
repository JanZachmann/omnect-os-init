//! Common types for bootloader implementations

use std::io::Write as _;
use std::process::{Command, Stdio};

use crate::filesystem::FsckExitCode;

const GZIP_CMD: &str = "/bin/gzip";
const GUNZIP_CMD: &str = "/bin/gunzip";
/// busybox base64 applet lives under /bin, not /usr/bin
const BASE64_CMD: &str = "/bin/base64";

/// Encode fsck result for storage in the bootloader environment.
///
/// Produces `base64(gzip("{code}\n{output}"))` using the busybox `gzip` and
/// `base64` applets in the initramfs. The format is internal, but
/// [`unpack_fsck_text`] and [`parse_fsck_text`] have to change with it.
///
/// Returns an empty string if either applet fails, so the record does not
/// survive a reboot. This boot's copy stays
/// in `OdsStatus`, because `apply_boot_env_decision` does not clear it, so the
/// ODS JSON still carries it. `/data/log/fsck/<partition>.log` does not
/// close the gap on the `FsckRequiresReboot` path, where the data partition is
/// not mounted.
pub fn encode_fsck_output(code: i32, output: &str) -> String {
    let raw = format!("{code}\n{output}");

    // Pipe raw text through `gzip -c` to get compressed bytes.
    let gzip_result = (|| -> std::io::Result<Vec<u8>> {
        let mut gzip = Command::new(GZIP_CMD)
            .args(["-c"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let mut stdin = gzip
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no gzip stdin"))?;
        stdin.write_all(raw.as_bytes())?;
        // Drop stdin to close the pipe; gzip won't flush output until EOF on its input.
        // Without this, wait_with_output() deadlocks if gzip output fills the OS pipe buffer.
        drop(stdin);

        let out = gzip.wait_with_output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "gzip exited with status {}",
                out.status
            )));
        }
        Ok(out.stdout)
    })();

    let compressed = match gzip_result {
        Ok(c) => c,
        Err(e) => {
            log::warn!("encode_fsck_output: gzip failed: {e}");
            return String::new();
        }
    };

    // Pipe compressed bytes through `base64 -w 0` (no line wrapping).
    let base64_result = (|| -> std::io::Result<String> {
        let mut b64 = Command::new(BASE64_CMD)
            .args(["-w", "0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let mut stdin = b64
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no base64 stdin"))?;
        stdin.write_all(&compressed)?;
        drop(stdin);

        let out = b64.wait_with_output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "base64 exited with status {}",
                out.status
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    })();

    match base64_result {
        Ok(s) => s,
        Err(e) => {
            log::warn!("encode_fsck_output: base64 failed: {e}");
            String::new()
        }
    }
}

/// Decode a fsck result previously encoded with [`encode_fsck_output`].
///
/// Returns a `FsckRecord` on success, or `None` if decoding fails.
#[cfg(any(feature = "grub", test))]
pub fn decode_fsck_output(encoded: &str) -> Option<super::FsckRecord> {
    parse_fsck_text(&unpack_fsck_text(encoded).ok().flatten()?)
}

/// Undo base64 and gzip. `Ok(None)` if the value does not unpack, `Err` if an
/// applet cannot run.
pub(crate) fn unpack_fsck_text(encoded: &str) -> std::io::Result<Option<String>> {
    // Decode base64 → compressed bytes.
    let b64_out = Command::new(BASE64_CMD)
        .args(["-d"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("no stdin"))?;
            stdin.write_all(encoded.as_bytes())?;
            drop(stdin);
            child.wait_with_output()
        })?;
    if !b64_out.status.success() {
        return Ok(None);
    }

    // Decompress gzip → raw text.
    let gz_out = Command::new(GUNZIP_CMD)
        .args(["-c"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("no stdin"))?;
            stdin.write_all(&b64_out.stdout)?;
            drop(stdin);
            child.wait_with_output()
        })?;
    if !gz_out.status.success() {
        return Ok(None);
    }

    Ok(Some(String::from_utf8_lossy(&gz_out.stdout).into_owned()))
}

pub(crate) fn parse_fsck_text(raw: &str) -> Option<super::FsckRecord> {
    let (code_str, output) = raw.split_once('\n')?;
    let code = code_str.trim().parse::<i32>().ok()?;
    Some(super::FsckRecord {
        exit_code: FsckExitCode::from(code),
        output: output.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns true if all required external commands are available at their
    /// expected initramfs paths. Tests that invoke subprocesses are skipped
    /// on developer/CI machines where these may live elsewhere (e.g. /usr/bin).
    fn commands_available() -> bool {
        [GZIP_CMD, GUNZIP_CMD, BASE64_CMD]
            .iter()
            .all(|cmd| std::path::Path::new(cmd).exists())
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        assert!(
            commands_available(),
            "required commands not found at initramfs paths ({}, {}, {}); \
             ensure gzip, gunzip and base64 are installed",
            GZIP_CMD,
            GUNZIP_CMD,
            BASE64_CMD
        );
        let code = 1;
        let output = "Pass 1: Checking inodes, blocks, and sizes\nErrors corrected.";
        let encoded = encode_fsck_output(code, output);
        assert!(!encoded.is_empty(), "encoding should succeed");
        let record = decode_fsck_output(&encoded).unwrap();
        assert_eq!(
            record.exit_code,
            crate::filesystem::FsckExitCode::from(code)
        );
        assert_eq!(record.output, output);
    }

    /// Has only the env; the fsck methods are the trait defaults.
    struct EnvOnly(Option<String>);

    impl crate::bootloader::BootEnv for EnvOnly {
        fn get_env(
            &self,
            _key: crate::bootloader::BootEnvKey,
        ) -> crate::bootloader::Result<Option<String>> {
            Ok(self.0.clone())
        }

        fn set_env(
            &mut self,
            _key: crate::bootloader::BootEnvKey,
            value: Option<&str>,
        ) -> crate::bootloader::Result<()> {
            self.0 = value.map(str::to_string);
            Ok(())
        }
    }

    fn read_fsck_env(
        value: &str,
    ) -> crate::bootloader::Result<Option<crate::bootloader::FsckRecord>> {
        use crate::bootloader::BootEnv;
        EnvOnly(Some(value.to_string())).get_fsck_status(crate::partition::PartitionName::Data)
    }

    #[test]
    fn get_fsck_status_decodes_the_env_value() {
        assert!(commands_available());
        let encoded = encode_fsck_output(1, "Errors corrected.");

        let record = read_fsck_env(&encoded).unwrap().unwrap();

        assert_eq!(record.exit_code, FsckExitCode::from(1));
        assert_eq!(record.output, "Errors corrected.");
    }

    #[test]
    fn get_fsck_status_rejects_a_record_without_exit_code() {
        assert!(commands_available());
        let output = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!("printf 'Pass 1\\nErrors corrected.' | {GZIP_CMD} -c | {BASE64_CMD} -w0"),
            ])
            .output()
            .unwrap();
        let encoded = String::from_utf8(output.stdout).unwrap();

        assert!(matches!(
            read_fsck_env(&encoded),
            Err(crate::error::BootEnvError::InvalidValue { .. })
        ));
    }

    #[test]
    fn get_fsck_status_rejects_a_value_that_does_not_unpack() {
        assert!(commands_available());
        for value in ["not-valid-base64!!!", ""] {
            assert!(
                matches!(
                    read_fsck_env(value),
                    Err(crate::error::BootEnvError::InvalidValue { .. })
                ),
                "{value:?}"
            );
        }
    }

    #[test]
    fn test_decode_invalid_returns_none() {
        assert!(decode_fsck_output("not-valid-base64!!!").is_none());
        assert!(decode_fsck_output("").is_none());
    }
}
