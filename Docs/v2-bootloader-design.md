# v2 Bootloader — frozen trampoline, golden core, sectioned updates

Status: proposed (rev 2). Supersedes the first revision, which assumed 1 KiB
erase pages and wrote the slot pages before the trampoline page. This document
amends the partition rule in `Firmware/RUST_PORT_HARDWARE_CONTRACT.md`
(line 16 currently mandates retaining the stock C bootloader unchanged and
forbids a rewrite). All flash addresses and sizes below are for the
STM32F070RB: 128 KiB flash organized as **64 pages of 2 KiB** (RM0360; see
also `Flash.c:7` and the 2 KiB settings page in the contract), 16 KiB RAM.

## Goal

Replace the stock 32 KiB bootloader reservation with a smaller, safer, fully
USB-updatable boot architecture, so the application partition grows from
92 KiB to 104 KiB. Every update after initial installation must be
power-glitch-safe (no window where a brown-out bricks the device), with
rollback to a golden recovery core. Users without SWD must be able to migrate
from v1 using only the fork's GUI/CLI tooling.

Power-cut immunity here is **not** about users yanking the cable mid-update.
This device is VBUS-powered and its own sources hard-reset VBUS during PD
renegotiation — the contract records a reset loop caused by exactly that
(contract line 289), and the device renegotiates PD on every reboot. A
spontaneous VBUS dip during a flash write is indistinguishable from an
unplugged cable, so every write path must degrade to a recoverable state by
construction. The design gets this almost for free: on F0 flash, programming
can only clear bits, so erased/blank state is a stable, safe default for
every control word. The single genuinely unrecoverable window is confined to
one tens-of-milliseconds event per device lifetime (see Migration).

## Why now

The Rust application image is 92,672 bytes of the 94,208-byte (92 KiB)
partition — 1,536 bytes free, and the release gate (`build_image.sh`,
`minimum_free = 1024`) requires at least 1,024. That is 512 bytes of margin;
the next modest feature fails the build. The only recoverable flash is the
stock bootloader's 32 KiB reservation: the settings page
(0x0801f000..0x0801f7ff) and boot-metadata page (0x0801f800..0x0801ffff) are
fixed at the top of the part, and the app cannot grow upward.

## Hardware constraints that shape the design

- Single-bank flash, 2 KiB erase pages, no VTOR on Cortex-M0. The
  vector-copy-to-SRAM + remap mechanism (contract line 12) remains the boot
  mechanism; see "Vector-copy chain" below for who performs each copy.
- The flash controller stalls instruction fetch during erase/program; code
  that writes flash must run its polling loop from RAM. The application
  already uses this pattern (`main.rs:74-88`); the migrator and both boot
  cores reuse it.
- Programming can only clear bits (1→0). Words are programmed once after
  erase; whether re-programming an already-programmed word to clear
  additional bits is reliable on F070 must be verified on hardware before the
  boot-record encoding is finalized (see Open items). Every structure in the
  metadata page is designed so that a torn or half-written word falls back to
  the erased default, which always selects the golden core.
- A power cut during the erase/program of the vector-table page (page 0)
  destroys the only bootable code. Nothing running from flash can recover,
  because USB enumeration itself requires intact code. This is the one
  physically unavoidable window on this silicon, and the design confines it
  to a single one-time event (see Migration).

## Flash map

| Region | Range | Size | Pages | Notes |
|---|---|---|---|---|
| Trampoline | 0x08000000 | 2 KiB | 0 | Frozen forever after migration |
| Core slot A ("golden") | 0x08000800 | 6 KiB | 1–3 | Minimal USB flasher, never updated |
| Core slot B ("working") | 0x08002000 | 12 KiB | 4–9 | Full boot core + app updater |
| Application | 0x08005000 | 104 KiB | 10–61 | (vs 92 KiB today) |
| App descriptor | 0x0801EFC0 | 64 B | end of 61 | App CRC + length + magic, see below |
| Settings page | 0x0801F000 | 2 KiB | 62 | Unchanged |
| Boot metadata page | 0x0801F800 | 2 KiB | 63 | Slot flag + boot records, see below |

The CI fit check is: 2 + 6 + 12 + app + 2 + 2 ≤ 128 KiB with app ≥ 100 KiB
(no unallocated reserve — the regions above total exactly 128 KiB). The
application image must end below the descriptor: 104 KiB minus the 64-byte
descriptor region, which the linker reserves (`0x0801EFC0..0x0801EFFF`).

### App descriptor (replaces metadata-held CRC)

