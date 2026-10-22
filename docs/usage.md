# omnect-os-init usage

This document describes what the initramfs does at run time and how to use it
on a device. Which features an image contains is decided at build time; see the
[meta-omnect README](https://github.com/omnect/meta-omnect#readme) for the
distro features, machine features and recipe variables.

Most functions are triggered by a bootloader environment variable. Set it on
the device with `bootloader_env.sh`, which needs root permissions, then reboot.

## Boot sequence

On every boot the init:

1. mounts the rootfs read-only and the boot partition, after a filesystem check
   of the boot partition;
2. reads the bootloader environment and selects the boot mode: a flash mode, a
   factory reset, or a normal boot;
3. checks and mounts the `factory`, `cert`, `etc` and `data` partitions, sets
   up the overlays for `/etc` and `/home`, and creates the
   [status file](#status-file) for omnect-device-service;
4. hands over to systemd with `switch_root`.

A flash mode replaces steps 3 and 4: it unmounts what it needs and ends with a
power off or a reboot.

## Error handling

A fatal error ends the boot in the initramfs:

- while an update is being validated (`omnect_validate_update` is set): reboot,
  so the bootloader falls back to the old partition. This needs the bootloader
  environment, so an error before it was read (e.g. the rootfs cannot be
  mounted) is handled as on any other boot;
- on a release image: the system halts and logs the error to the kernel log;
- on a developer image: a debug shell starts on the console.

A filesystem check that asks for a reboot always reboots.

## Flash modes

The flash modes write a whole disk image, with all partitions:

1. clone the disk the system runs from onto another disk;
2. flash the running disk with an image pushed in over `scp`;
3. flash the running disk with an image downloaded from a URL.

Flash mode 1 is built in by default. Modes 2 and 3 are enabled at build time.

The bootloader environment variable `flash-mode` selects the mode. A value
that selects no built-in mode is logged and ignored; the init then checks
`factory-reset`, or boots normally. All flash mode variables are deleted
before anything is written, so a failed run is not repeated on the next boot.
A refused or failed run ends in the [error handling](#error-handling).

On an EFI machine every flash mode deletes all EFI boot entries and adds one
entry `omnect_os` for the written disk.

Each run writes a log file `flash-mode-<n>.log` to the data partition. After a
normal boot it is found at `/mnt/data/flash-mode-<n>.log`.

### Flash mode 1

Flash mode 1 clones the running disk onto another disk, given in
`flash-mode-devpath`. Use the kernel device name, e.g. `/dev/mmcblk2` or
`/dev/sda`. udev does not run in the initramfs, so links such as
`/dev/disk/by-path/...` do not exist.

```sh
sudo -s
bootloader_env.sh set flash-mode 1
bootloader_env.sh set flash-mode-devpath /dev/mmcblk2
reboot
...
flash mode 1: cloning /dev/mmcblk1 onto /dev/mmcblk2
...
```

After you set the variables, reboot from the same disk, e.g. from USB again
when the clone was started from USB. On an EFI machine the new boot entry
points to the clone.

The clone starts like a newly flashed device:

- default bootloader environment;
- first boot condition;
- empty `etc` and `data` partitions. `data` gets the size set in the image
  build. It grows on the clone's first boot only if the image has
  `resize-data`.

The running rootfs is copied to rootfs A of the clone. The `boot`, `factory`
and `cert` partitions and the bootloader area are copied as they are. On a GPT
disk the clone's boot partition and rootfs A get new partition UUIDs
(PARTUUID).

The system powers off after a successful clone.

Flash mode 1 is refused before anything is written when:

- `flash-mode-devpath` is not set, is not a block device, or does not appear
  within 30 seconds;
- `flash-mode-devpath` is a partition, or the disk the system booted from;
- a build constant the clone needs is missing;
- `factory-reset` is set at the same time. Both variables are deleted; set the
  one you meant again.

The log is written to the data partition of the disk the system booted from,
on success and on failure. A conflict with a factory reset is refused before
the run starts and leaves no log file; its reason is only in the kernel log.

### Flash mode 2

Flash mode 2 waits for a bmap file and a `wic.xz` image over `scp` on `eth0`,
and flashes the image onto the running disk.

```sh
sudo -s
bootloader_env.sh set flash-mode 2
reboot
...
flash mode 2: flashing /dev/mmcblk0 with an image pushed in over scp
...
please run: scp -O <bmap-file> omnect@<target-ip>:wic.bmap
```

Then send the bmap file and the image from the host. OpenSSH 9.0 and later
needs the option `-O` (legacy protocol):

```sh
scp -O image.wic.bmap omnect@<target-ip>:wic.bmap
scp -O image.wic.xz omnect@<target-ip>:wic.xz
```

Log in as user `omnect`. The initramfs has its own `omnect` user; its password
is set at build time from the same setting as the rootfs user. A password
changed later in the rootfs does not change the initramfs password.

The network setup has time limits: `eth0` must come up within 60 seconds and
have an IPv4 address within 120 seconds after `dhcpcd` returns. Without a DHCP
server, `dhcpcd` sets a link-local address (169.254.x.x), which is used as
well. The wait for `wic.bmap` has no limit.

The bmap file is checked before the image is asked for: its checksum, its
format version, its block ranges, and that the image fits the disk.

By default the init unpacks the blocks the bmap file lists into RAM, checks
them against the bmap file, and only then writes the disk. An image build can select direct
flashing instead, for devices with too little RAM: the disk is then written
straight from the `scp` stream, and a broken image is seen only after the disk
was partly written.

If one attempt fails (bad bmap file, broken or cut-off image, image too large,
write error), the init logs the reason and asks for both files again. Keep
the device powered: after a failed write the disk may not boot any more.
Failures before the first request (e.g. a network timeout) or after the flash
end in the error handling.

The system reboots after a successful flash. The log is not written when the
disk was partly written or its partition table could not be read again.

An image build can also add the file `/etc/enforce_flash_mode` to the
initramfs. Then every boot starts flash mode 2. Only `flash-mode` `1`
overrides it. On a release image this also works when the bootloader
environment cannot be read.

### Flash mode 3

Flash mode 3 downloads a `wic.xz` image and its sha256 file over `eth0`, and
flashes the image onto the running disk. The URLs are stored base64-encoded.

```sh
sha256sum image.wic.xz > image.wic.sha256
```

```sh
sudo -s
bootloader_env.sh set flash-mode 3
bootloader_env.sh set flash-mode-url $(echo "http://url.to/image.wic.xz" | base64 -w 0 -)
bootloader_env.sh set flash-mode-url-sha256 $(echo "http://url.to/image.wic.sha256" | base64 -w 0 -)
reboot
...
flash mode 3: flashing /dev/mmcblk0 with an image downloaded from http://url.to/image.wic.xz
...
```

The device needs an address from a DHCP server; a link-local address is not
enough, because it has no route and no DNS server.

The init downloads the sha256 file and the image into RAM. The image must fit
into the available RAM minus 128 MiB. Before anything is written, the image is
checked against the sha256 file and against the disk size.

A download fails on an HTTP error, when the connection is not made within 30
seconds, or when it stays below 1 byte per second for 60 seconds. A redirect
from an HTTPS URL must lead to HTTPS again. The log hides the user name,
password, query and fragment of a URL, because they can contain an access
token.

The TLS certificate of an HTTPS URL is checked only on machines with a
reliable real-time clock (cargo feature `rtc`). Without it, the sha256 file
only detects a corrupted download, not a changed one.

The system reboots after a successful flash. The log is not written when the write to
the disk failed or the partition table could not be read again.

### Flash mode and factory reset together

- Flash mode 1 and `factory-reset`: refused (see [flash mode 1](#flash-mode-1)).
- Flash mode 2 or 3 and `factory-reset`: the flash mode runs. The factory reset
  does not run and is logged as an error.

## Factory reset

A factory reset re-creates the filesystems of the `etc` and `data` partitions.
Set the bootloader environment variable `factory-reset` to a JSON object:

```sh
sudo bootloader_env.sh set factory-reset '{"mode": 1, "preserve": ["network", "firewall", "certificates", "applications"]}'
sudo reboot
```

- `mode` selects how the old data is removed (see below).
- `preserve` is mandatory. It lists topics whose files are kept; an empty array
  keeps nothing but `/etc/omnect/factory-reset.d/`.

The variable is deleted at the start of the reset. If the `factory` partition
has a directory `etc`, its content is copied to the new `etc` partition.

| Mode | Wipe                                     | Remark                                    |
| ---- | ---------------------------------------- | ----------------------------------------- |
| 1    | none; only the filesystems are re-created | fast, no privacy                          |
| 2    | overwrite `etc` and `data` with random data | better privacy, slow                   |
| 3    | discard all blocks of `etc` and `data`    | fast; the disk has to support discard     |

The wipe modes do not guarantee full privacy. That needs a hardware function of
the disk, e.g. ATA secure erase.

The reset runs these steps: back up the preserved files to RAM, wipe, create
the filesystems, restore the preserved files. Then the device boots normally.
A failed reset never stops the boot; the result is reported in the
[status file](#status-file).

- A failure before the wipe (e.g. in the backup) stops the reset; `etc` and
  `data` stay as they were.
- A failed wipe does not stop the reset: the filesystems are re-created and the
  files restored, so the device stays usable. The result is status 2 with the
  reason in `error`.
- A failed restore of one path does not stop the restore of the others.
- If `etc` or `data` cannot be mounted after the filesystems were re-created,
  the reason is also stored in the bootloader environment variable
  `omnect_factory_reset_last_error` (`<partition>:<reason>`), because the
  following boot may halt on the same partition.

### Preserve topics

The topics in `preserve` are the keys of `/etc/omnect/factory-reset.json`; each
key holds an array of paths. The topic `applications` adds the paths of every
`*.json` file in `/etc/omnect/factory-reset.d/`:

```json
{
  "paths": [
    "/home/omnect/.bash_history",
    "/home/root/.bash_history"
  ]
}
```

- Each file needs a `paths` array (it may be empty). A file without it is a
  configuration error and the reset does not run, so no data is lost.
- A path that does not exist is logged and skipped; the reset still succeeds.
- A path must be on the `etc` or `data` partition: directly, through the
  overlays of `/etc` and `/home`, or through the bind mounts `/var/lib`,
  `/usr/local` (and `/var/log` with `persistent-var-log`). A path elsewhere
  fails the restore.
- A path with a `..` component is a configuration error.

### Factory reset result

The result is the object `factory_reset` in the [status file](#status-file):

```sh
jq '.factory_reset' /run/omnect-device-service/omnect-os-initramfs.json
```

```json
{
  "status": 0,
  "paths": [
    "/etc/omnect/factory-reset.d/",
    "/etc/restore_file",
    "/home/omnect/.bash_history",
    "/home/root/.bash_history"
  ],
  "data_wiped": true
}
```

Error, where `preserve` names a key that `/etc/omnect/factory-reset.json` does
not define:

```json
{
  "status": 3,
  "error": "Missing field in factory-reset config: /rootfs/etc/omnect/factory-reset.json: no 'network' key",
  "data_wiped": false
}
```

- `status`:
  - `0`: success
  - `1`: the request is unusable: invalid JSON, or no supported `mode`
  - `2`: a step failed; see `error`, and `data_wiped` for whether data was
    changed
  - `3`: configuration error, e.g. `preserve` missing, a topic without a
    definition, or a path with `..`; see `error`
  - `4`: success, but a filesystem had to be created twice; see `context`
- `error`: the reason of the failure; absent when nothing failed.
- `context`: more detail on a warning or error; absent when empty.
- `paths`: the configured paths to preserve, not the ones actually restored;
  absent when the reset stopped before it changed `etc` and `data`.
- `data_wiped`: `true` once the reset started to change `etc` and `data`.
  With status 2, `false` means nothing was changed; `true` means data may
  already be lost.

An unusable trigger (status 1 or 3) does not start a reset. It is reported
once, so the request does not repeat on every boot.

## Filesystem check

The partitions are checked with `fsck -y`, so all repair questions are
answered with yes. `boot` is checked on every boot; `factory`, `cert`, `etc`
and `data` on every boot except in a flash mode. The rootfs partitions are not
checked.

When fsck replayed the journal of an ext4 partition and exited with 0, it runs
a second time with `fsck -y -f`, because the replay can leave damage that only
a full check finds. The exit code of the second run is the result; the output
holds both runs. This full check runs after every unclean shutdown and takes
longer on large partitions.

- Exit code 0 or 1 (no errors, or errors corrected): the boot continues.
- Exit code with the "reboot required" bit (2): the system reboots.
- Other errors: logged; the boot continues.

If a check was not clean, its result is kept until the
[status file](#status-file) is written, also over a reboot, and then deleted:

- U-Boot: in the bootloader environment variable `omnect_fsck_<partition>`
  (gzip and base64);
- GRUB: `boot` in `omnect_fsck_boot`, the other partitions in files
  `fsck.<partition>` on the boot partition. Output that is too large is
  replaced by a short note.

Nothing is kept when the bootloader environment cannot be read. The full
output is also written to `/mnt/data/var/log/fsck/<partition>.log` when the
data partition is mounted and fsck printed output.

## Update validation

After an A/B update the bootloader sets flags in its environment. The init
turns them into files in `/run/omnect-device-service/` for
omnect-device-service, which does the validation. This is skipped when the
bootloader environment cannot be read. See the
[omnect-device-service docs](https://github.com/omnect/omnect-device-service/blob/main/src/twin/firmware_update/update_validation.md).

| Bootloader variable             | File                            | Variable deleted |
| ------------------------------- | ------------------------------- | ---------------- |
| `omnect_validate_update`        | `omnect_validate_update`        | no               |
| `omnect_validate_update_failed` | `omnect_validate_update_failed` | yes              |
| `omnect_bootloader_updated`     | `omnect_bootloader_updated`     | yes              |

Both validate flags set at once is a fatal error.

## Status file

The init writes `/run/omnect-device-service/omnect-os-initramfs.json` for
omnect-device-service. Only `first_boot` is always present; the other keys
appear when there is something to report.

```json
{
  "fsck": {
    "boot": {
      "code": 1,
      "output": "fsck from util-linux 2.37.4\nfsck.fat 4.2 (2021-01-31)\nDirty bit is set. Fs was not properly unmounted and some data may be corrupt.\n Automatically removing dirty bit.\n\n*** Filesystem was changed ***\nWriting changes.\n/dev/mmcblk0p1: 75 files, 11264/20431 clusters"
    }
  },
  "first_boot": false
}
```

| Key              | Present when                                   | Content |
| ---------------- | ---------------------------------------------- | ------- |
| `first_boot`     | always                                         | `true` on the first boot after flashing |
| `fsck`           | a [filesystem check](#filesystem-check) was not clean | one object per partition: `code` (fsck exit code), `output` |
| `factory_reset`  | a [factory reset](#factory-reset-result) was requested | see there |
| `degraded_boot`  | a release image booted without a usable bootloader environment | `reason` |
| `resize_data`    | the data partition could not be resized on the first boot | `outcome` (`skipped_fsck`, `tool_error`, `invalid_layout`), `reason` |
| `extra_bootargs` | the extra boot arguments could not be synced    | `outcome` (`file_read_failed`, `read_failed`, `set_env_failed`, `read_back_failed`, `read_back_mismatch`), `reason` |

## Extra boot arguments

On the first boot after flashing, the init reads the files
`omnect_extra_bootargs_omnect` and `omnect_extra_bootargs_custom` from the boot
partition and stores them in the bootloader variable `omnect_extra_bootargs`.
If the value changed, the system reboots once so the bootloader uses it. This
does not run while an update is being validated.

## Early module load

A driver that probes in the initramfs finds another module, for example a DMA
engine, only when that module is already loaded. udev loads modules only after
`switch_root`. With the `early-modules` feature, the init loads the modules
listed in the rootfs after the first-boot steps (extra boot arguments, data
resize), before the other partitions are mounted. A flash mode loads none.

To add a module, install a `.conf` file into `/etc/omnect/early-modules.d/` in
the rootfs image and build the init with `early-modules`. The files are read
before the `/etc` overlay is mounted, so a file added on the running device is
not read:

```
# <module> [parameters]
imx_sdma
my_driver debug=1 mode=fast
```

- Each line names one module, followed by optional parameters in the format
  `insmod` takes. A listed module gets its parameters only from its line.
- `-` and `_` in a module name are equal.
- Empty lines and lines that start with `#` are skipped.
- The files are read in name order. A module listed again is ignored with a
  warning.
- Without a `.conf` file the step does nothing.

The step:

1. reads the kernel release from `/proc/sys/kernel/osrelease`, and
   `modules.dep` and `modules.builtin` from `/lib/modules/<release>/` in the
   rootfs;
2. sets `/sys/module/firmware_class/parameters/path` to `/rootfs/lib/firmware`,
   so a firmware request during the load finds the file in the rootfs. The
   path is not restored;
3. for each listed module, in file and line order: skips it when it is built
   in or already loaded; else loads its dependencies from `modules.dep` and
   then the module itself with `finit_module`. A dependency that has its own
   line gets its parameters.

Needs:

- `modules.dep` and `modules.builtin` (`depmod`), the module files and their
  firmware in the rootfs.
- `CONFIG_MODULE_DECOMPRESS` in the kernel for a compressed module
  (`.ko.xz`, `.ko.zst`, `.ko.gz`).
- `CONFIG_FW_LOADER=y`. With `=m`, the `firmware_class` parameter file does
  not exist in the initramfs, and no module is loaded.
- A `/lib` link in the rootfs must be relative (`lib -> usr/lib`). An absolute
  link resolves into the initramfs, so `modules.dep` and the firmware are not
  found.

Not supported: the init reads only `modules.dep` and `modules.builtin`. So
`modprobe.d` (`options`, `alias`, `softdep`, `blacklist`) and
`<module>.<param>=` on the kernel command line do not apply, and a line must
name the module, not an alias.

The step is best effort. It never stops the boot and does not change the
status reported to omnect-device-service. Each problem is logged as a warning
with its reason. When a listed module did not load, the step ends with one
error line `early modules: not loaded: <names>`. udev may still load the
module later, but a driver that started earlier does not use it.

With `CONFIG_FW_LOADER_USER_HELPER_FALLBACK=y`, a firmware file missing in the
rootfs can block the load for the sysfs fallback timeout (60 s by default).

To check the result on a device:

```bash
dmesg | grep "early modules"
cat /sys/module/firmware_class/parameters/path   # /rootfs/lib/firmware
cat /sys/module/<module>/initstate               # live
```
