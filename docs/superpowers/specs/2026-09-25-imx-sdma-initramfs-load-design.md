# Design: Load `imx_sdma` in the Initramfs on i.MX8MM

**Date:** 2026-09-25
**Status:** Draft, for review
**Scope:** omnect-os-init — a new early step after the rootfs mount, a
`firmware_class.path` restore before `switch_root`, one Cargo feature.
meta-omnect sets the feature for i.MX8MM.

---

## 1. Problem

On i.MX8MM the SDMA driver is built as a module (`CONFIG_IMX_SDMA=m`). Its
firmware, `imx/sdma/sdma-imx7d.bin`, is in the rootfs and cannot be built into
the kernel because of its license. Several built-in drivers use SDMA:
`spi-imx` defers its probe until the SDMA controller exists, and the i.MX UART
driver requests its DMA channel when a port is opened.

The Rust initramfs does not load the module, so udev loads it in the rootfs,
after systemd has started. Measured on a phyGATE Tauri-L (`phygate-tauri-l-imx8mm-2`):

| Time | Event |
|---|---|
| 4.5 s | systemd starts |
| 10.27 s | udev loads `imx_sdma`, the firmware loads |
| 10.32 s | TPM on `spi0.1` appears |
| 10.95 s | CAN controller `can0` on `spi0.0` appears |

A unit ordered on its device (`aziot-tpmd` requires `dev-tpmrm0.device`) is
not affected. Anything that is not ordered on its device is: an application
that opens `can0` early, or a UART opened before SDMA exists, which then runs
without DMA until it is opened again.

## 2. Legacy behaviour

meta-omnect installed `init.d/90-imx_sdma` for `mx8mm-nxp-bsp`, with the
reason "load imx_sdma in initramfs to prevent race conditions with drivers
using sdma". After the rootfs mount it bind-mounted the rootfs `/lib/modules`
and `/lib/firmware` into the initramfs and ran `modprobe imx_sdma`. It did not
unmount them ("Device or resource busy"), and it ignored every error.

## 3. Goals and non-goals

Goals:

- `imx_sdma` and its firmware load before `switch_root` on i.MX8MM, so the
  devices behind SDMA exist when systemd starts.
- A missing module or firmware never stops the boot and never adds a
  firmware-loader timeout.
- Images for other machines do not change.

Non-goals:

- A general mechanism to load arbitrary modules in the initramfs. Section 9
  lists this as an open question.
- Changing the kernel configuration or the firmware packaging.

## 4. Overview

A new step, `early_modules::load_imx_sdma(rootfs)`, runs in `run_init` right
after `mount_core_partitions` has mounted the rootfs, and before anything
else. It:

1. skips when `/sys/module/imx_sdma` exists (already loaded, or built in);
2. points the kernel firmware search at the rootfs: it saves the current value
   of `/sys/module/firmware_class/parameters/path` and writes
   `<rootfs>/lib/firmware` to it;
3. opens `<rootfs>/lib/modules/<release>/kernel/drivers/dma/imx-sdma.ko`, with
   `<release>` read from `/proc/sys/kernel/osrelease`, and loads it with
   `finit_module(2)`.

`mode::normal::run` restores the saved `firmware_class.path` value right
before `switch_root`. Every boot path that reaches `switch_root` goes through
`mode::normal::run`; the factory-reset mode ends there too.

The step is compiled only with the Cargo feature `imx-sdma`.

## 5. Why no bind mounts and no `modprobe`

- `finit_module` takes a file descriptor, so the module is read directly from
  the rootfs. `imx-sdma.ko` has no dependencies (`modinfo`: `depends:` is
  empty) and the kernel builds modules uncompressed
  (`CONFIG_MODULE_COMPRESS_NONE=y`), so `modprobe` adds nothing. The initramfs
  needs no `kmod` package; `nix` needs its `kmod` feature.
- The kernel reads firmware relative to the root of PID 1
  (`kernel_read_file_from_path_initns`). Before `switch_root` that is the
  initramfs, which has no `/lib/firmware`. `firmware_class.path` is the first
  entry the loader tries, and it can be changed at runtime (mode 0644), so
  writing the rootfs path to it replaces the legacy bind mount of
  `/lib/firmware`. Nothing is left mounted in the initramfs.

## 6. The asynchronous firmware load

`sdma_probe` requests the firmware with `request_firmware_nowait`, so
`finit_module` returns before the firmware is read. The read runs on a kernel
workqueue and needs the rootfs path to be valid at that time.