The application's CRC and length live at a **fixed address inside the
application partition** (`0x0801EFC0`): magic word, layout version, image
length, CRC-32 of `0x08005000..0x08005000+length`. The updater programs the
app pages first and the descriptor **last** as the commit step. A power cut
before the descriptor is programmed leaves it blank → app verifies invalid →
updater mode (recoverable, same risk model as v1). The core verifying the app
never needs to erase or program the metadata page to record a new CRC, which
is what makes the "metadata is never erased during normal operation" policy
actually hold. The linker script must reserve the descriptor so the image
can never overlap it.

CRC-32 is pinned to the v1 algorithm: poly `0x04C11DB7`, init `0xFFFFFFFF`,
no reflection, no final XOR (the bitwise implementation at `main.c:183-196`);
the host-side tools already share this with the v1 metadata CRC.

### Boot metadata page layout (0x0801F800)

| Offset | Content |
|---|---|
| 0x00 | `layout_version` — guards against images built for a different map |
| 0x04 | Slot flag — erased/unknown → golden (safe default); valid slot-B mark → slot B |
| 0x08.. | Boot-record area — one word per boot attempt, see below |

Because the app CRC no longer lives here, erasing this page never invalidates
a working application: erased metadata simply means "boot golden," and golden
verifies the app from its descriptor. That is what makes every metadata
operation crash-safe by default.

**Boot records.** On each boot the core claims the first erased word in the
record area and programs an "attempted" mark. Once the main loop has proven
healthy — the same signal the settings compaction path already waits on — the
application marks that record healthy (its address is passed to the app in a
fixed SRAM word agreed in the contract). The core counts consecutive
unhealthy records; N failures → the core stays in updater mode instead of
looping. One or two record words are consumed per boot (depending on whether
the hardware permits clearing additional bits in the same word; see Open
items), so the 2 KiB page lasts on the order of a few hundred boots. When the
record area is exhausted the core performs a **metadata rebuild**: erase the
page, reprogram `layout_version` and the slot flag. A power cut mid-rebuild
leaves erased metadata → golden → normal app boot via the descriptor, so the
rebuild is safe at every instant. Erase wear is bounded by the rebuild cycle
(~1 erase per few hundred boots), an order of magnitude below the app's
settings-journal churn.

`JUMP:BOOTLOADER` (amended during implementation): the v1 erase-the-metadata
semantics no longer force updater mode in v2 — the app CRC lives in the
in-partition descriptor, so erased metadata plus a valid app simply boots
the app. Instead the application **programs the updater-request word**
(metadata word 2, `REQUEST_MARK`) and reboots. Programming is a single-word,
crash-safe operation (a torn write still reads non-erased → still requests
updater, the safe direction), strictly better than an erase. The selected
core sees the request and stays in its updater; the next successful app
upload's END commit rebuilds the metadata page (erase; reprogram layout
version and the slot flag), which clears the request and resets the boot
records. Boot records occupy words 3+ as (attempt, health) pairs, strictly
pair-strided.

## Trampoline page (frozen)

Page 0 contains, at fixed addresses:

- Initial SP and reset vector (the only entries hardware fetches).
- A ~50-instruction trampoline (`< 256` bytes, deliberately versionless):
  1. Sample the interlock GPIO (encoder) twice with a short delay; held
     down → jump to the golden core's entry (interlock override).
  2. Read the slot flag; select slot B only on a valid mark, else golden.
  3. Validate the selected slot's vectors at its fixed base: initial SP
     within `0x20000000..0x20004000` and reset vector inside that slot's
     bounds (the same checks the stock bootloader applies to the app,
     `main.c:257`). Invalid → try the other slot.
  4. Both slots invalid → jump to the legacy application entry `0x08008000`
     if its vectors pass the same sanity checks, else halt in a tight loop.

Step 4 is what makes the migration self-healing (the migrator is still
resident at the legacy entry) and costs nothing in normal operation — after
migration the golden core is never erased or rewritten, so "both slots
invalid" cannot occur. The page is written exactly once per device (during
v1→v2 migration) and never erased or programmed again; any change to the
trampoline itself requires SWD. `layout_version` in the metadata page guards
against images built for a different map.

## Boot flow and fail-safe rules

1. Hardware fetches SP/reset from the trampoline.
2. Trampoline: interlock override → flag → slot validity → legacy fallback.
3. The selected core copies its own vector table (192 bytes) to
   `0x20000000`, remaps SRAM to address zero via SYSCFG `MEM_MODE`, sets MSP
   from its own vector, and runs. It then verifies itself (built-in CRC over
   its slot against a constant in its last page) and verifies the
   application: descriptor magic/length/CRC and entry vector inside the
   application partition. Invalid → the core stays resident and presents the
   USB updater.
