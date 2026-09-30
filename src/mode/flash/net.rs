//! Network setup for the flash modes that receive their image over the
//! network: bring the interface up, get an address by DHCP, start dropbear.

// TODO: remove me, as soon as flash mode 2 calls it
#![allow(dead_code)]

use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use nix::ifaddrs::getifaddrs;

use crate::error::FlashError;
use crate::filesystem::{FsType, MountOptions, MountPoint, is_path_mounted, mount};
use crate::mode::flash::CHILD_PATH;

const FLASH_INTERFACE: &str = "eth0";
const IP_CMD: &str = "/sbin/ip";
const DHCPCD_CMD: &str = "/sbin/dhcpcd";
const DROPBEAR_CMD: &str = "/sbin/dropbear";
const DROPBEAR_GENERATE_HOSTKEY_FLAG: &str = "-R";
const DROPBEAR_KEY_DIR: &str = "/etc/dropbear";
const DEVPTS_MOUNT_POINT: &str = "/dev/pts";
const TMP_DIR: &str = "/tmp";
const INTERFACE_UP_TIMEOUT: Duration = Duration::from_secs(60);
const DHCP_ADDRESS_TIMEOUT: Duration = Duration::from_secs(120);
const NET_POLL_INTERVAL: Duration = Duration::from_secs(1);

const IP_LINK_ARGS: [&str; 4] = ["link", "set", FLASH_INTERFACE, "up"];

/// The side effects of the network setup, so a test can pin their order.
pub(crate) trait NetOps {
    /// `ip link set eth0 up`; an error means "try again later".
    fn link_up(&mut self) -> Result<(), FlashError>;
    /// `dhcpcd eth0`; returns once dhcpcd has gone to the background.
    fn run_dhcpcd(&mut self) -> Result<(), FlashError>;
    /// Every interface address as `(interface name, address)`.
    fn addresses(&mut self) -> Result<Vec<(String, Option<IpAddr>)>, FlashError>;
    fn sleep(&mut self, duration: Duration);
}

pub(crate) struct RealNetOps;

fn child(cmd: &str) -> Command {
    let mut command = Command::new(cmd);
    command.env("PATH", CHILD_PATH);
    command
}

fn run_visible(cmd: &str, args: &[&str]) -> Result<(), FlashError> {
    let status = child(cmd)
        .args(args)
        .status()
        .map_err(|e| FlashError::NetworkFailed(format!("failed to run {cmd}: {e}")))?;
    if !status.success() {
        return Err(FlashError::NetworkFailed(format!(
            "{cmd} {args:?} failed ({status})"
        )));
    }
    Ok(())
}

impl NetOps for RealNetOps {
    fn link_up(&mut self) -> Result<(), FlashError> {
        run_visible(IP_CMD, &IP_LINK_ARGS)
    }

    fn run_dhcpcd(&mut self) -> Result<(), FlashError> {
        fs::create_dir_all(TMP_DIR).map_err(|source| FlashError::PathIo {
            path: TMP_DIR.into(),
            source,
        })?;
        run_visible(DHCPCD_CMD, &[FLASH_INTERFACE])
    }

