//! Network setup for the flash modes that receive their image over the
//! network: bring the interface up, get an address by DHCP, start dropbear.

use std::ffi::OsStr;
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::thread;
use std::time::Duration;

use nix::ifaddrs::getifaddrs;

use crate::error::FlashError;
use crate::filesystem::{FsType, MountOptions, MountPoint, is_path_mounted, mount};
use crate::mode::flash::run_inherited;

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
/// A "still waiting" line per poll would flood kmsg and the run log.
const NET_WAIT_LOG_INTERVAL: Duration = Duration::from_secs(10);

const IP_LINK_ARGS: [&str; 4] = ["link", "set", FLASH_INTERFACE, "up"];

trait NetOps {
    /// An error means "try again later".
    fn link_up(&mut self) -> Result<(), FlashError>;
    /// Returns once dhcpcd has daemonized; the address is polled afterwards.
    fn run_dhcpcd(&mut self) -> Result<(), FlashError>;
    fn addresses(&mut self) -> Result<Vec<(String, Option<IpAddr>)>, FlashError>;
    fn sleep(&mut self, duration: Duration);
}

struct RealNetOps;

fn run_visible(cmd: &str, args: &[&str]) -> Result<(), FlashError> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    run_inherited(cmd, &args).map_err(FlashError::NetworkFailed)
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

fn ipv4_of(addrs: &[(String, Option<IpAddr>)], iface: &str) -> Option<Ipv4Addr> {
    addrs.iter().find_map(|(name, ip)| match ip {
        Some(IpAddr::V4(v4)) if name == iface => Some(*v4),
        _ => None,
    })
}

/// The bound is the sum of the sleeps. Progress is logged every
/// `NET_WAIT_LOG_INTERVAL`, and at once when the reason for the wait changes.
fn wait_for<T>(
    ops: &mut dyn NetOps,
    what: &str,
    timeout: Duration,
    interval: Duration,
    mut attempt: impl FnMut(&mut dyn NetOps) -> Result<Option<T>, FlashError>,
) -> Result<T, FlashError> {
    log::info!("waiting for {what} (up to {}s)", timeout.as_secs());
    let mut waited = Duration::ZERO;
    let mut next_log = NET_WAIT_LOG_INTERVAL;
    let mut logged_error = String::new();
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
        if waited >= next_log || last_error != logged_error {
            log::info!("still waiting for {what}{last_error}");
            next_log = waited + NET_WAIT_LOG_INTERVAL;
            logged_error = last_error;
        }
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

    /// The log capture is global, so a test that logs "still waiting" must not
    /// run while another one captures.
    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        crate::logging::capture::SERIALIZE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Records every side effect as one line. `link_failures` link attempts
    /// fail first; the address shows up after `address_errors` failed and
    /// `address_misses` empty polls.
    #[derive(Default)]
    struct FakeNetOps {
        calls: Vec<String>,
        link_failures: usize,
        address_errors: usize,
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
            if self.address_errors > 0 {
                self.address_errors -= 1;
                return Err(FlashError::NetworkFailed("netlink busy".to_string()));
            }
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
        let _guard = serialized();
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
    fn a_failed_address_lookup_is_retried() {
        let _guard = serialized();
        let mut ops = FakeNetOps {
            address_errors: 2,
            ..Default::default()
        };
        let ip = bring_up_with(&mut ops, STEP * 5, STEP * 5, STEP).unwrap();
        assert_eq!(ip, Ipv4Addr::new(10, 0, 0, 7));
        assert_eq!(ops.calls.iter().filter(|c| *c == "sleep").count(), 2);
    }

    #[test]
    fn a_link_that_never_comes_up_fails_after_the_bound() {
        let _guard = serialized();
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
        let _guard = serialized();
        let mut ops = FakeNetOps {
            address_misses: usize::MAX,
            ..Default::default()
        };
        let err = bring_up_with(&mut ops, ZERO, STEP, STEP).unwrap_err();
        assert!(matches!(err, FlashError::NetworkFailed(_)), "{err}");
        assert_eq!(ops.calls.iter().filter(|c| *c == "dhcpcd").count(), 1);
        assert_eq!(ops.calls.iter().filter(|c| *c == "addresses").count(), 2);
    }

    fn still_waiting_lines(ops: &mut FakeNetOps, timeout: Duration) -> Vec<String> {
        let _guard = serialized();
        crate::logging::capture::install_test_logger();
        crate::logging::start_capture();
        bring_up_with(ops, timeout, timeout, STEP).unwrap_err();
        crate::logging::take_capture()
            .into_iter()
            .filter(|line| line.contains("still waiting"))
            .collect()
    }

    #[test]
    fn a_long_wait_logs_its_progress_once_per_log_interval() {
        let mut ops = FakeNetOps {
            address_misses: usize::MAX,
            ..Default::default()
        };
        let polls = 25;
        let lines = still_waiting_lines(&mut ops, STEP * polls);
        let expected = (STEP * polls).as_secs() / NET_WAIT_LOG_INTERVAL.as_secs();
        assert_eq!(lines.len() as u64, expected, "{lines:?}");
    }

    #[test]
    fn a_wait_logs_its_reason_at_once() {
        let mut ops = FakeNetOps {
            link_failures: usize::MAX,
            ..Default::default()
        };
        let lines = still_waiting_lines(&mut ops, STEP * 3);
        assert!(
            lines
                .first()
                .is_some_and(|line| line.contains("no such device")),
            "{lines:?}"
        );
        assert_eq!(
            lines.len(),
            1,
            "an unchanged reason is not repeated: {lines:?}"
        );
    }
}
