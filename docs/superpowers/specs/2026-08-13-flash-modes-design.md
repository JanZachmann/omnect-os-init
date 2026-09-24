# Flash Modes 1, 2, 3 — Design

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
  rawio.rs      in-process replacement for every `dd` call               flash-mode-1
  unmount.rs    `/sysroot` teardown and `/proc/mounts` sweep             flash-mode-1
  net.rs        interface up, dhcpcd, dropbear                           flash-mode-2/3
  bmap.rs       bmaptool wrapper                                         flash-mode-2/3
  scp.rs        mode 2 orchestration                                     flash-mode-2
  url.rs        mode 3 orchestration                                     flash-mode-3
```

The right column is the gating feature (§3.6). `rawio.rs` and `unmount.rs` were
not anticipated in the original design; both hold logic modes 2 and 3 are
expected to reuse (the byte-offset copy, and the `/sysroot`-plus-`/proc/mounts`
teardown of §5.1), but each is gated on `flash-mode-1` for now, since mode 1 is
the only mode implemented so far. Widening the gate is expected once mode 2 or
3 lands.

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

Mode 2 adds two through the same mechanism:

| Yocto variable | Constant | Note |
|---|---|---|
| `OMNECT_PART_OFFSET_BOOT` + `OMNECT_PART_SIZE_BOOT` | `ZERO_HEAD_SIZE` | `Option<u64>`, sum in KB |
| `OMNECT_FLASH_MODE_2_DIRECT_FLASHING` | `DIRECT_FLASHING` | `bool`, `1` → `true`, anything else → `false` |

`ZERO_HEAD_SIZE` is `Option<u64>` and absent on builds that do not set it, exactly
like the existing five; mode 2 treats it as a missing required constant.
`DIRECT_FLASHING` is a plain `bool` defaulting to `false` when the variable is
absent or is not `1`, matching the legacy
`oe.utils.conditional('OMNECT_FLASH_MODE_2_DIRECT_FLASHING', '1', 'true', 'false')`.
The legacy recipe computes the sum with `bc` because bitbake does not evaluate
shell arithmetic; `build.rs` sums the two values itself and needs only the two
Yocto variables.

`UBOOT_ENV2_START` stays `Option<u64>` rather than required: it is set per
machine and absent where no second environment bank is reserved, which is
exactly the condition for skipping the second write (§10.7).

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
`efibootmgr` (EFI variables), `curl`, `dhcpcd` and `dropbear`.

Six operations the legacy scripts shell out for are done in-process instead.
Four use `nix`, which is already a dependency with the required features
enabled; `uuidgen` needs the new `uuid` crate; `dd` needs nothing:

| Legacy | In-process |
|---|---|
| `uuidgen` | `uuid::Uuid::new_v4` |
| `dd` | `std::io` read/write at an offset, with `COPY_BUFFER_SIZE` as the buffer |
| `mkfifo` | `nix::unistd::mkfifo` |
| `chown omnect:omnect` | `nix::unistd::chown`, with the uid/gid looked up via the `user` feature |
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

Two more `rerun-if-env-changed` lines and two more generated constants for mode 2
(§2.7): `ZERO_HEAD_SIZE`, summed from `OMNECT_PART_OFFSET_BOOT` and
`OMNECT_PART_SIZE_BOOT`, and `DIRECT_FLASHING`. The existing `read_u64_env`
helper covers the first; the second needs a small boolean reader. The
doc-comment table at the top of `build.rs` gains both rows.

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

The refusal reaches kmsg only. A flash boot never writes the ODS status file:
`run_init` returns the error and the fatal-error path just logs it, so there is
no status file for the refusal to appear in.

Note also that the queued `factory-reset` key does not survive modes 2 and 3. On
U-Boot the environment lives at the `UBOOT_ENV1_START`/`UBOOT_ENV2_START` byte
offsets, and mode 2's own zeroing of the first `ZERO_HEAD_SIZE` KB reaches through
that region; on GRUB, `grubenv` sits on the boot partition, which the flash
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

Reviewers want mode 1 gated as well. That changes what ships, so it is recipe
work tracked in §12 rather than part of the port.

## 4. Mode 1 — clone to another disk

### 4.1 Sequence

1. Read `flash-mode-devpath`. Clear `flash-mode` and `flash-mode-devpath`.
2. Validate the required build-time constants: `DATA_SIZE` always; on U-Boot
   also `UBOOT_ENV1_START` and `UBOOT_ENV_SIZE`. `UBOOT_ENV1_START` is also
   required whenever `BOOTLOADER_START` is set, independently of the
   bootloader feature, because step 8 computes the bootloader-area copy
   length as `UBOOT_ENV1_START - BOOTLOADER_START`. `UBOOT_ENV2_START` is
   optional (§10.7).
3. Reject an empty destination, and one equal to the source or to a partition
   of it, on the value as given. These need no device node, so they run ahead
   of the wait and report a misconfiguration at once instead of after the full
   timeout.
4. Wait for the destination block device, bounded (§7). Then resolve the
   destination path — once, here, because resolving needs the node to exist —
   and use the resolved path for every step that follows. Repeat the checks of
   step 3 on it, so an alias spelling of the running disk is caught too, and
   reject a destination that is not a block device.
5. `sync`, then unmount `/sysroot` completely — the boot partition first, then the
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
5. Wait for `/home/omnect/wic.bmap` to appear, unbounded — this waits for a
   person (§7).
6. Flash, according to `DIRECT_FLASHING`:
   - **`false`** — verify pass first: `bmaptool copy --bmap wic.bmap wic.xz wic`,
     which consumes the FIFO and materializes the mapped, decompressed image as a
     file in the initramfs tmpfs. Then zero the first `ZERO_HEAD_SIZE` KB of the
     disk, then flash from the materialized file. The RAM cost of the verify pass
     is the size of the mapped image; that cost is why the direct path exists.
   - **`true`** — zero the first `ZERO_HEAD_SIZE` KB, then `bmaptool` straight from
     the FIFO onto the disk. No verification.
7. EFI handling (§6), `sync`, log (§8), `reboot`.

Once `bmaptool` starts it blocks reading the FIFO until the operator's `scp`
feeds it, and that wait stays unbounded too: a timeout there would kill a flash
in progress and leave the disk half-written (§10.8).

The zeroing step is the legacy `non_bmap_dd_handling`. Its comment records
post-flash boot failures observed on both GRUB (mismatched `bootx64.efi`
checksums) and U-Boot (boot-partition errors after `bmaptool`). It is ported —
see §10.1.

## 6. EFI handling

Applies on machines whose `MACHINE_FEATURES` contains `efi`. Ported from
`flash_mode_efi_handling` in `common-sh`, unchanged:

1. Delete every EFI boot entry, active or not. `flash_mode_efi_handling` greps
   with an unquoted `^Boot[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]\*`: the
   shell turns `\*` into `*`, which `grep` reads as "zero or more" of the last
   hex class, so every `Boot####` line matches, with or without the `*`
   active marker.
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
- **Modes 2 and 3** — the whole disk is overwritten. Before flashing the outcome
  is not yet known; after a failure the disk is in an unknown half-written state
  and mounting anything on it is unsafe. Persistence is therefore best-effort
  onto the freshly written data partition after a **successful** flash only. On
  failure these modes leave nothing on disk, the same as legacy, and diagnosis
  stays on kmsg and the console.

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
- `dd` is replaced by in-process file I/O (§2.8);
- every mode unmounts `/sysroot` fully before writing, because the Rust flow mounts
  it before dispatch and legacy did not (§2.3). Without this, mode 1 would image a
  mounted `rootCurrent`;