4. Boot records as described above: N unhealthy boots → updater mode.

**Vector-copy chain.** The Rust application links RAM from `0x200000c0`
because it expects its vectors already copied by the bootloader (contract
lines 12–13). In v2 each core performs its own copy at entry, and the working
core performs the application's copy — 192 bytes to `0x20000000`, remap, MSP,
jump — exactly what the stock bootloader does today (`main.c:163-175`). The
cores must leave `0x20000000..0x200000BF` clean for the app's vectors.

**Watchdog.** The cores never enable IWDG; the v1 boot path already runs
watchdog-free after a system reset (the stock bootloader performs unbounded
USB waits). The application remains solely responsible for its own IWDG, and
its reset ends it. Core flash sequences (a 52-page app erase ≈ 2 s) therefore
never race a watchdog they do not own.

## Update operations

The v2 protocol is sectioned; the wire framing stays compatible with the
existing ACK/DATA/CRC uploader (START/DATA/END commands, `main.c:40-44`) so
the Python test suite and GUI logic port rather than rewrite. START gains a
section selector (application / slot B); erase and write validation rejects
any range outside the selected section.

- **Application update (the default path):** the core erases/programs only
  application pages, descriptor last. Identical risk model to today — a
  failed app update leaves a recoverable device in updater mode. No interlock
  required.
- **Core slot B update:** stream the new core into slot B (the running core
  never touches its own pages), verify-after-page, CRC the slot, then flip
  the slot flag — a single word *program* into the metadata page, which is
  never erased during normal operation. Requires the physical interlock.
  Power cut at any point → previous boot path intact → retry. There is no
  unrecoverable window.
- **Golden core (slot A):** never updated by design. It enumerates USB and
  can rewrite slot B, the application region (descriptor included), and the
  slot flag. It may erase the metadata page (rebuild; also the only way to
  consume a sticky state). It refuses the settings page unless a destructive
  command explicitly names it.
- **Settings page:** unchanged ownership — written only by the application,
  under the existing journal discipline. Neither core touches it.

## Physical interlock

Writing any boot-core page or rebuilding metadata outside the boot-record
append path is authorized only when the device enters updater mode with the
encoder held down at plug-in. Routine application updates never enter this
state, so no host bug or rogue script can reach the dangerous paths during
normal use. The trampoline's interlock override (jump straight to golden)
means the gesture is also the guaranteed escape hatch when slot B is
corrupt — it is sensed before the flag is consulted.

## Failure matrix

| Interruption point | Resulting state | Recovery |
|---|---|---|
| App update, any point | Old app valid, or descriptor/CRC invalid | Updater mode; re-flash app over USB |
| VBUS glitch during app update | Same as above | Same — the source is the "unplugged cable" |
| Core B update, before flag flip | Flag still points at golden | Retry |
| Flag flip (torn word) | Trampoline sees unknown flag | Boots golden; re-do core update |
| Core B corrupt after flip (bad image, defect) | B fails self-check or N unhealthy boots → B updater | Interlock at plug-in → trampoline overrides flag → golden rewrites B |
| Metadata record area exhausted | Core rebuilds metadata | Erase-default = golden; app still valid via descriptor |
| JUMP:BOOTLOADER | App erases metadata page, reboots | Golden updater |
| Migration cut before page-0 write | Stock bootloader intact | Re-run migration flow |
| Migration cut during page-0 write | No bootable code | SWD only (tens of ms, once per device) |
| Migration cut after page-0 write | Trampoline live, slots incomplete | Trampoline → slots invalid → legacy entry → migrator resumes |

## Migration from v1 (no SWD required)

The stock bootloader only erases/programs the application region above
0x08008000 (`Flash.c:79,112`), so page 0 is unreachable through it.
Migration therefore uses a one-time migrator image. **The slot pages lie
inside the stock bootloader's reservation and are not empty** — the stock
image (HAL + USB stack) spans the first pages of the reservation — so the
order below is chosen so that the stock bootloader is destroyed only at the
same instant the trampoline replaces it:

1. GUI/CLI (legacy mode, unchanged wire protocol): flash `migrator.bin` —
   the v2 core machinery packaged as an ordinary application image at
   0x08008000. The stock bootloader flashes and runs it like any app.
2. Migrator (flash-wait loop in RAM, per `main.rs:74-88`):
   1. Erase the boot metadata page.
   2. Erase, program, and verify page 0 (the trampoline) **from
      RAM-executing code**. This is the one unrecoverable window: a power
      cut inside this erase→program→verify sequence (tens of milliseconds,
      once per device lifetime) leaves no bootable code and requires SWD.
   3. Erase and program slot A (golden), verify.
   4. Erase and program slot B, verify.
   5. Reset.