    fn addresses(&mut self) -> Result<Vec<(String, Option<IpAddr>)>, FlashError> {
        let addrs = getifaddrs()
            .map_err(|e| FlashError::NetworkFailed(format!("failed to list addresses: {e}")))?;
        Ok(addrs
            .map(|entry| {
                let ip = entry.address.as_ref().and_then(|a| {
                    a.as_sockaddr_in()
                        .map(|v4| IpAddr::V4(v4.ip()))
                        .or_else(|| a.as_sockaddr_in6().map(|v6| IpAddr::V6(v6.ip())))
                });
                (entry.interface_name, ip)
            })
            .collect())
    }

    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// The first IPv4 address of `iface`.
pub(crate) fn ipv4_of(addrs: &[(String, Option<IpAddr>)], iface: &str) -> Option<Ipv4Addr> {
    addrs.iter().find_map(|(name, ip)| match ip {
        Some(IpAddr::V4(v4)) if name == iface => Some(*v4),
        _ => None,
    })
}

/// The bound is the sum of the sleeps, not wall time.
fn wait_for<T>(
    ops: &mut dyn NetOps,
    what: &str,
    timeout: Duration,
    interval: Duration,
    mut attempt: impl FnMut(&mut dyn NetOps) -> Result<Option<T>, FlashError>,
) -> Result<T, FlashError> {
    log::info!("waiting for {what} (up to {}s)", timeout.as_secs());
    let mut waited = Duration::ZERO;
    loop {
        let last_error = match attempt(&mut *ops) {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => String::new(),
            Err(e) => format!(": {e}"),
        };
        if waited >= timeout {
            return Err(FlashError::NetworkFailed(format!(
                "timed out after {}s waiting for {what}{last_error}",
                timeout.as_secs()
            )));
        }
        log::info!("still waiting for {what}{last_error}");
        ops.sleep(interval);
        waited += interval;
    }
}

fn bring_up_with(
    ops: &mut dyn NetOps,
    up_timeout: Duration,
    address_timeout: Duration,
    interval: Duration,
) -> Result<Ipv4Addr, FlashError> {
    wait_for(
        ops,
        &format!("{FLASH_INTERFACE} to come up"),
        up_timeout,
        interval,
        |ops| ops.link_up().map(Some),
    )?;
    ops.run_dhcpcd()?;
    wait_for(
        ops,
        &format!("an IPv4 address on {FLASH_INTERFACE}"),
        address_timeout,
        interval,
        |ops| Ok(ipv4_of(&ops.addresses()?, FLASH_INTERFACE)),
    )
}

/// Bring `eth0` up, get an address by DHCP and return it.
pub(crate) fn bring_up() -> Result<Ipv4Addr, FlashError> {
    let ip = bring_up_with(
        &mut RealNetOps,
        INTERFACE_UP_TIMEOUT,
        DHCP_ADDRESS_TIMEOUT,
        NET_POLL_INTERVAL,
    )?;
    log::info!("{FLASH_INTERFACE} has address {ip}");
    Ok(ip)
}

/// Start dropbear, which generates a host key on first use and daemonizes.
pub(crate) fn start_dropbear() -> Result<(), FlashError> {
    let pts = Path::new(DEVPTS_MOUNT_POINT);
    fs::create_dir_all(pts).map_err(|source| FlashError::PathIo {
        path: pts.to_path_buf(),
        source,
    })?;
    if !is_path_mounted(pts)? {
        mount(MountPoint::new(
            FsType::Devpts.as_str(),
            pts,
            MountOptions::devpts(),
        ))?;
    }
    fs::create_dir_all(DROPBEAR_KEY_DIR).map_err(|source| FlashError::PathIo {
        path: DROPBEAR_KEY_DIR.into(),
        source,
    })?;
    run_visible(DROPBEAR_CMD, &[DROPBEAR_GENERATE_HOSTKEY_FLAG])
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO: Duration = Duration::ZERO;
    const STEP: Duration = Duration::from_secs(1);

    fn addr(name: &str, ip: Option<&str>) -> (String, Option<IpAddr>) {
        (name.to_string(), ip.map(|s| s.parse().unwrap()))
    }

    #[test]
    fn ipv4_of_returns_the_address_of_the_interface() {
        let addrs = [
            addr("lo", Some("127.0.0.1")),
            addr("eth0", Some("fe80::1")),
            addr("eth0", Some("192.168.1.5")),
        ];
        assert_eq!(ipv4_of(&addrs, "eth0"), Some(Ipv4Addr::new(192, 168, 1, 5)));
    }

    #[test]
    fn ipv4_of_is_none_for_an_ipv6_only_interface() {
        let addrs = [addr("eth0", Some("fe80::1")), addr("eth0", None)];
        assert_eq!(ipv4_of(&addrs, "eth0"), None);
    }

    #[test]
    fn ipv4_of_is_none_when_only_lo_has_an_address() {
        let addrs = [addr("lo", Some("127.0.0.1"))];
        assert_eq!(ipv4_of(&addrs, "eth0"), None);
    }

    /// Records every side effect as one line. `link_failures` link attempts
    /// fail first; the address shows up after `address_misses` empty polls.
    #[derive(Default)]
    struct FakeNetOps {
        calls: Vec<String>,
        link_failures: usize,
        address_misses: usize,
    }

    impl NetOps for FakeNetOps {
        fn link_up(&mut self) -> Result<(), FlashError> {
            self.calls.push("link up".to_string());
            if self.link_failures > 0 {
                self.link_failures -= 1;
                return Err(FlashError::NetworkFailed("no such device".to_string()));
            }
            Ok(())
        }

        fn run_dhcpcd(&mut self) -> Result<(), FlashError> {
            self.calls.push("dhcpcd".to_string());
            Ok(())
        }

        fn addresses(&mut self) -> Result<Vec<(String, Option<IpAddr>)>, FlashError> {
            self.calls.push("addresses".to_string());
            if self.address_misses > 0 {
                self.address_misses -= 1;
                return Ok(vec![addr("lo", Some("127.0.0.1"))]);
            }
            Ok(vec![addr("eth0", Some("10.0.0.7"))])
        }

        fn sleep(&mut self, _duration: Duration) {
            self.calls.push("sleep".to_string());
        }
    }

    #[test]
    fn link_up_is_retried_then_dhcpcd_runs_once_then_the_address_is_polled() {
        let mut ops = FakeNetOps {
            link_failures: 2,
            address_misses: 1,
            ..Default::default()
        };
        let ip = bring_up_with(&mut ops, STEP * 5, STEP * 5, STEP).unwrap();
        assert_eq!(ip, Ipv4Addr::new(10, 0, 0, 7));
        assert_eq!(
            ops.calls,
            [
                "link up",
                "sleep",
                "link up",
                "sleep",
                "link up",
                "dhcpcd",
                "addresses",
                "sleep",
                "addresses",
            ]
        );
    }

    #[test]
    fn a_link_that_never_comes_up_fails_after_the_bound() {
        let mut ops = FakeNetOps {
            link_failures: usize::MAX,
            ..Default::default()
        };
        let err = bring_up_with(&mut ops, STEP * 2, STEP, STEP).unwrap_err();
        assert!(matches!(err, FlashError::NetworkFailed(_)), "{err}");
        assert_eq!(
            ops.calls,
            ["link up", "sleep", "link up", "sleep", "link up"]
        );
    }

    #[test]
    fn no_address_fails_after_the_bound_with_dhcpcd_run_once() {
        let mut ops = FakeNetOps {
            address_misses: usize::MAX,
            ..Default::default()
        };
        let err = bring_up_with(&mut ops, ZERO, STEP, STEP).unwrap_err();
        assert!(matches!(err, FlashError::NetworkFailed(_)), "{err}");
        assert_eq!(ops.calls.iter().filter(|c| *c == "dhcpcd").count(), 1);
        assert_eq!(ops.calls.iter().filter(|c| *c == "addresses").count(), 2);
    }

    #[test]
    fn children_get_the_explicit_path() {
        let command = child("/bin/true");
        let path = command
            .get_envs()
            .find(|(k, _)| *k == "PATH")
            .and_then(|(_, v)| v);
        assert_eq!(path, Some(std::ffi::OsStr::new(CHILD_PATH)));
    }
}