- a queued factory reset combined with a flash mode is now an error. Legacy ran
  both (86 then 87); single-mode dispatch cannot, and refuses the pair rather
  than dropping one silently. Both triggers are cleared before the error, so a
  power cycle boots normally (§3.3, §10.6);
- modes 2 and 3 may persist a log where legacy did not (§8.3, §10.5);
- the `check_fs` on the source data partition before the log mount is dropped.
  Legacy runs it in `flash_mode_1_run` just before mounting; the port mounts
  directly. Bounded: the log write is best-effort either way, so a mount that
  fails only warns (§8.2);
- the console tee is lost. Legacy pipes the whole run through
  `tee … >/dev/console`, so the operator at the device sees every line. The
  port emits `log::info!` to `/dev/kmsg` only, which a `quiet` boot keeps off
  the console. `e2image` progress still reaches it, because the kernel gives
  PID 1 `/dev/console` as its standard streams and the copy inherits them.
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

Zeroing the first `ZERO_HEAD_SIZE` KB of the disk before flashing in mode 2. The
legacy comment records post-flash boot failures observed on both GRUB and U-Boot,
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

### 10.4 Uniform `reboot`, including mode 1?

Mode 1 currently powers off on success.

**Decided: keep `poweroff` for mode 1** — it leaves a cloned disk an operator
must move, and a reboot would come back up on the source disk.

### 10.5 Persist a log for modes 2 and 3 at all?

§8.3 specifies a best-effort write after a successful flash, which costs
an extra mount of a just-written partition and yields nothing on the failures
where a log would help most.

**Decided: best-effort after success.** The alternative would be kmsg and
console only, exactly like legacy.

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
- gate mode 1 the same way: map `DISTRO_FEATURES` `flash-mode-1` onto the Cargo
  feature and drop `flash-mode-1` from `default`, so a machine that wants disk
  cloning has to ask for it. This changes what ships, so it is recipe work rather
  than part of the port;
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