3. From the moment step 2 completes, every power cut is recoverable: the
   trampoline finds the slots invalid and jumps to the legacy entry
   (0x08008000), where the migrator is still resident, and resumes at the
   interrupted step. Before step 2, the stock bootloader is untouched and
   the flow simply re-runs.
4. The v2 trampoline boots golden, sees the erased metadata, and — with a
   valid app still absent (the old v1 app region is now the app partition's
   middle, with no descriptor) — sits in USB updater mode. The GUI detects
   the v2 USB identity and flashes the real v2 application.

The migrator's core-swap machinery is the same code the golden core uses for
slot-B updates — built once, exercised by migration, reused forever.

Documentation and the GUI prompt must state "keep the device plugged in;
prefer a mains-powered charger, not a power bank" for the migration step.

## Host tooling changes (all inside this fork)

- **USB identity scheme.** Today the v1 app and v1 bootloader share
  VID 1155 / PID 22336 and the GUI distinguishes modes by serial probing
  (`GUI/BenchVolt-PD.py:242`), not identity. v2 assigns a new PID to the
  boot-core updater (both cores enumerate with it; they speak the same
  sectioned protocol). The v2 application keeps the existing PID so serial
  tooling is unchanged. Mode detection order for the GUI: v2-core PID →
  sectioned protocol; legacy PID + probe → legacy protocol. The GUI refuses
  v2 images over the legacy protocol and vice versa — this, not identity
  alone, is the migration stage-confusion guard.
- `tools/flash_firmware.py` and the GUI update tab gain dual mode (legacy
  stock-bootloader protocol; v2 sectioned protocol).
- GUI gates core-section operations behind the interlock flow, shows the
  "do not unplug" state during the migrator's page-0 write, and runs a
  **PD preflight** before any update: refuse to start if the PD contract was
  renegotiated within the last few seconds or is below the nominal level —
  the observed renegotiation-on-reboot behavior makes a fresh contract the
  likeliest VBUS-glitch window.
- Release artifacts during transition: `migrator.bin` (one-time),
  `benchvolt-v2.bin` (sectioned), and `benchvolt-pd.bin` for
  stock-bootloader devices until the fleet is migrated, then deprecated.
- CI replaces the bootloader/app partition checks with a sectioned fit check
  (2 + 6 + 12 + app + 2 + 2 = 128 KiB, app ≥ 100 KiB, descriptor reserved in
  the linker script); `build_image.sh` emits the sectioned image with
  per-section CRCs using the pinned CRC-32.
- Documentation: README flashing runbook (including the charger-vs-power-bank
  note) and SCPI/boot contract notes.

## Testing

- Host-side protocol simulation: the Python uploader tests (already run in
  `check.sh`) drive a simulated sectioned-update state machine end to end,
  including descriptor-last commit ordering.
- Rust host tests for the boot-decision logic (flag semantics, record
  accounting, metadata rebuild, descriptor validation, layout-version
  checks) via the existing `--no-default-features` harness. The trampoline's
  decision table (interlock → flag → validity → legacy fallback) is a pure
  function with its own host test.
- A fast-forward host simulation of ~1,000 boots to exercise the record
  area and at least one metadata rebuild.
- Hardware checklist: brown-out / VBUS-dip injection at each failure-matrix
  row on a bench with SWD attached, before any no-SWD release — including
  a cut during every migration step (only the page-0 write may be fatal)
  and a re-programming probe to settle the boot-record encoding (Open
  items).

## Rollout

- M0: this plan + contract amendment merged.
- M1: trampoline, golden core (raw-register USB, see Open items), working
  core, sectioned protocol; CI fit checks on the 2 KiB page map.
- M2: migrator + dual-mode GUI/CLI + release pipeline.
- M3: release with transitional artifacts; migrate own units (SWD or GUI).
- M4: deprecate the v1 flashing path once migrated.

## Contract amendment (replaces line 16 paragraph)

> The stock C bootloader is superseded by the v2 boot architecture described
> in `Docs/v2-bootloader-design.md`: a frozen 2 KiB trampoline page, a
> never-updated golden recovery core at `0x08000800`, and an updatable
> working core at `0x08002000`, with the application partition extended to
> `0x08005000..0x0801EFFF` (104 KiB) and the application CRC and length held
> in an in-partition descriptor at `0x0801EFC0` rather than in the boot
> metadata page. The upload protocol remains USB CDC, keeps the ACK/DATA/CRC
> framing, and is extended with sectioned addressing. Erase/write validation
> must reject any range outside the image's declared sections; settings-page
> access from boot code is prohibited; boot-core writes and metadata rebuilds
> outside the boot-record append path require the physical interlock. The
> settings page and boot-metadata page locations, the vector-copy-to-SRAM
> boot mechanism (performed by each core for itself and by the working core
> for the application), and the application's right to erase the boot
> metadata page on bootloader entry are retained unchanged.

