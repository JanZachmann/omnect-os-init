//! Filesystem link creation from configuration
//!
//! Creates symbolic links based on fs-link configuration files.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{InitramfsError, Result};

/// Main fs-link config file.
const FS_LINK_CONFIG_PATH: &str = "etc/omnect/fs-link.json";

/// Drop-in config directory; its `*.json` files are merged with the main config.
const FS_LINK_CONFIG_PATH_D: &str = "etc/omnect/fs-link.d";

/// One-time link list, deleted after its links are created.
const FS_LINK_LIST_PATH: &str = "etc/omnect/create-fs-links.txt";

#[derive(Debug, Clone, Deserialize)]
pub struct FsLinkConfig {
    pub links: Vec<LinkEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LinkEntry {
    /// Target of the symlink (what it points to)
    pub target: String,
    /// Path where the symlink is created
    pub link: String,
}

pub fn create_fs_links(rootfs_dir: &Path) -> Result<()> {
    let config = load_fs_link_config(rootfs_dir)?;

    for entry in &config.links {
        create_link(rootfs_dir, entry)?;
    }

    if !config.links.is_empty() {
        log::info!("Created {} fs-links", config.links.len());
    }

    create_links_from_list(rootfs_dir)
}

fn create_links_from_list(rootfs_dir: &Path) -> Result<()> {
    let list_path = rootfs_dir.join(FS_LINK_LIST_PATH);
    if !list_path.exists() {
        return Ok(());
    }

    let entries = parse_link_list(&fs::read_to_string(&list_path)?)?;
    for entry in &entries {
        create_link(rootfs_dir, entry)?;
    }
    fs::remove_file(&list_path)?;

    log::info!(
        "Created {} fs-links from {}",
        entries.len(),
        list_path.display()
    );

    Ok(())
}

fn parse_link_list(content: &str) -> Result<Vec<LinkEntry>> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.split_whitespace();
            match (fields.next(), fields.next()) {
                (Some(target), Some(link)) => Ok(LinkEntry {
                    target: target.to_string(),
                    // The list may name the link as an absolute path.
                    link: link.trim_start_matches('/').to_string(),
                }),
                _ => Err(InitramfsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{FS_LINK_LIST_PATH}: expected \"<target> <link>\", got \"{line}\""),
                ))),
            }
        })
        .collect()
}

fn load_fs_link_config(rootfs_dir: &Path) -> Result<FsLinkConfig> {
    let mut all_links = Vec::new();

    let main_config_path = rootfs_dir.join(FS_LINK_CONFIG_PATH);
    if main_config_path.exists() {
        let config = load_config_file(&main_config_path)?;
        all_links.extend(config.links);
    }

    let config_d_path = rootfs_dir.join(FS_LINK_CONFIG_PATH_D);
    if config_d_path.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(&config_d_path)?
            .filter_map(|e| match e {
                Ok(entry) => Some(entry),
                Err(err) => {
                    log::warn!("Failed to read entry in {}: {err}", config_d_path.display());
                    None
                }
            })
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
            .collect();

        // Sort for deterministic order
        entries.sort_by_key(|e| e.path());

        for entry in entries {
            let config = load_config_file(&entry.path())?;
            all_links.extend(config.links);
        }
    }

    Ok(FsLinkConfig { links: all_links })
}

fn load_config_file(path: &Path) -> Result<FsLinkConfig> {
    let content = fs::read_to_string(path)?;
    let config: FsLinkConfig = serde_json::from_str(&content).map_err(|e| {
        InitramfsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Failed to parse fs-link config {}: {}", path.display(), e),
        ))
    })?;

    log::debug!(
        "Loaded {} links from {}",
        config.links.len(),
        path.display()
    );

    Ok(config)
}

/// Validate that a config-supplied path is a safe relative path.
///
/// Rejects absolute paths (Path::join would silently discard rootfs_dir) and
/// paths containing `..` components (directory traversal outside rootfs).
fn validate_relative_path(path: &str) -> Result<()> {
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(InitramfsError::Io(std::io::Error::other(format!(
            "fs-link path must be relative, got absolute path: {path}"
        ))));
    }
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err(InitramfsError::Io(std::io::Error::other(format!(
            "fs-link path must not contain '..': {path}"
        ))));
    }
    Ok(())
}