- The rootfs stays mounted at the same path until `switch_root`. On the
  measured device that is at least several hundred milliseconds later, because
  the other partitions are checked and mounted in between. A direct read of
  the 3 KB firmware file finishes well before that.
- If the read ever runs after the restore in section 4, the default paths
  apply. Before `switch_root` they do not exist in the initramfs; with
  `CONFIG_FW_LOADER_USER_HELPER_FALLBACK=y` the loader then waits for the
  sysfs fallback (60 s by default), and the driver retries once and then uses
  its ROM scripts. The boot is not blocked, because the wait runs on the
  workqueue, but SDMA runs without the RAM firmware.

The step does not wait for the firmware. The kernel offers no reliable signal
for "firmware loaded" for this driver, and a wait would add boot time on every
boot to cover a case that the timing already excludes. Section 9 lists this as
a decision for reviewers.

## 7. Error handling

The step is best effort, as in legacy. Every outcome is logged, none is
returned as an error, and none changes the ODS status.

| Outcome | Handling |
|---|---|
| `/sys/module/imx_sdma` exists | `info`, skip |
| module file missing | `warn`, skip, `firmware_class.path` unchanged |
| `firmware_class.path` cannot be read or written | `warn`, skip the load (without the path the firmware load would fall into the 60 s fallback) |
| `finit_module` fails with `EEXIST` | `info` (loaded in the meantime) |
| `finit_module` fails otherwise | `warn`, restore `firmware_class.path` at once |
| restore before `switch_root` fails | `warn`, continue |
| rootfs mount failed | the step does not run |

A release image must never stop in the fatal-error loop because of this step,
so no path returns an error.

## 8. Build and recipe

- omnect-os-init: Cargo feature `imx-sdma = ["core"]`, off by default; `nix`
  gets the `kmod` feature.
- meta-omnect `omnect-os-init.inc`:
  `CARGO_FEATURES:append:mx8mm-nxp-bsp = ",imx-sdma"`. This is the override
  the legacy recipe used, and it is active on the phytec i.MX8MM machines
  (`phytec-imx8mm.inc` selects `linux-phytec-imx` through `mx8-nxp-bsp`).
- No package changes: the module and the firmware come from the rootfs.
- The README table of runtime dependencies gets a row for `imx-sdma.ko` and
  `imx/sdma/sdma-imx7d.bin`, both read from the rootfs.

## 9. Open decisions

1. **Restore `firmware_class.path` before `switch_root`, or leave it set?**
   Restoring leaves the kernel as the rootfs expects it; the cost is the
   corner case in section 6. Leaving it set means a stale path
   (`/rootfs/lib/firmware`) that does not exist after `switch_root`, which the
   loader skips. Proposed: restore.
2. **Cargo feature, or detect SDMA at runtime** (device tree compatible
   `fsl,imx8mq-sdma`)? Detection would also cover other i.MX machines without
   a recipe change, but every image would carry the code. Proposed: Cargo
   feature, as the legacy recipe did.
3. **Only `imx_sdma`, or a list of early modules** set at build time? No other
   module needs this today. Proposed: only `imx_sdma`; generalize when a
   second case appears.

## 10. Testing

Unit tests, with the sysfs, procfs and module paths injectable:

- the module path is built from the rootfs and the release string;
- an existing `/sys/module/imx_sdma` skips the load and leaves
  `firmware_class.path` alone;
- a missing module file skips the load and leaves `firmware_class.path` alone;
- the saved `firmware_class.path` is written back by the restore, including an
  empty value;
- `EEXIST` is treated as loaded; other errors restore the path at once.

`finit_module` itself sits behind a small trait, so the tests never load a
module.

On hardware (phyGATE Tauri-L):

- `dmesg` shows `imx-sdma … loaded firmware` and the TPM on `spi0.1` before
  systemd starts;
- `/sys/module/firmware_class/parameters/path` is empty after boot;
- with `imx-sdma.ko` removed from the rootfs, the device boots normally and
  logs the warning;
- an image for another machine contains no `imx-sdma` code
  (`cargo build` without the feature).

## 11. Comparison to legacy

| | Legacy `90-imx_sdma` | This design |
|---|---|---|
| Gate | `mx8mm-nxp-bsp` override | Cargo feature, set for `mx8mm-nxp-bsp` |
| Module load | `modprobe` from a bind-mounted `/lib/modules` | `finit_module` on the rootfs file |
| Firmware | bind-mounted `/lib/firmware`, left mounted | `firmware_class.path`, restored before `switch_root` |
| Errors | ignored | logged, never fatal |
| Position | after the rootfs mount | after the rootfs mount |
