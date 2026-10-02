# Flash Modes 1, 2, 3 — Design

**Status:** Approved

Port the three flash modes from the legacy scripted initramfs
(`meta-omnect/recipes-omnect/initrdscripts/omnect-os-initramfs/flash-mode-{1,2,3}`)
to the Rust initramfs.

**Every item in [§10 Decisions required from
reviewers](#10-decisions-required-from-reviewers) is decided; the rest of the
spec follows those decisions.**

## 1. Overview

A flash mode deploys a whole disk image from the initramfs, before any rootfs is
handed control. The mode is selected through the bootloader environment and runs
at most once — the trigger is cleared before the work starts.

| Mode | What it does | Network | Gating |
|---|---|---|---|
| 1 | Clones the running disk onto another block device | no | on by default |
| 2 | Flashes a `wic.xz` pushed in over `scp` onto the running disk | yes | opt-in |
| 3 | Flashes a `wic.xz` downloaded from a URL onto the running disk | yes | opt-in |

On the destination disk mode 1 writes the default bootloader environment,
reformats `etc` and `data` to enforce the first-boot condition, and gives the
copied partitions fresh UUIDs. None of that touches the running disk.

Implementation order is **1 → 2 → 3**. Mode 2 comes before mode 3 because it
is the mode used in the development cycle, even though its interactive `scp`
wait is the hardest part to port and to test.

### 1.1 Fidelity policy

Observable behaviour is preserved: the same environment keys, the same terminal
actions, the same platform workarounds. The exceptions are all deliberate and
all recorded in [§9](#9-intentional-deviations-from-the-legacy-scripts):

- three legacy bugs are fixed;
- machine-driven unbounded waits become bounded; the wait for the operator's
  `scp` stays unbounded (§10.8);
- `dd` is replaced by in-process file I/O.

Everything else that looks odd is carried over, because it was added for observed
field failures. The items where that judgment is worth re-examining are collected
in §10 rather than decided here.

## 2. Architecture

### 2.1 Environment contract

Keys are hyphenated with no `omnect_` prefix, matching the existing
`factory-reset` key.

| Key | Modes | Format |
|---|---|---|
| `flash-mode` | 1, 2, 3 | `"1"` / `"2"` / `"3"` |
| `flash-mode-devpath` | 1 | plain device path, e.g. `/dev/mmcblk2` |
| `flash-mode-url` | 3 | base64 of the image URL |
| `flash-mode-url-sha256` | 3 | base64 of the sha256-file URL |

Both legacy bootloader backends return a bare value — `uboot-sh` uses
`fw_printenv -n`, `grub-sh` pipes `grub-editenv list` through `cut -d'=' -f2`.
The extra `cut -d= -f2` that `flash-mode-1` applies to `flash-mode-devpath` is
therefore dead code and is not ported.

U-Boot writeable-variable whitelist: `flash-mode:dw` and
`flash-mode-devpath:sw` are already in the base list in
`recipes-bsp/u-boot/u-boot/omnect_env.h`. The two URL keys are appended by
`kas/feature/flash-mode-3.yaml` through `OMNECT_UBOOT_WRITEABLE_ENV_FLAGS`.

### 2.2 Clear-trigger-first invariant

Every mode clears `flash-mode` before doing any work. Mode 3 additionally clears
each URL key immediately after reading it. A crash or power loss mid-flash then
leads to a normal boot attempt, never to an endless re-entry into flash mode.
This matches the factory-reset precedent.

### 2.3 Dispatch point

Flash-mode detection happens right after the bootloader environment is opened,
and `init_setup` is skipped when a flash mode is active.

Before this port, `run_init` ran: mount core partitions → open boot env →
`init_setup` (extra-bootargs sync, then resize-data) → `BootMode::detect` →
dispatch.

`init_setup` acts on the running disk. Running it before a flash mode is wrong
for a different reason per mode:

- modes 2 and 3 overwrite the running disk, so resizing its data partition is
  work that the flash discards seconds later;
- mode 1 writes a different disk, so the running data partition survives. The
  resize is not discarded, it is simply pointless here: the clone gets its
  partition table from the rewritten dump, which resets the data partition to
  its shipped size (§4.2), so any growth on the source is not carried over.
  Mode 1 then creates and formats the destination data partition itself
  (§4.1 step 9).

In all three modes an extra-bootargs reboot would additionally delay the
flash.

Relative to the legacy scripts this is partly a match and partly a deviation:

- **matches** — legacy ran the flash modes at `init.d/87`, ahead of `resize-data`
  (88) and `fs-mount` (89);
- **deviates** — the Rust initramfs mounts the rootfs at `/rootfs` and the boot
  partition at `/rootfs/boot` in `mount_core_partitions` before dispatch, on both
  bootloaders. Legacy mounted the boot partition on demand for environment access
  only (GRUB), and `fs-mount` (89) ran after the flash modes, so the rootfs was
  never mounted while a flash mode ran. This matters for mode 1, which images the
  running rootfs: every mode therefore unmounts `/rootfs` completely before
  writing anything (§4.1 step 5, §5.1). Nothing in any mode needs `/rootfs` —
  no mode reaches `switch_root`, and `grubenv.in` and `uboot-env.bin` live in the
  initramfs at `/etc/omnect/`;
- **matches** — the extra-bootargs sync is skipped for flash modes. Legacy has
  the same effect by placement: its sync lives in `setup_etc_from_factory` in
  `common-sh`, reached from `fs-mount` (89), so a flash mode at 87 ends in
  poweroff or reboot before it is ever called. Both sides also gate the sync on
  the first boot. The Rust flow has to skip it explicitly only because
  `init_setup` sits ahead of dispatch rather than behind it.

### 2.4 Naming

`mode` would otherwise mean three different things. Pinned as:

- `BootMode::Flash(FlashConfig)` — the new dispatch variant;
- `FlashMode::{Mode1, Mode2, Mode3}` — numeric, following the existing
  `ResetMode::Mode1` and the operator-facing documentation;
- `ResetMode` stays the factory-reset wipe mode.

### 2.5 Module layout

```
src/mode/flash/
  mod.rs        dispatch, terminal action, log capture and persistence   flash-mode
  config.rs     environment read, validation -> FlashConfig     (pure)   flash-mode
  efi.rs        efibootmgr handling                                      grub
  clone.rs      mode 1 orchestration                                     flash-mode-1
  sfdisk.rs     partition-table dump parsing and rewriting      (pure)   flash-mode-1
  rawio.rs      in-process replacement for every `dd` call               flash-mode, per item
  unmount.rs    rootfs unmount, /proc/mounts sweep                       flash-mode
  net.rs        interface up, dhcpcd, dropbear                           flash-mode-2/3
  bmap.rs       bmaptool wrapper                                         flash-mode-2/3
  scp.rs        mode 2 orchestration                                     flash-mode-2
  url.rs        mode 3 orchestration                                     flash-mode-3
```

The right column is the gating feature (§3.6). In `rawio.rs` each item is gated
on the modes that use it: `copy_range` on `flash-mode-1`, `zero_range` on
`flash-mode-2`, the shared constants on `flash-mode`. The device-number helpers
(`block_devnum`, `whole_disk_devnum`) are in `partition/device.rs`, gated on
`flash-mode`, and shared by the mode 1 refusal and the §5.1 sweep.

External tools are invoked through `std::process::Command` with named `const`
paths, as `filesystem/reformat.rs` already does. No new command-runner
abstraction, and no `gpt`/libparted crate.

### 2.6 Source of truth for existing types

- `RootDevice::partition_path(u32)` builds a partition path and handles the
  `p`-suffix difference between `/dev/sda2` and `/dev/mmcblk1p2`.
- The feature-gated `PARTITION_NUM_*` constants in `src/partition/layout.rs`
  carry the GPT-versus-DOS index difference.

Together these replace the legacy hardcoded indices (`etc`/`data` at 6/7 for
GPT, 7/8 for DOS), the explicit `1..8` block-device checks, and the
`if [ ! -b "${blk}1" ]; then p="p"; fi` suffix probe.

### 2.7 Build-time constants

Mode 1 needs no new mechanism. `build.rs` already emits all five values it
requires and documents them as "Used by flash-mode-1":

| Yocto variable | Constant | Unit |
|---|---|---|
| `OMNECT_PART_OFFSET_UBOOT_ENV1` | `UBOOT_ENV1_START` | KB |
| `OMNECT_PART_OFFSET_UBOOT_ENV2` | `UBOOT_ENV2_START` | KB |
| `OMNECT_PART_SIZE_UBOOT_ENV` | `UBOOT_ENV_SIZE` | KB |
| `OMNECT_PART_SIZE_DATA` | `DATA_SIZE` | KB |
| `BOOTLOADER_SEEK` | `BOOTLOADER_START` | KB |

`omnect_conv_size_param` in `meta-omnect/classes/omnect_fw_env_config.bbclass`
multiplies the U-Boot environment size and offsets by 1024 before writing
`fw_env.config`, which is what fixes their unit as KB. `DATA_SIZE` and
`BOOTLOADER_START` are KB for the same reason legacy treats them that way: the
legacy `flash-mode-1` script comments `DATA_SIZE` as "initial size of data
partition (in KB)" and computes byte offsets from `BOOTLOADER_START` as
`BOOTLOADER_START*1024`. `UBOOT_ENV1_START` is
also required whenever `BOOTLOADER_START` is set, on either bootloader, since
the bootloader-area copy length is `UBOOT_ENV1_START - BOOTLOADER_START`
(§4.1 step 2, step 8).

Mode 2 adds three through the same mechanism:

| Yocto variable | Constant | Unit |
|---|---|---|
| `OMNECT_PART_OFFSET_BOOT` | `BOOT_START` | KB |
| `OMNECT_PART_SIZE_BOOT` | `BOOT_SIZE` | KB |
| `OMNECT_USER_ID` | `OMNECT_USER_ID` | uid and gid |

All three are `Option<u64>`, like the existing five; mode 2 fails with a missing
build constant before it touches anything when one is absent. The zeroed head is
`BOOT_START + BOOT_SIZE` KB, the value legacy computes with `bc` into
`DD_ZERO_SIZE`; mode 2 sums it with `checked_add` in a tested function.
`OMNECT_USER_ID` is the fixed id `omnect_user.bbclass` gives both the `omnect`
user and its group, so the FIFO owner is known at build time and `/etc/passwd`
is not read.

`OMNECT_FLASH_MODE_2_DIRECT_FLASHING` is a switch, not a value, so it becomes
the cargo feature `flash-mode-2-direct` (§3.6). The recipe enables it when the
variable is `1`, matching the legacy
`oe.utils.conditional('OMNECT_FLASH_MODE_2_DIRECT_FLASHING', '1', 'true', 'false')`.

`UBOOT_ENV2_START` stays `Option<u64>` rather than required: it is set per
machine and absent where no second environment bank is reserved, which is
exactly the condition for skipping the second write (§10.7).

### 2.8 External tools and in-process equivalents

Paths verified against `buildhistory` for a built `omnect-os-initramfs`
(`raspberrypi4_64`, U-Boot, `flash-mode-2` and `flash-mode-3` both enabled). The
image is usrmerged — `/bin -> usr/bin` and `/sbin -> usr/sbin` — so the
`/sbin/...` form the code uses resolves correctly. The table lists the resolved
path.

| Tool | Resolved path | Package | Modes |
|---|---|---|---|
| `sfdisk` | `/usr/sbin/sfdisk` | `util-linux-sfdisk` | 1 |
| `e2image` | `/usr/sbin/e2image` | `e2fsprogs` | 1 |
| `mkfs.ext4` | `/usr/sbin/mkfs.ext4` | `e2fsprogs-mke2fs` | 1 |
| `tune2fs` | `/usr/sbin/tune2fs` | `e2fsprogs-tune2fs` | 1 |
| `bmaptool` | `/usr/bin/bmaptool` | `bmaptool` | 2, 3 |
| `curl` | `/usr/bin/curl` | `curl` | 3 |
| `dhcpcd` | `/usr/sbin/dhcpcd` | `dhcpcd` | 2, 3 |
| `dropbear` | `/usr/sbin/dropbear` | `dropbear` | 2 |
| `efibootmgr` | `/usr/sbin/efibootmgr` | `efibootmgr` | 1, 2, 3, EFI machines only |
| `ip` | `/usr/sbin/ip` | busybox applet | 2, 3 |
| `xz` | `/usr/bin/xz` | `xz`, run by `bmaptool` | 2, 3 |

`efibootmgr` is absent from the verified image, which has no `efi` in
`MACHINE_FEATURES` — consistent with the recipe gating and with §6 applying only
on EFI machines.

The verified image is the one built with the legacy scripts. The Rust
initramfs recipe names only the `e2fsprogs` sub-packages it needs for the other
boot paths, so it has to install `e2fsprogs` (for `e2image`) and, on EFI
machines, `efibootmgr` itself (§12).

The remaining tools stay external because no pure-Rust equivalent exists at a
dependency weight an initramfs can carry: `sfdisk` (partition tables),
`e2image`, `mkfs.ext4` and `tune2fs` (ext4), `bmaptool` (block maps),
`efibootmgr` (EFI variables), `curl`, `dhcpcd` and `dropbear`.

Seven operations the legacy scripts shell out for are done in-process instead.
Five use `nix`, which is already a dependency; `getifaddrs` needs its `net`
feature, the others are enabled already. `uuidgen` needs the new `uuid` crate;
`dd` needs nothing:

| Legacy | In-process |
|---|---|
| `uuidgen` | 16 bytes from `/dev/urandom` into `uuid::Builder::from_random_bytes` |
| `dd` | `std::io` read/write at an offset, with `COPY_BUFFER_SIZE` as the buffer |
| `mkfifo` | `nix::unistd::mkfifo` |
| `chown omnect:omnect` | `nix::unistd::chown` to `OMNECT_USER_ID` (§2.7) |
| `ip addr show` | `nix::ifaddrs::getifaddrs` |
| `sync` | `nix::unistd::sync` |
| `reboot -f` / `poweroff -f` | `nix::sys::reboot::reboot` with `RB_AUTOBOOT` / `RB_POWER_OFF` |

Every `dd` call in the three modes is a plain read and write at a byte offset —
the bootloader area copy, the `boot`, `factory` and `cert` partition copies, the
`uboot-env.bin` writes and the zeroing in mode 2 — so `File::seek` plus a
buffered copy covers all of them, followed by the explicit `sync` the legacy
scripts get from `dd` returning.

The reboot call follows the existing pattern in `handle_fatal_error`: it returns
`Result<Infallible>`, so the `Ok` arm is uninhabited and only the error path is
reachable.

## 3. Component changes

### 3.0 `build.rs`

Three more `rerun-if-env-changed` lines and three more generated constants for
mode 2 (§2.7): `BOOT_START`, `BOOT_SIZE` and `OMNECT_USER_ID`, all read with the
existing `read_u64_env`. The doc-comment table at the top of `build.rs` gains the
three rows.

### 3.1 `src/bootloader/mod.rs`

Four `BootEnvKey` variants, gated per mode:

```rust
#[cfg(feature = "flash-mode")]
/// `flash-mode` — mode selector set by the operator. Cleared by the initramfs
/// before the selected mode starts work.
FlashMode,
#[cfg(feature = "flash-mode-1")]
/// `flash-mode-devpath` — destination block device for mode 1.
FlashModeDevPath,
#[cfg(feature = "flash-mode-3")]
/// `flash-mode-url` — base64 image URL for mode 3.
FlashModeUrl,
#[cfg(feature = "flash-mode-3")]
/// `flash-mode-url-sha256` — base64 sha256-file URL for mode 3.
FlashModeUrlSha256,
```

The shared selector is gated on an internal `flash-mode` feature that each of the
three mode features enables (§3.6), so it exists whenever any mode can be
reached and disappears when none are. Modes 2 and 3 add no selector key of their
own.

### 3.2 `src/error.rs`

A `FlashError` variant hierarchy alongside `FactoryResetError`, covering:
destination device missing or not a block device, destination equal to source,
missing build-time constant, partition-table dump or apply failure, image copy
failure, network setup failure, download failure, checksum mismatch,
bootloader-environment write failure on the destination, and a failed
partition-table re-read after a flash.

### 3.3 `src/mode/mod.rs`

```rust
pub enum BootMode {
    Normal,
    #[cfg(feature = "factory-reset")]
    FactoryReset(FactoryResetTrigger),
    #[cfg(feature = "flash-mode")]
    Flash(flash::config::FlashConfig),
}
```

Detection: both triggers set at once is rejected as an error rather than
resolved by precedence (§10.6). The two operate on different disks — a factory
reset on the booted device, a mode-1 clone on another one — and the combination
was never an intended request. Legacy ran `init.d/86-factory-reset` before
`init.d/87-flash_mode_*` and so performed both, but single-mode `BootMode`
dispatch cannot express that, and silently dropping one of two requested
destructive actions is worse than refusing the pair.

Both triggers are cleared *before* the error is raised; only then does it take
the §8.1 failure path. Clearing first is not optional. This is the one fatal
path that runs before any mode has started, so the §2.2 invariant does not
cover it, and on a release image §8.1 halts forever rather than rebooting —
leaving the triggers set would mean every power cycle hits the same refusal and
the device never boots again. With both cleared, a power cycle boots normally
and the operator re-queues whichever action they meant.

Mode 2's second trigger, the `/etc/enforce_flash_mode` flag file (§5.4), ships
inside the initramfs and cannot be cleared. It does not reopen the problem:
clearing `factory-reset` is enough to remove the conflict, and the next boot
runs mode 2 alone.

Detection follows the legacy order `86-factory-reset`, `87-flash_mode_1`,
`87-flash_mode_2` (flag file first, then the key), `87-flash_mode_3`:

| State | Result |
|---|---|
| `flash-mode` `1`, with or without the flag | Mode 1 |
| flag, `flash-mode` `2`, `3`, unknown, blank or unset | Mode 2 |
| `flash-mode` `2`, no flag | Mode 2 |
| flag or `flash-mode` `2`, plus a set `factory-reset` | both cleared, then refused |
| `flash-mode` `2`, no flag, `factory-reset` unreadable | Normal |
| flag, `flash-mode` not `1`, `factory-reset` unreadable | Mode 2 |
| flag, boot env unavailable or `flash-mode` unreadable | Mode 2 |

The flag rows with an unreadable environment follow legacy, which checks the
flag before it reads any environment; the conflict check is skipped there
because it cannot be made. The
flag is checked at run time on purpose: one `omnect-os-init` package goes into
every initramfs, and the flag is added by a separate image recipe.

The refusal reaches kmsg only. A flash boot never writes the ODS status file:
`run_init` returns the error and the fatal-error path just logs it, so there is
no status file for the refusal to appear in.

Note also that the queued `factory-reset` key does not survive modes 2 and 3. On
U-Boot the environment lives at the `UBOOT_ENV1_START`/`UBOOT_ENV2_START` byte
offsets, and mode 2's own zeroing of the first `BOOT_START + BOOT_SIZE` KB
reaches through that region; on GRUB, `grubenv` sits on the boot partition, which the flash
overwrites. The reset request is destroyed, not deferred.

### 3.4 `src/lib.rs`

`BootMode::detect` moves between the boot-env decision and `init_setup`, and
runs once:

- `Flash` → dispatch directly, skipping `init_setup`;
- any other mode → `init_setup` runs, then that mode is dispatched.

The mode functions keep the existing signature convention: `run(ctx) ->
Result<()>` whose `Ok` path never returns, the same contract
`mode::normal::run` already has through `switch_root`.

### 3.5 Shared reformat helper

`factory_reset::reformat::reformat_ext4` moves to a shared module. Mode 1 needs
it to enforce the first-boot condition on the destination disk, and mode 1 ships
in images built without the `factory-reset` feature.

### 3.6 `Cargo.toml`

```toml
flash-mode = ["core"]                    # shared flash layer; not selected directly
flash-mode-1 = ["flash-mode"]            # disk cloning; part of the default feature set
flash-mode-2 = ["flash-mode"]            # scp push over the network
flash-mode-2-direct = ["flash-mode-2"]   # mode 2 without the verify pass
flash-mode-3 = ["flash-mode"]            # URL download
```

`flash-mode` gates the shared layer — the selector env key, `BootMode::Flash`,
`config.rs`, `efi.rs`, and the dispatch branch. It is never enabled directly;
each mode feature pulls it in. `flash-mode-2` and `flash-mode-3` additionally
gate `net.rs` and `bmap.rs`.

One new dependency, pulled in by `flash-mode-1` only:
`uuid = { version = "1.11", default-features = false }`. The bytes come from
`/dev/urandom`, so a failing random source is an error and not a panic in PID 1.

`default = ["core", "flash-mode-1"]`, mirroring the legacy recipe, which installs
`flash-mode-1` unconditionally and gates 2 and 3 on `DISTRO_FEATURES`. This also
resolves the current mismatch where the project `CLAUDE.md` feature table lists
`flash-mode-1/2/3` but `Cargo.toml` defines none of them.

Reviewers want mode 1 gated as well. That changes what ships, so it is recipe
work tracked in §12 rather than part of the port.

## 4. Mode 1 — clone to another disk

### 4.1 Sequence

1. Read `flash-mode-devpath`. Clear `flash-mode` and `flash-mode-devpath`. A
   missing, blank or unreadable devpath does not stop detection; its reason is
   carried to the mode, which fails with it inside the log capture, so the
   persisted log says why no destination was used.
2. Validate the required build-time constants: `DATA_SIZE` always; on U-Boot
   also `UBOOT_ENV1_START` and `UBOOT_ENV_SIZE`. `UBOOT_ENV1_START` is also
   required whenever `BOOTLOADER_START` is set, independently of the
   bootloader feature, because step 8 computes the bootloader-area copy
   length as `UBOOT_ENV1_START - BOOTLOADER_START`. `UBOOT_ENV2_START` is
   optional (§10.7).
3. Wait for the destination block device, bounded (§7). A destination that
   belongs to the source always exists already, so the wait returns at once
   for it; only a path that names no device at all waits the full timeout.
4. Resolve the destination path — once, here, because resolving needs the node
   to exist — and use the resolved path for every step that follows. Reject a
   destination that is not a block device, and one whose device number is the
   source disk's or whose parent disk (read from `/sys/dev/block`) is the
   source disk. Reject a partition of any other disk too, before `sfdisk`
   writes a table into it. Comparing device numbers catches every alias
   spelling of the running disk. A destination sysfs does not list is refused, because a
   partition of the source cannot be ruled out.
5. `sync`, then unmount `/rootfs` completely — the boot partition first, then the
   rootfs. Both are mounted by `mount_core_partitions` on both bootloaders. The
   boot unmount is needed so the raw copy of the boot partition reads a
   consistent image; the rootfs unmount is needed so step 11 does not run
   `e2image` against a filesystem the kernel currently has mounted, with live
   superblock and journal state.
6. Read the source partition-table dump, rewrite it (§4.2), apply it to the
   destination.
7. Verify every expected destination partition now exists as a block device.
8. If `BOOTLOADER_START` is set, copy the bootloader area from source to
   destination: `bs=1024`, `count = UBOOT_ENV1_START - BOOTLOADER_START`, at the
   same byte offset on both sides.
9. Reformat destination `etc` and `data` as ext4 with their volume labels. This
   is what enforces the first-boot condition on the clone.
10. Copy destination `boot`, `factory` and `cert` from the corresponding source
    partitions.
11. Copy the running rootfs into destination `rootA` with
    `e2image -ra -p /dev/omnect/rootCurrent`.
12. Assign fresh partition UUIDs to destination `boot` and `rootA`. The UUIDs
    live in the partition table, so assigning them after the image copy is
    equivalent and keeps them in one place.
13. Write the default bootloader environment to the destination:
    - GRUB: mount the destination boot partition, copy
      `/etc/omnect/grubenv.in` to `EFI/BOOT/grubenv`, unmount;
    - U-Boot: write `/etc/omnect/uboot-env.bin` at `UBOOT_ENV1_START`, and at
      `UBOOT_ENV2_START` when that offset is defined. Where a machine reserves a
      second bank, skipping the write would leave it holding whatever the clone
      inherited. The legacy comment reads the two writes as enforcing a redundant
      environment, which writing bytes to an offset cannot do. See §10.7.
14. EFI handling on the destination (§6).
15. `sync`.

Steps 9 and 10 keep the legacy order — reformat before copying the other
partitions.

Log persistence and the terminal action sit in `mod.rs`, around this sequence,
not inside it: the log is written whether the sequence succeeded or failed, and
`poweroff` follows only on success (§8.1, §8.3). A second `sync` runs there
after the log write, because `reboot(2)` does not flush and step 15 runs before
the log is written. This mirrors the legacy split
between `run_flash_mode_1` and `flash_mode_1_run`.

### 4.2 Partition-table dump rewriting

The one piece of real logic in mode 1, and the reason `sfdisk.rs` is a separate
pure module. Both variants reset the data partition to its shipped size, undoing
any earlier `resize-data` growth so the clone starts from the shipped layout.

Sizes are in 512-byte sectors, so the KB-valued `DATA_SIZE` is doubled.

- **GPT** — set `last-lba` to `data_start + DATA_SIZE*2 - 1`, and the data
  partition's `size=` to `DATA_SIZE*2`.
- **DOS** — set the data partition's `size=` to `DATA_SIZE*2`, and the extended
  container's `size=` to `data_start - extended_start + DATA_SIZE*2`.

Start sectors come from the source dump.

### 4.3 Destination partition addressing

The destination receives a copy of the source partition table, so the roles map
onto the same indices on both disks. A `RootDevice` is built for the destination
path and each role resolved with `partition_path(PARTITION_NUM_*)`.

## 5. Modes 2 and 3 — network flashing

Both overwrite the running disk. Both share: unmount everything on the disk,
bring up the network, flash, EFI handling, `sync`, `reboot`. The disk is named
`/dev/omnect/rootblk` below; the code addresses the same device by the root
device's base path.

### 5.1 Unmounting

`sync`, then unmount `/rootfs` completely — boot partition first, then rootfs, on
both bootloaders (§2.3). Then unmount every remaining mount point backed by the
target disk, by sweeping `/proc/mounts`.

### 5.2 Network setup (`net.rs`)

Restricted to `eth0`, as in legacy. Retry `ip link set eth0 up` until it
succeeds — the interface may be probed late, e.g. a USB NIC — then start
`dhcpcd eth0` and wait for an IPv4 address. Both waits are bounded (§7). Mode 2
additionally creates and mounts `devpts` at `/dev/pts`, creates `/etc/dropbear`
and starts `dropbear -R`, generating the host key at runtime.

Child processes get an explicit `PATH`, as legacy exports it before `bmaptool`:
PID 1 has no login environment, `bmaptool` runs `xz`, and `dhcpcd` runs hook
scripts.

### 5.3 Mode 3 — pull from URL

1. Clear `flash-mode`. Read and immediately clear `flash-mode-url`, then
   `flash-mode-url-sha256`. Base64-decode both; reject empty values.
2. Unmount (§5.1), network up (§5.2).
3. Download the sha256 file, then the image, with
   `curl --no-progress-meter -Lo`. Add `-k` when `MACHINE_FEATURES` does not
   contain `rtc`: without a reliable clock, certificate validity cannot be
   checked.
4. Verify the image against the downloaded sha256.
5. `bmaptool copy --nobmap <image> /dev/omnect/rootblk`.
6. Re-read the partition table (§8.3), EFI handling (§6), `sync`, log (§8),
   `reboot`.

Legacy mode 3 computes a `dest_blk` and a partition suffix from `rootA` and never
uses them; not ported.

### 5.4 Mode 2 — scp push

Trigger: `flash-mode == 2`, **or** the presence of `/etc/enforce_flash_mode`, the
flag file shipped by `omnect-os-initramfs-test`. Both are kept.

1. Clear `flash-mode`.
2. Unmount (§5.1), network up (§5.2).
3. Create the image FIFO at `/home/omnect/wic.xz`, owned by the `omnect` user, so
   `scp` streams directly into `bmaptool`. Then start `dropbear` (§5.2). The FIFO
   comes first, so a client that can log in always finds it; CI checks for it
   over `ssh` to know the device is ready.
4. Log the first command the operator must run, with the acquired IP address:
   `scp <bmap-file> omnect@<ip>:wic.bmap`.
5. Wait for `/home/omnect/wic.bmap` to be complete, unbounded — this waits for
   a person (§7). Complete means a regular file whose content ends with the
   closing `</bmap>` tag, so a half-copied bmap does not end the wait. Then log
   the second command, `scp <wic-image> omnect@<ip>:wic.xz`, in the same order
   as legacy.
6. Flash. Every `bmaptool` call uses `--bmap /home/omnect/wic.bmap`:
   - **default** — verify pass first:
     `bmaptool copy --bmap wic.bmap wic.xz /home/omnect/wic`, which consumes the
     FIFO and materializes the mapped, decompressed image as a file in the
     initramfs root. Then zero the first `BOOT_START + BOOT_SIZE` KB of the
     disk, then `bmaptool copy --bmap wic.bmap /home/omnect/wic
     /dev/omnect/rootblk`. The RAM cost of the verify pass is the size of the
     mapped image; that cost is why the direct path exists. The kernel makes
     the initramfs root a tmpfs when `CONFIG_TMPFS` is set and the command
     line has no `root=` (GRUB), and a ramfs otherwise (U-Boot passes
     `root=`). ramfs has no size limit, so an image too big for RAM fails by
     running out of memory; tmpfs stops at its size limit, half the RAM by
     default, with `ENOSPC`.
   - **`flash-mode-2-direct`** — zero the head, then
     `bmaptool copy --bmap wic.bmap wic.xz /dev/omnect/rootblk` straight from
     the FIFO. No verification.
7. Re-read the partition table (§8.3), EFI handling (§6), `sync`, log (§8),
   `reboot`.

Once `bmaptool` starts it blocks reading the FIFO until the operator's `scp`
feeds it, and that wait stays unbounded too: a timeout there would kill a flash
in progress and leave the disk half-written (§10.8).

The zeroing step is the legacy `non_bmap_dd_handling`. Its comment records
post-flash boot failures observed on both GRUB (mismatched `bootx64.efi`
checksums) and U-Boot (boot-partition errors after `bmaptool`). It is ported —
see §10.1.

## 6. EFI handling

Applies to `grub` builds: the recipe selects the `grub` feature exactly for
machines whose `MACHINE_FEATURES` contain `efi`, so the check is made at build
time. Ported from
`flash_mode_efi_handling` in `common-sh`, with the order changed so the machine
always keeps a boot entry:

1. Mount `efivarfs` at `/sys/firmware/efi/efivars` unless it is mounted
   already. `efibootmgr` needs it, and the init mounts only `/dev`, `/proc`,
   `/sys` and `/run`.
2. List every EFI boot entry, active or not. `flash_mode_efi_handling` greps
   with an unquoted `^Boot[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]\*`: the
   shell turns `\*` into `*`, which `grep` reads as "zero or more" of the last
   hex class, so every `Boot####` line matches, with or without the `*`
   active marker.
3. Create an `omnect_os` entry pointing at `\EFI\BOOT\bootx64.efi` on partition 1
   of the target disk. `efibootmgr -c` takes an unused number.
4. Delete every entry listed in step 2.
5. Mount the target boot partition — the destination's for mode 1, the running
   disk's for modes 2 and 3 — write `efibootmgr -v` output to
   `EFI/BOOT/efibootmgr_entry` on it, and unmount.

Legacy deletes first and creates second. The end state is the same, but a
failure between the two left the machine with no boot entry.

The legacy duplicate entry — a second entry with the same loader and the label
`"omnect_os "`, differing only by a trailing space — is not ported (§10.2).

Step 4 is in §10.3.

## 7. Bounded waits

Every wait logs once when it starts, and every machine-driven wait is bounded
by a named constant. On
timeout the mode fails into the normal fatal-error path (§8).

| Wait | Legacy | Proposed bound | Rationale |
|---|---|---|---|
| Mode 1 destination block device | 30 s, off-by-one bug | 30 s | unchanged, bug fixed |
| Interface up | unbounded | 60 s | machine-driven, should be immediate |
| IPv4 address after `dhcpcd` returns | unbounded | 120 s | `dhcpcd` returns after its own 30 s timeout and keeps trying in the background; without a DHCP server it assigns an IPv4LL address |
| Mode 2 `wic.bmap` arrival | unbounded | unbounded | waits for a person to start the `scp` (§10.8) |

The values are proposals — reviewers should say if any is wrong for their
machines. Each becomes a named constant.

Because `flash-mode` is already cleared, a power cycle leaves the device in
Normal boot in both the legacy and the ported behaviour, and the operator
re-triggers the mode. Bounding a machine-driven wait turns a silent hang into a
diagnosable failure. The `scp` wait is not machine-driven: a bound there refuses
a flash that legacy would still perform, and buys nothing an operator does not
already know from the missing data (§10.8).

## 8. Error handling, terminal actions and logging

### 8.1 Terminal actions

| Mode | Success | Failure |
|---|---|---|
| 1 | `poweroff` | existing fatal-error path |
| 2 | `reboot` | existing fatal-error path |
| 3 | `reboot` | existing fatal-error path |

The failure path is the existing `handle_fatal_error`: a shell in the debug
image, a log-and-sleep loop in the release image. No new policy.

Mode 1 powers off rather than rebooting because it leaves a cloned disk that an
operator must physically move; rebooting would come back up on the source disk.
See §10.4.

### 8.2 Per-error handling

| Error source | Handling |
|---|---|
| Boot env read failure | Log warn → Normal boot. A queued factory reset is not cleared and not reported either; it runs on the next boot where the env can be read |
| Unknown `flash-mode` value | Log warn → Normal boot |
| `flash-mode` clear failure | Log warn → continue. The keys stay set after the power off, so the next power-on runs the mode again, onto whatever is at `flash-mode-devpath` then |
| Missing build-time constant | Fatal |
| Destination device missing, invalid, or equal to source | Fatal |
| Dump read, rewrite, or apply failure | Fatal |
| Destination partition missing after apply | Fatal |
| Reformat or partition copy failure | Fatal |
| UUID assignment failure | Fatal |
| Destination bootloader-env write failure | Fatal |
| Network setup or wait timeout | Fatal |
| Download failure or checksum mismatch | Fatal |
| `bmaptool` failure | Fatal |
| Partition-table re-read failure after the flash (§8.3) | Fatal, no log. The image is on the disk, but the EFI step and the log mount must not use the old table |
| EFI handling failure | Fatal. The machine keeps its old entries if the create failed, and the new entry plus any not yet deleted if a delete failed |
| Log persistence failure | Log warn → continue |

"Fatal" means the mode aborts into §8.1's failure path. For mode 1 the source
disk is untouched, so a power cycle boots normally. For modes 2 and 3 the disk is
left half-written, which is unavoidable for a whole-disk flash.

### 8.3 Logging

One capture mechanism for all three modes, mirrored to kmsg and the console as it
runs. Persistence depends on whether a safe target exists:

- **Mode 1** — the log is written to the **source** data partition as
  `flash-mode-1.log`, unconditionally, with the name and target legacy uses. Nothing in the sequence writes destructively
  to the source, so this is safe on both success and failure. Mode 1 mounts
  that partition itself for the write: nothing else does so on a flash boot.
  `mount_remaining_partitions` (which mounts `data` in the Normal path) runs
  only there, and mode 1 unmounts the rootfs at step 5 before its own work
  starts.

  "Not written destructively" is the precise claim. Three writes do reach the
  source, each required and each matching legacy:

  - clearing the flash triggers, which is `grubenv` on the source boot
    partition under GRUB and the source U-Boot environment region under U-Boot
    (§2.2);
  - mounting the source `data` partition read-write for this log;
  - on an EFI machine, rewriting the running machine's NVRAM boot entries (§6).

  Stating it as "the source is never written" would be wrong, and would invite
  a later change to break the property while the doc still reads as true.
- **Modes 2 and 3** — the whole disk is overwritten. A failure while the disk
  is written leaves it in an unknown partly written state, and mounting anything
  on it is unsafe. That failure is `FlashError::DiskPartlyWritten`, and it leaves
  nothing on disk; diagnosis stays on kmsg and the console. The new image may
  place partitions elsewhere, so right after the flash pass the kernel re-reads
  the partition table (`BLKRRPART`), before the EFI dump mount (§6 step 5) and
  the log mount. A failed re-read is `FlashError::StalePartitionTable` and also
  leaves no log. Every other outcome, success or a failure before the disk is
  written or after the re-read, writes the log best-effort to the data
  partition. The file is `flash-mode-2.log` for mode 2.

See §10.5.

## 9. Intentional deviations from the legacy scripts

Three bugs in `flash-mode-1`, fixed rather than reproduced:

1. **Destination-device wait off-by-one.** The loop is
   `for i in $(seq 1 30); do if [ -b "${blk_dev_dst}" ]; then break; fi; ...; done`
   followed by `if [ ${i} -eq 30 ]; then stderr_fatal ...`. When the device
   appears on the 30th iteration, `i` is 30 and the script reports failure even
   though the device is present.
2. **DOS extended-partition start read from the wrong path.** The
   extended-container branch calls `get_start_sector $(readlink -f extended)`
   with a relative path, where every sibling call passes `/dev/omnect/...`.
   `get_start_sector` matches its argument against an `sfdisk -d` dump of the
   root block device, so the relative path cannot match and the
   extended-partition size calculation is wrong on DOS machines.
3. **Unconditional partition-UUID refresh on a table with no per-partition
   UUID.** The partition-UUID refresh in `flash-mode-1` runs `sfdisk
   --part-uuid` on the boot and root partitions unconditionally, with `||
   return 1` on failure. An MBR partition table has no per-partition UUID, so
   this step fails legacy mode 1 on a DOS machine — even though the
   partition-copy section just above it already branches on `part_type` to
   handle GPT and DOS separately for the `etc`/`data` reformat and the
   boot/factory/cert copy. The port gates the UUID refresh on GPT (§4.1 step
   12), so a DOS clone completes.

Also not ported:

- the redundant `cut -d= -f2` on `flash-mode-devpath` (§2.1);
- the unused `dest_blk` / partition-suffix computation in `flash-mode-3` (§5.3);
- the hardcoded partition indices and the `p`-suffix probe (§2.6).

Behaviour changes, as opposed to bug fixes:

- machine-driven unbounded waits become bounded (§7); the wait for the
  operator's `scp` keeps polling as legacy does (§10.8);
- mode 2 creates the image FIFO before it starts `dropbear`; legacy started
  `dropbear` first, so a login could briefly find no `wic.xz` (§5.4);
- the `wic.bmap` wait ends when the file is complete, where legacy stopped as
  soon as the file existed (§5.4);
- `dd` is replaced by in-process file I/O (§2.8);
- the EFI loader is passed to `efibootmgr` as `\EFI\BOOT\bootx64.efi`. The
  legacy script's unquoted `\\\\EFI\\\\BOOT` reached it as
  `\\EFI\\BOOT\\bootx64.efi`. Whether the firmware treats both the same is
  not verified; the EFI hardware run in §10.2 covers it;
- the new EFI entry is created before the old ones are deleted (§6);
- every mode unmounts `/rootfs` fully before writing, because the Rust flow mounts
  it before dispatch and legacy did not (§2.3). Without this, mode 1 would image a
  mounted `rootCurrent`;
- a queued factory reset combined with a flash mode is now an error. Legacy ran
  both (86 then 87); single-mode dispatch cannot, and refuses the pair rather
  than dropping one silently. Both triggers are cleared before the error, so a
  power cycle boots normally (§3.3, §10.6);
- modes 2 and 3 may persist a log where legacy did not (§8.3, §10.5);
- a failed unmount in the §5.1 sweep fails the run; legacy ignored it
  (`umount ... 2>/dev/null`);
- modes 2 and 3 make the kernel re-read the partition table after the flash,
  before the EFI step mounts the boot partition; legacy mounted it through the
  table from before the flash (§8.3);
- the `check_fs` on the source data partition before the log mount is dropped.
  Legacy runs it in `flash_mode_1_run` just before mounting; the port mounts
  directly. Bounded: the log write is best-effort either way, so a mount that
  fails only warns (§8.2);
- the console tee is lost. Legacy pipes the whole run through
  `tee … >/dev/console`, so the operator at the device sees every line. The
  port emits `log::info!` to `/dev/kmsg` only, which a `quiet` boot keeps off
  the console. `e2image` output still reaches it: the init reads it from a pipe
  and writes it to its own stderr, which the kernel sets to `/dev/console`.
  Recorded as a known deviation; a console writer is a separate decision.

### 9.1 Limitations: identifiers shared with the source disk

Mode 1 reproduces some of the source disk's identifiers on the clone. Both
points are parity with legacy, not regressions introduced by the port.

- The destination keeps the source's disk-level identifier — the MBR disk
  signature on a DOS table, the GPT `label-id` on a GPT table — because the
  source's `sfdisk -d` dump is reapplied to the destination unchanged apart
  from resetting the data partition, and on DOS the extended container, to
  its shipped size (§4.2). On GPT, `boot` and `rootA` additionally receive
  fresh per-partition UUIDs (§4.1 step 12); a DOS table has no per-partition
  UUID for `sfdisk` to refresh, so nothing is renewed there (§9 bug 3).
- On both layouts the clone reproduces the source's vfat volume ID and ext4
  superblock UUID: `boot` is copied as a raw byte range and the rootfs via
  `e2image` (§4.1 steps 10, 11), and neither touches filesystem-level
  identifiers.

With both disks attached, GRUB's `bootpart_fsuuid` boot-partition lookup
through `blkid` is therefore ambiguous — it matches by filesystem UUID, which
both disks now share. Mode 1 powers off on success rather than rebooting
(§10.4), which gives the operator a window to move the disk before either one
is booted again.

Two further consequences, both legacy parity:

- The clone's `rootB` is never initialised. Only `rootA` receives an image
  (§4.1 step 11), so a destination that previously held an omnect install keeps
  whatever rootfs was in `rootB`. Harmless in practice: the default bootloader
  environment written in step 13 selects `rootA`.
- On an EFI machine, mode 1 repoints the **running** machine's NVRAM at the
  destination disk (§6). After the power off, the source machine's default boot
  entry names a disk the operator is about to remove.

## 10. Decisions required from reviewers

Every item below is decided, and the rest of the spec follows that decision.

### 10.1 Keep `non_bmap_dd_handling`?

Zeroing the first `BOOT_START + BOOT_SIZE` KB of the disk before flashing in
mode 2. The legacy comment records post-flash boot failures observed on both GRUB and U-Boot,
but the root cause was never established, so this may be masking a `bmaptool` or
partition-alignment problem rather than fixing one.

**Decided: keep.**

### 10.2 Keep the duplicate EFI boot entry?

`flash_mode_efi_handling` creates two entries pointing at the same loader,
differing only by a trailing space in the label, commented as "for debug
purposes, when booting after flash-mode-{1,2} fails".

**Decided: drop it.** The port creates one entry (§6).

The reason given for dropping it — that EFI updates since then have made the
second entry unnecessary — is **unverified**: no source was recorded for it, and
the EFI path has had no hardware run. Confirmation by the hardware CI on an EFI
machine is therefore a condition for shipping this, not a note. If a machine
still needs the second entry, restore it and record why here.

### 10.3 Keep deleting every existing EFI boot entry?

The current handling removes every EFI boot entry on the machine, active or
not, before creating its own, including entries unrelated to omnect (§6 item 1).

**Decided: keep — it is what ships today.**
The new entry is created before the old ones are deleted (§6).

### 10.4 Uniform `reboot`, including mode 1?

Mode 1 currently powers off on success.

**Decided: keep `poweroff` for mode 1** — it leaves a cloned disk an operator
must move, and a reboot would come back up on the source disk.

### 10.5 Persist a log for modes 2 and 3 at all?

§8.3 specifies a best-effort write unless the disk may be partly written, which
costs an extra mount of the data partition and yields nothing on the failures
during the disk write.

**Decided: best-effort, unless the disk may be partly written.** The
alternative would be kmsg and console only, exactly like legacy.

### 10.6 Should a queued factory reset still run before mode 1?

Legacy ran `init.d/86-factory-reset` before `init.d/87-flash_mode_1`, so both
happened: the source disk was reset, then cloned. Single-mode `BootMode` dispatch
gives one handler and cannot express that.

**Decided: reject the combination with an error.** A factory reset acts on the
booted device and a mode-1 clone on another one; requesting both was never
intended, and silently performing only one of two destructive requests is the
worse failure.

The error clears both triggers first (§3.3) — without that, a release image
halts forever and every power cycle repeats the refusal. If even a one-time
halt is unwanted on an in-field device, the alternative is to clear both, log
the refusal and continue to Normal boot, which is the handling unknown
`flash-mode` values already get in §8.2. Say so if you prefer that; the
refusal is equally visible either way.

### 10.7 Keep writing the U-Boot environment to both offsets?

Mode 1 step 13 copies `uboot-env.bin` to `UBOOT_ENV1_START` and
`UBOOT_ENV2_START`. The legacy comment claims this enforces a redundant
environment even when the initial wic had only one. It does not: U-Boot uses a
second copy only when its build configures a redundant environment.

**Decided: write the second copy when `UBOOT_ENV2_START` is defined.** The
offset has no default and is set per machine, so its absence already expresses
"this machine reserves no second bank" and no new variable is needed. Where the
U-Boot build ignores a reserved bank the extra write is wasted, not harmful.
`UBOOT_ENV2_START` therefore drops out of the required constants (§2.7, §4.1).

### 10.8 Bound the mode 2 `wic.bmap` wait?

The other three waits in §7 are machine-driven, so a bound is meaningful. This
one waits for a person to start the `scp`, and legacy polls forever: an operator
who starts the copy after the bound still gets a flash today, but would get the
§8.1 failure path after the port.

**Decided: leave it unbounded.** The bound existed only to keep the "no
unbounded code path" rule. Production images never reach this mode, and on a
development image a shell after the bound says nothing the missing data has not
already said. The same decision removes the `bmaptool` watchdog, whose timeout
would kill a flash in progress and leave the disk half-written (§5.4).

## 11. Testing

Decision logic is pure and unit-tested; command execution is a thin layer that is
only smoke-tested. Real end-to-end coverage stays in Concourse CI on hardware.

| Test | Kind | Location |
|---|---|---|
| GPT dump rewrite: `last-lba` and data size | unit | `src/mode/flash/sfdisk.rs` |
| DOS dump rewrite: data and extended size | unit | `src/mode/flash/sfdisk.rs` |
| Dump rewrite rejects a malformed dump | unit | `src/mode/flash/sfdisk.rs` |
| `FlashConfig` parse: valid `1`/`2`/`3` | unit | `src/mode/flash/config.rs` |
| `FlashConfig` parse: unknown value, empty value | unit | `src/mode/flash/config.rs` |
| Mode 1: missing or empty `flash-mode-devpath` rejected | unit | `src/mode/flash/config.rs` |
| Mode 3: base64 decode, invalid base64 rejected, empty URL rejected | unit | `src/mode/flash/config.rs` |
| Destination role → partition index, GPT and DOS | unit | `src/mode/flash/clone.rs` |
| `curl` options selected from `MACHINE_FEATURES` `rtc` | unit | `src/mode/flash/url.rs` |
| scp instruction text includes the acquired IP | unit | `src/mode/flash/scp.rs` |
| Detection: both triggers set → both cleared, then refused | unit | `src/mode/mod.rs` |
| Detection: every row of the §3.3 trigger table | integration | `tests/flash_modes.rs` |
| `/proc/mounts` sweep: device numbers, deepest first | unit | `src/mode/flash/unmount.rs` |
| Mode 2 step order, default and `flash-mode-2-direct` | unit | `src/mode/flash/scp.rs` |
| Mode 2 log skipped only on a partly written disk | unit | `src/mode/flash/mod.rs`, `src/mode/flash/scp.rs` |
| Clear-first ordering: a failing mode still leaves its triggers cleared | unit | `src/mode/flash/mod.rs` |
| Destination refusal by device number, parent disk from a fake sysfs tree | unit | `src/mode/flash/clone.rs` |
| Boot-env read failure falls back to Normal boot | integration | `tests/flash_modes.rs` |

`tests/flash_modes.rs` follows `tests/factory_reset.rs` and uses the existing
`MockBootEnv`.

## 12. meta-omnect companion work

Implemented separately, listed here so nothing is lost:

- export the five mode 1 constants of §2.7 into the `omnect-os-init` build
  environment (`omnect-os-init.inc`). Without them every constant is `None`,
  and mode 1 fails with a missing build constant before it writes anything;
- for mode 2, pass `OMNECT_PART_OFFSET_BOOT`, `OMNECT_PART_SIZE_BOOT` and
  `OMNECT_USER_ID` the same way, and enable the cargo feature
  `flash-mode-2-direct` when `OMNECT_FLASH_MODE_2_DIRECT_FLASHING` is `1`;
- define `OMNECT_USER_ID ?= "15581"` in the distro configuration and use it in
  `omnect_user.bbclass` for both `groupadd -g` and `useradd -u`, so the class
  and the init recipe, which does not inherit the class, share one value;
- map `DISTRO_FEATURES` `flash-mode-2` and `flash-mode-3` onto the corresponding
  Cargo features;
- gate mode 1 the same way: map `DISTRO_FEATURES` `flash-mode-1` onto the Cargo
  feature and drop `flash-mode-1` from `default`, so a machine that wants disk
  cloning has to ask for it. This changes what ships, so it is recipe work rather
  than part of the port;
- when mode 2 or 3 is ported, add `FLASH_MODE_X_PACKAGES` plus `dropbear`
  (mode 2) and `curl` (mode 3) back to the Rust initramfs image, gated by the
  same distro features as the legacy image, and keep the `omnect_user` class
  inherited for mode 2. The Rust image does not install them while the modes
  are not ported;
- when mode 2 or 3 is ported, remove the "does not implement flash mode N yet"
  note from that mode's section in the meta-omnect `README.md`, and check its
  console output example and behaviour notes against the port
- retire `init.d/87-flash_mode_{1,2,3}` and the `sed` substitutions in
  `omnect-os-initramfs-scripts.bb` once the Rust path ships;
- `util-linux-uuidgen` can be dropped from the initramfs once the port ships:
  `uuidgen` is called from `flash-mode-1` and nowhere else, and the Rust port
  generates the UUID itself (§2.8);
- install `e2fsprogs` and, on EFI machines, `efibootmgr` in the Rust
  initramfs image. `e2image` ships in the base `e2fsprogs` package, and the
  Rust image names only the `e2fsprogs-e2fsck`, `-mke2fs` and `-tune2fs`
  sub-packages; `buildhistory` for a Rust-init `omnect-os-initramfs` has no
  `e2image` (§2.8).

## 13. Interactions

This spec is written against `upstream/main` at `d1168de`. The factory-reset wipe
modes 2, 3 and 4 are being designed in parallel on
`feat/factory-reset-wipe-modes`. Both add `BootEnvKey` variants and both touch
`BootMode` and `src/lib.rs` dispatch, so those three places are the expected
merge points. The naming pinned in §2.4 exists to keep `FlashMode` and
`ResetMode` distinct once both land.
