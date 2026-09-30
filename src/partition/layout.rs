//! Partition layout
//!
//! Builds the partition map using the partition table type selected at build
//! time via the `gpt` or `dos` Cargo feature.  There is no runtime detection:
//! the table type is a fixed property of the Yocto image.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use crate::error::PartitionError;
use crate::partition::RootDevice;

/// Typed partition identifier.
///
/// Used as the key in `PartitionLayout.partitions`. Call `as_str()` to get
/// the canonical string form for bootloader env writes, ODS JSON, and symlink names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartitionName {
    Boot,
    RootA,
    RootB,
    RootCurrent,
    Factory,
    Cert,
    Etc,
    Data,
    #[cfg(feature = "dos")]
    Extended,
}

impl PartitionName {
    /// The canonical string form of this partition name.
    ///
    /// Used for symlink names, ODS JSON keys, and bootloader env keys.
    /// Returns `&'static str` — suitable for syscall and wire boundaries.
    pub const fn as_str(self) -> &'static str {
        match self {
            PartitionName::Boot => "boot",
            PartitionName::RootA => "rootA",
            PartitionName::RootB => "rootB",
            PartitionName::RootCurrent => "rootCurrent",
            PartitionName::Factory => "factory",
            PartitionName::Cert => "cert",
            PartitionName::Etc => "etc",
            PartitionName::Data => "data",
            #[cfg(feature = "dos")]
            PartitionName::Extended => "extended",
        }
    }
}

impl fmt::Display for PartitionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for PartitionName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl serde::Serialize for PartitionName {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Partition layout for a block device
#[derive(Debug, Clone)]
pub struct PartitionLayout {
    /// Map of partition name to device path
    pub partitions: HashMap<PartitionName, PathBuf>,
    /// The root device
    pub device: RootDevice,
}

impl PartitionLayout {
    /// Builds the partition map for the given root device.
    ///
    /// Partition numbering is selected at compile time via the `gpt` or `dos` feature.
    pub fn new(device: RootDevice) -> crate::partition::Result<Self> {
        let partitions = build_partition_map(&device)?;
        Ok(Self { partitions, device })
    }

    /// Get the device path for a named partition
    pub fn get(&self, name: PartitionName) -> Option<&PathBuf> {
        self.partitions.get(&name)
    }

    /// Check if current root is rootA (partition 2)
    fn is_root_a(&self) -> bool {
        partition_suffix(&self.device.root_partition) == Some(PARTITION_NUM_ROOT_A)
    }

