# v2 Bootloader — frozen trampoline, golden core, sectioned updates

Status: proposed. This document amends the partition rule in
`Firmware/RUST_PORT_HARDWARE_CONTRACT.md` (line 16 currently mandates retaining
the stock C bootloader unchanged and forbids a rewrite). All flash addresses
and sizes below are for the STM32F070RB (128 KiB flash, 16 KiB RAM,
1 KiB erase pages).

## Goal

Replace the stock 32 KiB bootloader reservation with a smaller, safer, fully
USB-updatable boot architecture, so the application partition grows from
92 KiB to ~105-116 KiB. Every update after initial installation must be
power-cut-immune (no window where a brown-out bricks the device), with
rollback to a golden recovery core. Users without SWD must be able to
migrate from v1 using only the fork's GUI/CLI tooling.

## Why now

The Rust application image is 92,672 bytes of the 94,208-byte (92 KiB)
partition — 1,536 bytes free, and the release gate (`build_image.sh`) requires
at least 1,024. That is 512 bytes of margin; the next modest feature fails
the build. The only recoverable flash is the stock bootloader's 32 KiB
reservation: the settings page (0x0801f000..0x0801f7ff) and boot-metadata
page (0x0801f800..0x0801ffff) are fixed at the top of the part, and the app
cannot grow upward.

## Hardware constraints that shape the design

- Single-bank flash, no VTOR on Cortex-M0. The stock bootloader's
  vector-copy-to-SRAM + remap mechanism (contract line 12) remains the boot
  mechanism.
- The flash controller stalls instruction fetch during erase/program; code
  that writes flash must poll from RAM. The application already uses this
  pattern for its settings journal (`main.rs:77-88`); the boot cores reuse it.
- A power cut during the erase/program of the vector-table page (page 0)
  destroys the only bootable code. Nothing running from flash can recover,
  because USB enumeration itself requires intact code. This is the one
  physically unavoidable window on this silicon, and the design confines it
  to a single one-time event (see Migration).

## Flash map

| Region | Range | Size | Notes |
|---|---|---|---|
| Trampoline | 0x08000000 | 1 KiB | Frozen forever after migration |
| Core slot A ("golden") | 0x08000400 | ~6 KiB | Minimal USB flasher, never updated |
| Core slot B ("working") | 0x08001C00 | ~12 KiB | Full boot core + app updater |
| Application | 0x08004800 | ~105 KiB | (vs 92 KiB today) |
| Settings page | 0x0801F000 | 2 KiB | Unchanged |
| Boot metadata page | 0x0801F800 | 2 KiB | Extended, see below |

Exact slot sizes are set by the implementations; the budget to beat is
application ≥ 100 KiB with both slots resident. Final sizes are validated by
the CI fit check.

## Trampoline page (frozen)

Page 0 contains, at fixed addresses:

- Initial SP and reset vector (the only entries hardware fetches).
- A ~20-instruction trampoline: read the boot-slot flag word from the
  metadata page, then jump to the selected slot's fixed entry address.
- Flag semantics: erased/unknown flag word → **slot A** (safe default); a
  valid "slot B" mark (programmed 1→0) → slot B. A flag word torn by power
  loss therefore falls back to golden.

The page is written exactly once per device (during v1→v2 migration) and is
never erased or programmed again. Any change to the trampoline itself
requires SWD; it is deliberately tiny and versionless. `layout_version` in
the metadata page guards against images built for a different map.

## Boot flow and fail-safe rules

1. Hardware fetches SP/reset from the trampoline.
2. Trampoline dispatches on the flag word (default golden).
3. Selected core verifies itself, then verifies the application:
   CRC + length in the metadata page, entry vector inside the application
   partition. Invalid → the core stays resident and presents the USB updater.
4. Boot-attempt counter in metadata: the application marks itself healthy
   once the main loop has proven healthy (the same signal the settings
   compaction path already waits on). N failed boots without a health mark →
   the core stays in updater mode instead of looping.

`JUMP:BOOTLOADER` (SCPI, existing) reboots into the golden core's updater.

## Update operations

The v2 protocol is sectioned; the wire framing stays compatible with the
existing ACK/DATA/CRC uploader so the Python test suite and GUI logic port
rather than rewrite.

- **Application update (the default path):** core erases/programs only
  application pages. Identical risk model to today — a failed app update
  leaves a recoverable device in updater mode. No interlock required.
- **Core slot B update:** stream the new core into slot B (the running core
  never touches its own pages), verify-after-page, CRC the slot, then flip
  the flag word — a single word *program* into the metadata page, which is
  never erased during normal operation (programming can only clear bits, so
  an interrupted flag write degrades to the golden default). Requires the
  physical interlock. Power cut at any point → previous boot path intact →
  retry. There is no unrecoverable window.
- **Golden core (slot A):** never updated by design. It enumerates USB and
  can rewrite slot B, the application region, and the flag. It may erase the
  metadata page (the only context allowed to) as an explicit destructive
  command. It refuses the settings page unless that same destructive command
  explicitly names it.
