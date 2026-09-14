# Flash Modes 1, 2, 3 — Design

Port the three flash modes from the legacy scripted initramfs
(`meta-omnect/recipes-omnect/initrdscripts/omnect-os-initramfs/flash-mode-{1,2,3}`)
to the Rust initramfs.

**This spec has a blocking section: [§10 Decisions required from
reviewers](#10-decisions-required-from-reviewers). Implementation must not start
before every item there is decided. §10.2 and §10.6 are decided; §10.1, §10.3,
§10.4, §10.5 and §10.7 are still open.**

## 1. Overview

A flash mode deploys a whole disk image from the initramfs, before any rootfs is
handed control. The mode is selected through the bootloader environment and runs
at most once — the trigger is cleared before the work starts.

| Mode | What it does | Network | Gating |
|---|---|---|---|
| 1 | Clones the running disk onto another block device | no | on by default |
| 2 | Flashes a `wic.xz` pushed in over `scp` onto the running disk | yes | opt-in |
| 3 | Flashes a `wic.xz` downloaded from a URL onto the running disk | yes | opt-in |

Mode 1 behaves like a factory reset with respect to the new disk: it resets the
bootloader environment, reformats `etc` and `data` to enforce the first-boot
condition, and gives the copied partitions fresh UUIDs.

Implementation order is **1 → 2 → 3**. Mode 2 comes before mode 3 because it
is the mode used in the development cycle, even though its interactive `scp`
wait is the hardest part to port and to test.

### 1.1 Fidelity policy

Observable behaviour is preserved: the same environment keys, the same terminal
actions, the same platform workarounds. Two exceptions, both deliberate and both
recorded in [§9](#9-intentional-deviations-from-the-legacy-scripts):

- two legacy bugs are fixed;
- unbounded waits become bounded.

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

Current `run_init` order is: mount core partitions → open boot env → `init_setup`
(extra-bootargs sync, then resize-data) → `BootMode::detect` → dispatch.

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
- **deviates** — the Rust initramfs mounts the rootfs at `/sysroot` and the boot
  partition at `/sysroot/boot` in `mount_core_partitions` before dispatch, on both
  bootloaders. Legacy mounted the boot partition on demand for environment access
  only (GRUB), and `fs-mount` (89) ran after the flash modes, so the rootfs was
  never mounted while a flash mode ran. This matters for mode 1, which images the
  running rootfs: every mode therefore unmounts `/sysroot` completely before
  writing anything (§4.1 step 5, §5.1). Nothing in any mode needs `/sysroot` —
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
  efi.rs        efibootmgr handling                                      flash-mode
  clone.rs      mode 1 orchestration                                     flash-mode-1
  sfdisk.rs     partition-table dump parsing and rewriting      (pure)   flash-mode-1
  net.rs        interface up, dhcpcd, dropbear                           flash-mode-2/3
  bmap.rs       bmaptool wrapper                                         flash-mode-2/3
  scp.rs        mode 2 orchestration                                     flash-mode-2
  url.rs        mode 3 orchestration                                     flash-mode-3
```

The right column is the gating feature (§3.6).

External tools are invoked through `std::process::Command` with named `const`
paths, as `factory_reset/reformat.rs` already does. No new command-runner
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

| Yocto variable | Constant |
|---|---|
| `OMNECT_PART_OFFSET_UBOOT_ENV1` | `UBOOT_ENV1_START` |
| `OMNECT_PART_OFFSET_UBOOT_ENV2` | `UBOOT_ENV2_START` |
| `OMNECT_PART_SIZE_UBOOT_ENV` | `UBOOT_ENV_SIZE` |
| `OMNECT_PART_SIZE_DATA` | `DATA_SIZE` |
| `BOOTLOADER_SEEK` | `BOOTLOADER_START` |

Mode 2 adds two through the same mechanism:

| Yocto variable | Constant | Note |
|---|---|---|
| `OMNECT_PART_OFFSET_BOOT` + `OMNECT_PART_SIZE_BOOT` | `DD_ZERO_SIZE` | `Option<u64>`, sum in KB |
| `OMNECT_FLASH_MODE_2_DIRECT_FLASHING` | `DIRECT_FLASHING` | `bool`, `1` → `true`, anything else → `false` |

`DD_ZERO_SIZE` is `Option<u64>` and absent on builds that do not set it, exactly
like the existing five; mode 2 treats it as a missing required constant.
`DIRECT_FLASHING` is a plain `bool` defaulting to `false` when the variable is
absent or is not `1`, matching the legacy
`oe.utils.conditional('OMNECT_FLASH_MODE_2_DIRECT_FLASHING', '1', 'true', 'false')`.
The legacy recipe computes the sum with `bc` because bitbake does not evaluate
shell arithmetic; `build.rs` sums the two values itself and needs only the two
Yocto variables.

### 2.8 External tools and in-process equivalents

Paths verified against `buildhistory` for a built `omnect-os-initramfs`
(`raspberrypi4_64`, U-Boot, `flash-mode-2` and `flash-mode-3` both enabled). The
image is usrmerged — `/bin -> usr/bin` and `/sbin -> usr/sbin` — so the
`/sbin/...` form the existing code uses resolves correctly.

| Tool | Path | Package | Modes |
|---|---|---|---|
| `sfdisk` | `/usr/sbin/sfdisk` | `util-linux-sfdisk` | 1 |
| `e2image` | `/usr/sbin/e2image` | `e2fsprogs` | 1 |
| `mkfs.ext4` | `/usr/sbin/mkfs.ext4` | `e2fsprogs-mke2fs` | 1 |
| `tune2fs` | `/usr/sbin/tune2fs` | `e2fsprogs-tune2fs` | 1 |
| `dd` | `/usr/bin/dd` | `coreutils` | 1, 2 |
| `bmaptool` | `/usr/bin/bmaptool` | `bmaptool` | 2, 3 |
| `curl` | `/usr/bin/curl` | `curl` | 3 |
| `dhcpcd` | `/usr/sbin/dhcpcd` | `dhcpcd` | 2, 3 |
| `dropbear` | `/usr/sbin/dropbear` | `dropbear` | 2 |
| `efibootmgr` | `/usr/sbin/efibootmgr` | `efibootmgr` | 1, 2, 3, EFI machines only |

`efibootmgr` is absent from the verified image, which has no `efi` in
`MACHINE_FEATURES` — consistent with the recipe gating and with §6 applying only
on EFI machines.

The remaining tools stay external because no pure-Rust equivalent exists at a
dependency weight an initramfs can carry: `sfdisk` (partition tables),
`e2image`, `mkfs.ext4` and `tune2fs` (ext4), `bmaptool` (block maps),
`efibootmgr` (EFI variables), `curl`, `dhcpcd` and `dropbear`. `dd` could be
replaced by plain file I/O, but `coreutils` ships in the image regardless, so
that would save a process spawn and no package.

Five operations the legacy scripts shell out for are done in-process instead.
Four use `nix`, which is already a dependency with the required features
enabled; `uuidgen` needs the new `uuid` crate:

| Legacy | In-process |
|---|---|
| `uuidgen` | `uuid::Uuid::new_v4` |
| `mkfifo` | `nix::unistd::mkfifo` |
| `chown omnect:omnect` | `nix::unistd::chown`, with the uid/gid looked up via the `user` feature |
| `sync` | `nix::unistd::sync` |
| `reboot -f` / `poweroff -f` | `nix::sys::reboot::reboot` with `RB_AUTOBOOT` / `RB_POWER_OFF` |

The reboot call follows the existing pattern in `handle_fatal_error`: it returns
`Result<Infallible>`, so the `Ok` arm is uninhabited and only the error path is
reachable.

## 3. Component changes

### 3.0 `build.rs`

Two more `rerun-if-env-changed` lines and two more generated constants for mode 2
(§2.7): `DD_ZERO_SIZE`, summed from `OMNECT_PART_OFFSET_BOOT` and
`OMNECT_PART_SIZE_BOOT`, and `DIRECT_FLASHING`. The existing `read_u64_env` helper
covers the first; the second needs a small boolean reader. The doc-comment table
at the top of `build.rs` gains both rows.

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
failure, network setup failure, download failure, checksum mismatch, and
bootloader-environment write failure on the destination.

### 3.3 `src/mode/mod.rs`

```rust
pub enum BootMode {
    Normal,
    #[cfg(feature = "factory-reset")]
    FactoryReset(factory_reset::config::FactoryResetConfig),
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

The error takes the §8.1 failure path. Both triggers stay set, so the operator
can clear one and retry.

Note also that the queued `factory-reset` key does not survive modes 2 and 3. On
U-Boot the environment lives at the `UBOOT_ENV1_START`/`UBOOT_ENV2_START` byte
offsets, and mode 2's own zeroing of the first `DD_ZERO_SIZE` KB reaches through
that region; on GRUB, `grubenv` sits on the boot partition, which the flash
overwrites. The reset request is destroyed, not deferred.

### 3.4 `src/lib.rs`

`run_init` gains an early flash-mode check between the boot-env decision and
`init_setup`:

- flash mode active → dispatch directly, skipping `init_setup`;
- otherwise → `init_setup` runs and dispatch happens where it does today.

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
flash-mode-3 = ["flash-mode"]            # URL download
```

`flash-mode` gates the shared layer — the selector env key, `BootMode::Flash`,
`config.rs`, `efi.rs`, and the dispatch branch. It is never enabled directly;
each mode feature pulls it in. `flash-mode-2` and `flash-mode-3` additionally
gate `net.rs` and `bmap.rs`.

One new dependency, pulled in by `flash-mode-1` only:
`uuid = { version = "1.11", default-features = false, features = ["v4"] }`.

`default = ["core", "flash-mode-1"]`, mirroring the legacy recipe, which installs
`flash-mode-1` unconditionally and gates 2 and 3 on `DISTRO_FEATURES`. This also
resolves the current mismatch where the project `CLAUDE.md` feature table lists
`flash-mode-1/2/3` but `Cargo.toml` defines none of them.

## 4. Mode 1 — clone to another disk

### 4.1 Sequence

1. Read `flash-mode-devpath`, resolve symlinks. Clear `flash-mode` and
   `flash-mode-devpath`.
2. Validate the required build-time constants: `DATA_SIZE` always; on U-Boot
   also `UBOOT_ENV1_START`, `UBOOT_ENV2_START`, `UBOOT_ENV_SIZE`.
3. Wait for the destination block device, bounded (§7).
4. Reject an empty destination, a destination that is not a block device, and a
   destination equal to the source. The last check compares the resolved
   destination against the resolved booted device (`/dev/omnect/rootblk`), so a
   symlink or an alias spelling of the running disk is caught too.
5. `sync`, then unmount `/sysroot` completely — the boot partition first, then the
   rootfs. Both are mounted by `mount_core_partitions` on both bootloaders. The
   boot unmount is needed so the `dd` of the boot partition reads a consistent
   image; the rootfs unmount is needed so step 12 does not run `e2image` against a
   filesystem the kernel currently has mounted, with live superblock and journal
   state.
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
11. Assign fresh partition UUIDs to destination `boot` and `rootA`.
12. Copy the running rootfs into destination `rootA` with
    `e2image -ra -p /dev/omnect/rootCurrent`.
13. Write the default bootloader environment to the destination:
    - GRUB: mount the destination boot partition, copy
      `/etc/omnect/grubenv.in` to `EFI/BOOT/grubenv`, unmount;
    - U-Boot: write `/etc/omnect/uboot-env.bin` at both `UBOOT_ENV1_START` and
      `UBOOT_ENV2_START`, as legacy does. Legacy comments this as enforcing a
      redundant environment even when the initial wic had only one, which is not
      what it achieves: U-Boot reads a second copy only when its own build
      configures a redundant environment, and writing bytes to the offset does
      not change that configuration. Where the build does configure one, the
      second write is required. See §10.7.
14. EFI handling on the destination (§6).
15. `sync`.

Steps 9 and 10 keep the legacy order — reformat before copying the other
partitions.

Log persistence and the terminal action sit in `mod.rs`, around this sequence,
not inside it: the log is written whether the sequence succeeded or failed, and
`poweroff` follows only on success (§8.1, §8.3). This mirrors the legacy split
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
bring up the network, flash, EFI handling, `sync`, `reboot`.

### 5.1 Unmounting

`sync`, then unmount `/sysroot` completely — boot partition first, then rootfs, on
both bootloaders (§2.3). Then unmount every remaining mount point backed by the
target disk, by sweeping `/proc/mounts`.

### 5.2 Network setup (`net.rs`)

Restricted to `eth0`, as in legacy. Bring the interface up, start `dhcpcd`, wait
for an IPv4 address — all waits bounded (§7). Mode 2 additionally mounts
`devpts` and starts `dropbear -R`, generating the host key at runtime.

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
6. EFI handling (§6), `sync`, log (§8), `reboot`.

Legacy mode 3 computes a `dest_blk` and a partition suffix from `rootA` and never
uses them; not ported.

### 5.4 Mode 2 — scp push

Trigger: `flash-mode == 2`, **or** the presence of `/etc/enforce_flash_mode`, the
flag file shipped by `omnect-os-initramfs-test`. Both are kept.

1. Clear `flash-mode`.
2. Unmount (§5.1), network up plus `dropbear` (§5.2).
3. Create the image FIFO at `/home/omnect/wic.xz`, owned by the `omnect` user, so
   `scp` streams directly into `bmaptool`.
4. Log the two commands the operator must run, with the acquired IP address:
   `scp <bmap-file> omnect@<ip>:wic.bmap` and
   `scp <wic-image> omnect@<ip>:wic.xz`.
5. Wait for `/home/omnect/wic.bmap` to appear, bounded but with a generous
   timeout — this waits for a person (§7).
6. Flash, according to `DIRECT_FLASHING`:
   - **`false`** — verify pass first: `bmaptool copy --bmap wic.bmap wic.xz wic`,
     which consumes the FIFO and materializes the mapped, decompressed image as a
     file in the initramfs tmpfs. Then zero the first `DD_ZERO_SIZE` KB of the
     disk, then flash from the materialized file. The RAM cost of the verify pass
     is the size of the mapped image; that cost is why the direct path exists.
   - **`true`** — zero the first `DD_ZERO_SIZE` KB, then `bmaptool` straight from
     the FIFO onto the disk. No verification.
7. EFI handling (§6), `sync`, log (§8), `reboot`.

One wait in mode 2 cannot be bounded by a timeout constant: once `bmaptool`
starts, it blocks reading the FIFO until the operator's `scp` feeds it. Bounding
that means running `bmaptool` as a child with a watchdog that kills it if no data
arrives, using the same `wic.bmap` timeout from §7. Without the watchdog, mode 2
keeps one unbounded hang path even though every wait we own is bounded. The
watchdog is part of this design; it is called out here because it is the only
place where bounding a wait needs more than a timeout on our own loop.

The zeroing step is the legacy `non_bmap_dd_handling`. Its comment records
post-flash boot failures observed on both GRUB (mismatched `bootx64.efi`
checksums) and U-Boot (boot-partition errors after `bmaptool`). It is ported —
see §10.1.

## 6. EFI handling

Applies on machines whose `MACHINE_FEATURES` contains `efi`. Ported from
`flash_mode_efi_handling` in `common-sh`, unchanged:

1. Delete every existing EFI boot entry.
2. Mount the target boot partition — the destination's for mode 1, the running
   disk's for modes 2 and 3.
3. Create an `omnect_os` entry pointing at `\EFI\BOOT\bootx64.efi` on partition 1
   of the target disk.
4. Write `efibootmgr -v` output to `EFI/BOOT/efibootmgr_entry` on the boot
   partition.
5. Unmount.

The legacy duplicate entry — a second entry with the same loader and the label
`"omnect_os "`, differing only by a trailing space — is not ported (§10.2).

Item 1 is in §10.3.

## 7. Bounded waits

Every wait is bounded by a named constant and logs progress while waiting. On
timeout the mode fails into the normal fatal-error path (§8).

| Wait | Legacy | Proposed bound | Rationale |
|---|---|---|---|
| Mode 1 destination block device | 30 s, off-by-one bug | 30 s | unchanged, bug fixed |
| Interface up | unbounded | 60 s | machine-driven, should be immediate |
| DHCP IPv4 address | unbounded | 120 s | covers a slow DHCP server |
| Mode 2 `wic.bmap` arrival | unbounded | 30 min | waits for a person to start the `scp` |

The values are proposals — reviewers should say if any is wrong for their
machines. Each becomes a named constant.

Because `flash-mode` is already cleared, a power cycle recovers a stuck device in
both the legacy and the ported behaviour. Bounding the waits turns a silent hang
into a diagnosable failure and satisfies the project's no-magic-numbers rule.

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
| Boot env read failure | Log warn → Normal boot |
| Unknown `flash-mode` value | Log warn → Normal boot |
| `flash-mode` clear failure | Log warn → continue; the mode may repeat on the next boot |
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
| EFI handling failure | Fatal |
| Log persistence failure | Log warn → continue |

"Fatal" means the mode aborts into §8.1's failure path. For mode 1 the source
disk is untouched, so a power cycle boots normally. For modes 2 and 3 the disk is
left half-written, which is unavoidable for a whole-disk flash.

### 8.3 Logging

One capture mechanism for all three modes, mirrored to kmsg and the console as it
runs. Persistence depends on whether a safe target exists:

- **Mode 1** — the log is written to the **source** data partition,
  unconditionally, as legacy does. That disk was never written to, so this is
  safe on both success and failure.
- **Modes 2 and 3** — the whole disk is overwritten. Before flashing the outcome
  is not yet known; after a failure the disk is in an unknown half-written state
  and mounting anything on it is unsafe. Persistence is therefore best-effort
  onto the freshly written data partition after a **successful** flash only. On
  failure these modes leave nothing on disk, the same as legacy, and diagnosis
  stays on kmsg and the console.

See §10.5.

## 9. Intentional deviations from the legacy scripts

Two bugs in `flash-mode-1`, fixed rather than reproduced:

1. **Destination-device wait off-by-one.** The loop is
   `for i in $(seq 1 30); do if [ -b "${blk_dev_dst}" ]; then break; fi; ...; done`
   followed by `if [ ${i} -eq 30 ]; then stderr_fatal ...`. When the device
   appears on the 30th iteration, `i` is 30 and the script reports failure even
   though the device is present.
2. **DOS extended-partition start read from the wrong path.** Line 133 calls
   `get_start_sector $(readlink -f extended)` with a relative path, where every
   sibling call passes `/dev/omnect/...`. `get_start_sector` matches its argument
   against an `sfdisk -d` dump of the root block device, so the relative path
   cannot match and the extended-partition size calculation is wrong on DOS
   machines.

Also not ported:

- the redundant `cut -d= -f2` on `flash-mode-devpath` (§2.1);
- the unused `dest_blk` / partition-suffix computation in `flash-mode-3` (§5.3);
- the hardcoded partition indices and the `p`-suffix probe (§2.6).

Behaviour changes, as opposed to bug fixes:

- unbounded waits become bounded (§7);
- every mode unmounts `/sysroot` fully before writing, because the Rust flow mounts
  it before dispatch and legacy did not (§2.3). Without this, mode 1 would image a
  mounted `rootCurrent`;
- a queued factory reset combined with a flash mode is now an error. Legacy ran
  both (86 then 87); single-mode dispatch cannot, and refuses the pair rather
  than dropping one silently (§3.3, §10.6);
- modes 2 and 3 may persist a log where legacy did not (§8.3, §10.5).

## 10. Decisions required from reviewers

**Blocking.** Each item states the default, which is the conservative choice.
Reviewers must confirm or overturn each one before implementation starts. Items
already decided in review are marked **Decided** and the rest of the spec
follows that decision.

### 10.1 Keep `non_bmap_dd_handling`?

Zeroing the first `DD_ZERO_SIZE` KB of the disk before flashing in mode 2. The
legacy comment records post-flash boot failures observed on both GRUB and U-Boot,
but the root cause was never established, so this may be masking a `bmaptool` or
partition-alignment problem rather than fixing one.

**Default: keep.**

### 10.2 Keep the duplicate EFI boot entry?

`flash_mode_efi_handling` creates two entries pointing at the same loader,
differing only by a trailing space in the label, commented as "for debug
purposes, when booting after flash-mode-{1,2} fails".

**Decided: drop it.** EFI updates since then have made it unnecessary; the port
creates one entry and the result is to be confirmed by test (§6).

### 10.3 Keep deleting every existing EFI boot entry?

The current handling removes all EFI boot entries on the machine before creating
its own, including entries unrelated to omnect.

**Default: keep — it is what ships today.**

### 10.4 Uniform `reboot`, including mode 1?

Mode 1 currently powers off on success.

**Default: keep `poweroff` for mode 1** — it leaves a cloned disk an operator
must move, and a reboot would come back up on the source disk.

### 10.5 Persist a log for modes 2 and 3 at all?

The default in §8.3 is a best-effort write after a successful flash, which costs
an extra mount of a just-written partition and yields nothing on the failures
where a log would help most.

**Default: best-effort after success.** Overturning this means kmsg and console
only, exactly like legacy.

### 10.6 Should a queued factory reset still run before mode 1?

Legacy ran `init.d/86-factory-reset` before `init.d/87-flash_mode_1`, so both
happened: the source disk was reset, then cloned. Single-mode `BootMode` dispatch
gives one handler and cannot express that.

**Decided: reject the combination with an error.** A factory reset acts on the
booted device and a mode-1 clone on another one; requesting both was never
intended, and silently performing only one of two destructive requests is the
worse failure. Both triggers stay set so the operator can clear one and retry
(§3.3, §9).

### 10.7 Keep writing the U-Boot environment to both offsets?

Mode 1 step 13 copies `uboot-env.bin` to `UBOOT_ENV1_START` and
`UBOOT_ENV2_START`. The legacy comment claims this enforces a redundant
environment even when the initial wic had only one. It does not: U-Boot uses a
second copy only when its build configures a redundant environment. Where the
build does configure one, writing only the first offset would leave the second
holding whatever the clone inherited, so the second write is needed there.

**Default: keep both writes, drop the claim.** The legacy script already
requires all three `UBOOT_ENV*` values to be defined, so a U-Boot build reaching
mode 1 always has a second offset configured, and the two writes are consistent
with it. Overturning this means deciding what a build without a redundant
environment should do — which needs an example of such a build first.

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
| Detection: flash mode takes precedence over factory reset | unit | `src/mode/mod.rs` |
| Detection: mode 2 flag-file trigger, present and absent | integration | `tests/flash_modes.rs` |
| Clear-first ordering, asserted via `set_env_calls` | integration | `tests/flash_modes.rs` |
| Boot-env read failure falls back to Normal boot | integration | `tests/flash_modes.rs` |

`tests/flash_modes.rs` follows `tests/factory_reset.rs` and uses the existing
`MockBootEnv`.

## 12. meta-omnect companion work

Implemented separately, listed here so nothing is lost:

- pass `OMNECT_PART_OFFSET_BOOT`, `OMNECT_PART_SIZE_BOOT` and
  `OMNECT_FLASH_MODE_2_DIRECT_FLASHING` into the `omnect-os-init` build
  environment, the same way the existing five constants are passed;
- map `DISTRO_FEATURES` `flash-mode-2` and `flash-mode-3` onto the corresponding
  Cargo features;
- keep `FLASH_MODE_X_PACKAGES` plus `dropbear` and `curl` gated as they are
  today, and keep the `omnect_user` class inherited for mode 2;
- retire `init.d/87-flash_mode_{1,2,3}` and the `sed` substitutions in
  `omnect-os-initramfs-scripts.bb` once the Rust path ships;
- `util-linux-uuidgen` can be dropped from the initramfs once the port ships:
  `uuidgen` is called from `flash-mode-1` and nowhere else, and the Rust port
  generates the UUID itself (§2.8);
- **no other package changes needed.** Verified against `buildhistory` for a built
  `omnect-os-initramfs`: every tool the three modes need is already installed
  (§2.8). In particular `e2image` ships in the base `e2fsprogs` package at
  `/usr/sbin/e2image`, which `PACKAGE_INSTALL` already pulls in, so the fact that
  the recipe names only `e2fsprogs`, `e2fsprogs-mke2fs` and `e2fsprogs-tune2fs` is
  not a gap.

## 13. Interactions

This spec is written against `upstream/main` at `d1168de`. The factory-reset wipe
modes 2, 3 and 4 are being designed in parallel on
`feat/factory-reset-wipe-modes`. Both add `BootEnvKey` variants and both touch
`BootMode` and `src/lib.rs` dispatch, so those three places are the expected
merge points. The naming pinned in §2.4 exists to keep `FlashMode` and
`ResetMode` distinct once both land.