    /// Get the current root partition path (rootA or rootB based on boot).
    ///
    /// Falls back to reconstructing the path from the device if the map entry
    /// is absent (indicates incomplete initialisation — should not occur in normal boot).
    pub fn root_current(&self) -> PathBuf {
        if self.is_root_a() {
            self.partitions
                .get(&PartitionName::RootA)
                .cloned()
                .unwrap_or_else(|| {
                    log::warn!("rootA not in partition map; reconstructing path");
                    self.device.partition_path(PARTITION_NUM_ROOT_A)
                })
        } else {
            self.partitions
                .get(&PartitionName::RootB)
                .cloned()
                .unwrap_or_else(|| {
                    log::warn!("rootB not in partition map; reconstructing path");
                    self.device.partition_path(PARTITION_NUM_ROOT_B)
                })
        }
    }
}

/// Partition numbers shared by both GPT and DOS layouts
pub(crate) const PARTITION_NUM_BOOT: u32 = 1;
pub(crate) const PARTITION_NUM_ROOT_A: u32 = 2;
pub(crate) const PARTITION_NUM_ROOT_B: u32 = 3;

/// GPT layout — partitions 4-7 are all primary
#[cfg(feature = "gpt")]
pub(crate) const PARTITION_NUM_FACTORY: u32 = 4;
#[cfg(feature = "gpt")]
pub(crate) const PARTITION_NUM_CERT: u32 = 5;
#[cfg(feature = "gpt")]
pub(crate) const PARTITION_NUM_ETC: u32 = 6;
#[cfg(feature = "gpt")]
pub(crate) const PARTITION_NUM_DATA: u32 = 7;

/// DOS layout — partition 4 is the extended container; logical partitions start at 5
#[cfg(feature = "dos")]
pub(crate) const PARTITION_NUM_EXTENDED: u32 = 4;
#[cfg(feature = "dos")]
pub(crate) const PARTITION_NUM_FACTORY: u32 = 5;
#[cfg(feature = "dos")]
pub(crate) const PARTITION_NUM_CERT: u32 = 6;
#[cfg(feature = "dos")]
pub(crate) const PARTITION_NUM_ETC: u32 = 7;
#[cfg(feature = "dos")]
pub(crate) const PARTITION_NUM_DATA: u32 = 8;

/// Parse the trailing numeric partition suffix from a device path.
///
/// Examples: `sda2` → Some(2), `mmcblk0p3` → Some(3), `nvme0n1p12` → Some(12).
/// Returns None if the path has no file name or no trailing digit suffix.
///
/// Uses the *trailing* digit run, not the first digit found, so that devices
/// like `mmcblk0p2` (which contain digits in the base name) are handled correctly.
pub(crate) fn partition_suffix(path: &std::path::Path) -> Option<u32> {
    let s = path.file_name().and_then(|s| s.to_str())?;
    let digit_start = s
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    s[digit_start..].parse().ok()
}

/// Build the partition map for the given device.
///
/// Partition numbering is selected at compile time via the `gpt` or `dos` feature.
/// Exactly one of `gpt` or `dos` must be enabled; build.rs enforces this.
fn build_partition_map(
    device: &RootDevice,
) -> crate::partition::Result<HashMap<PartitionName, PathBuf>> {
    let mut partitions = HashMap::new();

    partitions.insert(
        PartitionName::Boot,
        device.partition_path(PARTITION_NUM_BOOT),
    );
    partitions.insert(
        PartitionName::RootA,
        device.partition_path(PARTITION_NUM_ROOT_A),
    );
    partitions.insert(
        PartitionName::RootB,
        device.partition_path(PARTITION_NUM_ROOT_B),
    );

    let root_current = match partition_suffix(&device.root_partition) {
        Some(n) if n == PARTITION_NUM_ROOT_A => device.partition_path(PARTITION_NUM_ROOT_A),
        Some(n) if n == PARTITION_NUM_ROOT_B => device.partition_path(PARTITION_NUM_ROOT_B),
        _ => {
            return Err(PartitionError::UnknownRootPartition {
                path: device.root_partition.clone(),
            });
        }
    };
    partitions.insert(PartitionName::RootCurrent, root_current);

    #[cfg(feature = "dos")]
    partitions.insert(
        PartitionName::Extended,
        device.partition_path(PARTITION_NUM_EXTENDED),
    );

    partitions.insert(
        PartitionName::Factory,
        device.partition_path(PARTITION_NUM_FACTORY),
    );
    partitions.insert(
        PartitionName::Cert,
        device.partition_path(PARTITION_NUM_CERT),
    );
    partitions.insert(PartitionName::Etc, device.partition_path(PARTITION_NUM_ETC));
    partitions.insert(
        PartitionName::Data,
        device.partition_path(PARTITION_NUM_DATA),
    );

    Ok(partitions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sda_root_a() -> RootDevice {
        RootDevice {
            base: PathBuf::from("/dev/sda"),
            partition_sep: "",
            root_partition: PathBuf::from("/dev/sda2"),
        }
    }

    #[cfg(feature = "gpt")]
    fn nvme_root_a() -> RootDevice {
        RootDevice {
            base: PathBuf::from("/dev/nvme0n1"),
            partition_sep: "p",
            root_partition: PathBuf::from("/dev/nvme0n1p2"),
        }
    }

    fn mmc_root_b() -> RootDevice {
        RootDevice {
            base: PathBuf::from("/dev/mmcblk0"),
            partition_sep: "p",
            root_partition: PathBuf::from("/dev/mmcblk0p3"),
        }
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn test_partition_map_gpt_sata() {
        let map = build_partition_map(&sda_root_a()).unwrap();

        assert_eq!(
            map.get(&PartitionName::Boot),
            Some(&PathBuf::from("/dev/sda1"))
        );
        assert_eq!(
            map.get(&PartitionName::RootA),
            Some(&PathBuf::from("/dev/sda2"))
        );
        assert_eq!(
            map.get(&PartitionName::RootB),
            Some(&PathBuf::from("/dev/sda3"))
        );
        assert_eq!(
            map.get(&PartitionName::Factory),
            Some(&PathBuf::from("/dev/sda4"))
        );
        assert_eq!(
            map.get(&PartitionName::Cert),
            Some(&PathBuf::from("/dev/sda5"))
        );
        assert_eq!(
            map.get(&PartitionName::Etc),
            Some(&PathBuf::from("/dev/sda6"))
        );
        assert_eq!(
            map.get(&PartitionName::Data),
            Some(&PathBuf::from("/dev/sda7"))
        );
    }

    #[cfg(feature = "dos")]
    #[test]
    fn test_partition_map_dos_sata() {
        let map = build_partition_map(&sda_root_a()).unwrap();

        assert_eq!(
            map.get(&PartitionName::Boot),
            Some(&PathBuf::from("/dev/sda1"))
        );
        assert_eq!(
            map.get(&PartitionName::RootA),
            Some(&PathBuf::from("/dev/sda2"))
        );
        assert_eq!(
            map.get(&PartitionName::RootB),
            Some(&PathBuf::from("/dev/sda3"))
        );
        assert_eq!(
            map.get(&PartitionName::Extended),
            Some(&PathBuf::from("/dev/sda4"))
        );
        assert_eq!(
            map.get(&PartitionName::Factory),
            Some(&PathBuf::from("/dev/sda5"))
        );
        assert_eq!(
            map.get(&PartitionName::Cert),
            Some(&PathBuf::from("/dev/sda6"))
        );
        assert_eq!(
            map.get(&PartitionName::Etc),
            Some(&PathBuf::from("/dev/sda7"))
        );
        assert_eq!(
            map.get(&PartitionName::Data),
            Some(&PathBuf::from("/dev/sda8"))
        );
    }

    #[cfg(feature = "gpt")]
    #[test]
    fn test_partition_map_gpt_nvme() {
        let map = build_partition_map(&nvme_root_a()).unwrap();

        assert_eq!(
            map.get(&PartitionName::Boot),
            Some(&PathBuf::from("/dev/nvme0n1p1"))
        );
        assert_eq!(
            map.get(&PartitionName::RootA),
            Some(&PathBuf::from("/dev/nvme0n1p2"))
        );
        assert_eq!(
            map.get(&PartitionName::Data),
            Some(&PathBuf::from("/dev/nvme0n1p7"))
        );
    }

    #[cfg(feature = "dos")]
    #[test]
    fn test_partition_map_dos_mmc() {
        let map = build_partition_map(&mmc_root_b()).unwrap();

        assert_eq!(
            map.get(&PartitionName::Boot),
            Some(&PathBuf::from("/dev/mmcblk0p1"))
        );
        assert_eq!(
            map.get(&PartitionName::Data),
            Some(&PathBuf::from("/dev/mmcblk0p8"))
        );
    }

    #[test]
    fn test_root_current_root_a() {
        let device = sda_root_a();
        let layout = PartitionLayout::new(device).unwrap();
        assert_eq!(layout.root_current(), PathBuf::from("/dev/sda2"));
    }

    #[test]
    fn test_root_current_root_b() {
        let device = mmc_root_b();
        let layout = PartitionLayout::new(device).unwrap();
        assert_eq!(layout.root_current(), PathBuf::from("/dev/mmcblk0p3"));
    }

    #[test]
    fn test_partition_suffix() {
        assert_eq!(partition_suffix(&PathBuf::from("/dev/sda2")), Some(2));
        assert_eq!(partition_suffix(&PathBuf::from("/dev/mmcblk0p2")), Some(2));
        assert_eq!(partition_suffix(&PathBuf::from("/dev/mmcblk0p3")), Some(3));
        assert_eq!(
            partition_suffix(&PathBuf::from("/dev/nvme0n1p12")),
            Some(12)
        );
        assert_eq!(partition_suffix(&PathBuf::from("/dev/sda")), None);
    }

    #[test]
    fn test_partition_name_as_str() {
        assert_eq!(PartitionName::Boot.as_str(), "boot");
        assert_eq!(PartitionName::RootA.as_str(), "rootA");
        assert_eq!(PartitionName::RootB.as_str(), "rootB");
        assert_eq!(PartitionName::RootCurrent.as_str(), "rootCurrent");
        assert_eq!(PartitionName::Factory.as_str(), "factory");
        assert_eq!(PartitionName::Cert.as_str(), "cert");
        assert_eq!(PartitionName::Etc.as_str(), "etc");
        assert_eq!(PartitionName::Data.as_str(), "data");
    }

    #[test]
    fn test_partition_name_display() {
        assert_eq!(PartitionName::Boot.to_string(), "boot");
        assert_eq!(PartitionName::Data.to_string(), "data");
    }

    #[test]
    fn test_partition_layout_uses_typed_keys() {
        let device = RootDevice {
            base: std::path::PathBuf::from("/dev/sda"),
            partition_sep: "",
            root_partition: std::path::PathBuf::from("/dev/sda2"),
        };
        let layout = PartitionLayout::new(device).unwrap();
        assert!(layout.partitions.contains_key(&PartitionName::Boot));
        assert!(layout.partitions.contains_key(&PartitionName::Data));
    }

    #[test]
    fn partition_numbers_match_the_table_layout() {
        assert_eq!(PARTITION_NUM_BOOT, 1);
        assert_eq!(PARTITION_NUM_ROOT_A, 2);
        assert_eq!(PARTITION_NUM_ROOT_B, 3);
        #[cfg(feature = "gpt")]
        {
            assert_eq!(PARTITION_NUM_FACTORY, 4);
            assert_eq!(PARTITION_NUM_CERT, 5);
            assert_eq!(PARTITION_NUM_ETC, 6);
            assert_eq!(PARTITION_NUM_DATA, 7);
        }
        #[cfg(feature = "dos")]
        {
            assert_eq!(PARTITION_NUM_EXTENDED, 4);
            assert_eq!(PARTITION_NUM_FACTORY, 5);
            assert_eq!(PARTITION_NUM_CERT, 6);
            assert_eq!(PARTITION_NUM_ETC, 7);
            assert_eq!(PARTITION_NUM_DATA, 8);
        }
    }
}