/// `entry.target` is used as given, so a link may point anywhere, e.g. to
/// `/dev/null`. Only `entry.link` must stay inside `rootfs_dir`.
fn create_link(rootfs_dir: &Path, entry: &LinkEntry) -> Result<()> {
    validate_relative_path(&entry.link)?;
    let link_path = rootfs_dir.join(&entry.link);
    let target = PathBuf::from(&entry.target);

    if let Some(parent) = link_path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
    }

    if link_path.exists() || link_path.is_symlink() {
        // A plain directory cannot be replaced by a symlink — flag it clearly.
        // Symlinks pointing to directories are fine and must be replaceable.
        if !link_path.is_symlink() && link_path.is_dir() {
            return Err(InitramfsError::Io(std::io::Error::other(format!(
                "Cannot replace directory with symlink: {}",
                link_path.display()
            ))));
        }
        fs::remove_file(&link_path).map_err(|e| {
            InitramfsError::Io(std::io::Error::other(format!(
                "Failed to remove existing file {}: {}",
                link_path.display(),
                e
            )))
        })?;
    }

    symlink(&target, &link_path).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to create symlink {} -> {}: {}",
            link_path.display(),
            target.display(),
            e
        )))
    })?;

    log::debug!(
        "Created symlink: {} -> {}",
        link_path.display(),
        target.display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_link_entry_deserialize() {
        let json = r#"{"target": "/data/app", "link": "opt/app"}"#;
        let entry: LinkEntry = serde_json::from_str(json).unwrap();

        assert_eq!(entry.target, "/data/app");
        assert_eq!(entry.link, "opt/app");
    }

    #[test]
    fn test_fs_link_config_deserialize() {
        let json = r#"{
            "links": [
                {"target": "/data/app", "link": "opt/app"},
                {"target": "/data/config", "link": "etc/app"}
            ]
        }"#;

        let config: FsLinkConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.links.len(), 2);
    }

    #[test]
    fn test_create_link() {
        let temp = TempDir::new().unwrap();
        let target_dir = temp.path().join("target");
        fs::create_dir_all(&target_dir).unwrap();

        let entry = LinkEntry {
            target: target_dir.to_string_lossy().to_string(),
            link: "link".to_string(),
        };

        create_link(temp.path(), &entry).unwrap();

        let link_path = temp.path().join("link");
        assert!(link_path.is_symlink());
        assert_eq!(fs::read_link(&link_path).unwrap(), target_dir);
    }

    #[test]
    fn test_create_link_replaces_existing() {
        let temp = TempDir::new().unwrap();
        let target1 = temp.path().join("target1");
        let target2 = temp.path().join("target2");
        fs::create_dir_all(&target1).unwrap();
        fs::create_dir_all(&target2).unwrap();

        let link_path = temp.path().join("link");

        // Create first link
        let entry1 = LinkEntry {
            target: target1.to_string_lossy().to_string(),
            link: "link".to_string(),
        };
        create_link(temp.path(), &entry1).unwrap();

        // Replace with second link
        let entry2 = LinkEntry {
            target: target2.to_string_lossy().to_string(),
            link: "link".to_string(),
        };
        create_link(temp.path(), &entry2).unwrap();

        assert_eq!(fs::read_link(&link_path).unwrap(), target2);
    }

    #[test]
    fn test_create_link_nested_path() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        fs::create_dir_all(&target).unwrap();

        let entry = LinkEntry {
            target: target.to_string_lossy().to_string(),
            link: "nested/deep/link".to_string(),
        };

        create_link(temp.path(), &entry).unwrap();

        let link_path = temp.path().join("nested/deep/link");
        assert!(link_path.is_symlink());
    }

    #[test]
    fn test_validate_relative_path_absolute_rejected() {
        assert!(validate_relative_path("/etc/foo").is_err());
        assert!(validate_relative_path("/").is_err());
    }

    #[test]
    fn test_validate_relative_path_dotdot_rejected() {
        assert!(validate_relative_path("../foo").is_err());
        assert!(validate_relative_path("foo/../../bar").is_err());
        assert!(validate_relative_path("..").is_err());
    }

    #[test]
    fn test_validate_relative_path_valid() {
        assert!(validate_relative_path("foo/bar").is_ok());
        assert!(validate_relative_path("opt/app").is_ok());
        assert!(validate_relative_path("etc/omnect/config.json").is_ok());
    }

    #[test]
    fn test_validate_relative_path_single_dot_allowed() {
        // "." contains no ".." component so it passes
        assert!(validate_relative_path("./foo").is_ok());
        assert!(validate_relative_path("foo/./bar").is_ok());
    }

    #[test]
    fn test_validate_relative_path_empty_string_allowed() {
        // empty string is relative and contains no ".." — passes validation
        assert!(validate_relative_path("").is_ok());
    }

    #[test]
    fn test_load_empty_config() {
        let temp = TempDir::new().unwrap();
        let config = load_fs_link_config(temp.path()).unwrap();
        assert!(config.links.is_empty());
    }

    #[test]
    fn test_parse_link_list() {
        let entries = parse_link_list(
            "# comment\n\n/etc/systemd/system/app.service /etc/systemd/system/multi-user.target.wants/app.service\n  /data/app opt/app  extra\n",
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].target, "/etc/systemd/system/app.service");
        assert_eq!(
            entries[0].link,
            "etc/systemd/system/multi-user.target.wants/app.service"
        );
        assert_eq!(entries[1].target, "/data/app");
        assert_eq!(entries[1].link, "opt/app");
    }

    #[test]
    fn test_parse_link_list_rejects_single_field() {
        assert!(parse_link_list("/data/app\n").is_err());
    }

    #[test]
    fn test_create_fs_links_from_list_removes_list() {
        let temp = TempDir::new().unwrap();
        let list = temp.path().join(FS_LINK_LIST_PATH);
        fs::create_dir_all(list.parent().unwrap()).unwrap();
        fs::write(&list, "/data/app /opt/app\n").unwrap();

        create_fs_links(temp.path()).unwrap();

        assert_eq!(
            fs::read_link(temp.path().join("opt/app")).unwrap(),
            PathBuf::from("/data/app")
        );
        assert!(!list.exists());
    }

    #[test]
    fn test_create_fs_links_from_list_rejects_dotdot() {
        let temp = TempDir::new().unwrap();
        let list = temp.path().join(FS_LINK_LIST_PATH);
        fs::create_dir_all(list.parent().unwrap()).unwrap();
        fs::write(&list, "/data/app ../escape\n").unwrap();

        assert!(create_fs_links(temp.path()).is_err());
        assert!(list.exists());
    }

    #[test]
    fn test_load_config_file() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("config.json");

        let json = r#"{"links": [{"target": "/data", "link": "opt"}]}"#;
        fs::write(&config_path, json).unwrap();

        let config = load_config_file(&config_path).unwrap();
        assert_eq!(config.links.len(), 1);
    }
}