- **Settings page:** unchanged ownership — written only by the application,
  under the existing journal discipline.

## Physical interlock

Writing any boot-core page or erasing the metadata page is authorized only
when the device enters updater mode with the encoder held down at plug-in.
Routine application updates never enter this state, so no host bug or rogue
script can reach the dangerous paths during normal use.

## Failure matrix

| Interruption point | Resulting state | Recovery |
|---|---|---|
| App update, any point | Old app CRC-invalid or valid | Updater mode; re-flash app over USB |
| Core B update, before flag flip | Old boot path intact | Retry |
| Flag flip (torn word) | Trampoline sees unknown flag | Boots golden |
| Core B fails self-check or N boot attempts | Flag still points at B | Golden stays reachable via JUMP:BOOTLOADER / interlock updater |
| Metadata erase (golden, deliberate) | Flag erased | Boots golden; rewrite slot B/app as needed |
| Power cut during migration page-0 write | No bootable code | SWD only (one-time, tens of ms) |

## Migration from v1 (no SWD required)

The stock bootloader only erases/programs the application region above
0x08008000, so page 0 is unreachable through it. Migration therefore uses a
one-time migrator image:

1. GUI/CLI (legacy mode, unchanged wire protocol): flash `migrator.bin` —
   the v2 core packaged as an ordinary application image at 0x08008000.
   The stock bootloader flashes and runs it like any app.
2. Migrator verifies the image, writes trampoline-adjacent pages 1..N into
   the (currently empty) reservation first, verifies, then writes page 0
   **last**, then invalidates the boot-metadata page, then resets.
   The stock bootloader remains intact until the single page-0 write, so a
   power cut before that instant is retried by simply re-running the flow.
3. The v2 trampoline boots, sees invalid metadata/layout, and sits in USB
   updater mode. The GUI detects the v2 USB identity and flashes the real
   v2 application.

The only unrecoverable instant is a power cut inside the page-0
erase→program→verify sequence (tens of milliseconds, once per device
lifetime). Documentation and the GUI prompt must state "keep the device
plugged in" for this step; devices that lose power in that instant require
SWD.

The migrator's core-swap machinery is the same code the golden core uses for
slot-B updates — built once, exercised by migration, reused forever.

## Host tooling changes (all inside this fork)

- `tools/flash_firmware.py` and the GUI update tab gain dual mode
  (legacy stock-bootloader protocol; v2 sectioned protocol), with the active
  mode detected from the device's USB identity so the migration's two stages
  cannot be confused.
- GUI gates core-section operations behind the interlock flow and shows the
  "do not unplug" state during the migrator's page-0 write.
- Release artifacts during transition: `migrator.bin` (one-time),
  `benchvolt-v2.bin` (sectioned), and `benchvolt-pd.bin` for stock-bootloader
  devices until the fleet is migrated, then deprecated.
- CI replaces the bootloader/app partition checks with a sectioned fit check
  (trampoline + slots + app + 4 KiB reserved ≤ 128 KiB); `build_image.sh`
  emits the sectioned image with per-section CRCs.
- Documentation: README flashing runbook and SCPI/boot contract notes.

## Testing

- Host-side protocol simulation: the Python uploader tests (already run in
  `check.sh`) drive a simulated sectioned-update state machine end to end.
- Rust host tests for the boot-decision logic (flag semantics, CRC, boot
  counter, layout-version checks) via the existing `--no-default-features`
  harness.
- Hardware checklist: brown-out injection at each failure-matrix row on a
  bench with SWD attached, before any no-SWD release.

## Rollout

- M0: this plan + contract amendment merged.
- M1: trampoline, golden core, working core, sectioned protocol; CI fit
  checks.
- M2: migrator + dual-mode GUI/CLI + release pipeline.
- M3: release with transitional artifacts; migrate own units (SWD or GUI).
- M4: deprecate the v1 flashing path once migrated.

## Contract amendment (replaces line 16 paragraph)

> The stock C bootloader is superseded by the v2 boot architecture described
> in `Docs/v2-bootloader-design.md`: a frozen trampoline page, a never-updated
> golden recovery core, and an updatable working core, with the application
> partition extended accordingly. The upload protocol remains USB CDC,
> keeps the ACK/DATA/CRC framing, and is extended with sectioned addressing.
> Erase/write validation must reject any range outside the image's declared
> sections; settings-page access from boot code requires an explicit
> destructive command; and boot-core writes require the physical interlock.
> The settings page, boot-metadata page location, and the
> vector-copy-to-SRAM boot mechanism are retained unchanged.

## Open items

- Final slot sizes after implementation (golden with raw-register USB vs
  usb-device; target golden ≤ 6 KiB, working ≤ 14 KiB).
- Whether slot A adopts raw-register USB from the start or usb-device first
  and shrinks later (start simple; the fit check enforces the budget).
- Encoder-hold vs long-press as the interlock gesture.
- GUI copy for the migration warning and the "do not unplug" state.