## Bring-up findings (2026-08-30, golden-fakeapp on hardware)

- The raw-register USB stack enumerates on macOS after fixing: XOR-based
  EPnR arming (STAT/DTOG are toggle-on-write-1), DADDR.EF restore after bus
  reset, a reset-end EPnR-wipe guard, exact-length string descriptors, EP0
  tx-done ordering, VID 0x0483 (the stock "1155" is decimal), CFGR3.USBSW
  (F070 has no HSI48 — reset state leaves USB unclocked on cold boot), and
  clearing FLASH_CR.PER/PG after each flash op (LOCK preserves them).
- The handover disconnect is 2 s (not 500 ms): re-attaching while the host
  is still tearing down the stock bootloader's device gets counted as an
  enumeration failure against the port, and macOS starts abandoning
  attaches after SET_ADDRESS.
- M2 verified end-to-end on hardware, USB only (no SWD): factory state
  (stock bootloader + v1 firmware flashed over the legacy protocol) →
  JUMP:BOOTLOADER → `migrator.bin` via the legacy protocol → migrator
  installs trampoline + both cores and resets → golden updater (`BV2C`) →
  v2 application (built with the `v2-boot` feature, 13.4 KiB free vs 1.5)
  over the sectioned protocol → CMD_BOOT → app runs, marks its boot record
  healthy, and its JUMP:BOOTLOADER (request word) + re-upload + relaunch
  round-trip works. The GUI's smart-update flow implements this sequence
  with image-vs-device layout matching as the stage-confusion guard.
- M1 verified end-to-end on hardware (stock bootloader replaced on the bench
  unit; full-flash backup in `boot-v2/backups/`): trampoline flag dispatch to
  golden and slot B; app upload over the sectioned protocol (20/20 soak);
  descriptor-last commit; metadata rebuild preserving the slot flag;
  updater-request word honored and cleared by the next upload; launch with
  pair-aligned boot-attempt records; unhealthy-streak (3) holding in
  updater; self-heal after an app-region crash. Not yet hardware-tested:
  interlock gesture (needs a physical encoder press), legacy fallback,
  slot-B upload over USB, migrator, VBUS-glitch injection.
- USB PMA count race (silicon behavior): the engine raises EPnR CTR_RX
  marginally before COUNT_RX is visible in the PMA. A polled loop reads
  faster than any ISR and can fetch the PREVIOUS packet's count — visible
  only on size-changing (final) chunks. Mitigation in `boot-usb`: consume
  the event, spin ~1 µs, read the count until two reads agree, and keep RX
  NAKed until the command is fully processed (single-owner RX with
  `rx_release()` after dispatch — also proper flow control during
  multi-millisecond flash writes).
- A running image must never accept an upload that erases its own section:
  the app refuses all flash commands (INFO tag "BV2A" vs the cores' "BV2C")
  and real app updates always go through the updater-request reboot.
- Host-side hazards that mimic firmware bugs, for the GUI docs/runbook:
  macOS blocks identification of new USB accessories while the screen is
  locked ("blocked by transport restrictions" — device is addressed, then
  silence); and a browser holding a remembered WebUSB/Web-Serial permission
  for the VID/PID auto-opens the device exclusively on attach, which
  suppresses the CDC interfaces (no tty ever appears).

## Open items

- Verify on hardware whether re-programming an already-programmed word to
  clear additional bits is reliable on F070 (no ECC). If yes, the health mark
  shares the attempt word (one word per boot); if no, attempt and health are
  two adjacent single-programmed words (two words per boot). The record-area
  sizing and rebuild cadence follow from this.
- Final golden-core size with raw-register USB (target ≤ 6 KiB; the fit
  check enforces it). Raw-register USB from the start is the working
  assumption — a `usb-device`-based core is realistically 8–15 KiB and does
  not fit a frozen slot. Panic = abort, no formatting, no allocator in slot A.
- Exact PID value for the v2 boot-core updater identity.
- Encoder-hold-at-plug-in vs long-press as the interlock gesture
  (hold-at-plug-in is the working assumption: the trampoline can sense it
  deterministically; a long-press cannot be, since the trampoline exits
  within microseconds).
- GUI copy for the migration warning and the "do not unplug" state.
