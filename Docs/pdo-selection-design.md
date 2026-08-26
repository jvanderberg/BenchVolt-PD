# On-device PDO selection — design

Status: implemented (see `Firmware/Rust/benchvolt-pd`). Notes on deviations:
the "re-read on contract-change events" data flow below proved wrong on
hardware — the capability read transmits Get_Source_Cap, which itself
restarts negotiation and raises contract events, so re-reading on them is a
self-sustaining read/renegotiate loop (and the resulting full-screen repaint
storm overflowed the 192-command paint queue into a latched display failure).
The list is read once per screen entry, only while outputs are inactive;
the journal records the requested millivolts only (no index — the banner
names the voltage), encoded in previously-unused record bytes so no record
version bump was needed (old and new records decode in both directions); and
the row markers are colored ARMED/ACTIVE suffixes. The flash budget was met
by outlining the view layer's text/fill primitives (~2.1 KiB reclaimed
across every screen).

## Goal

Let the user pick which of the attached source's fixed PDOs the device
requests, from the front panel, without a computer. Applying a PDO frequently
causes the source to hard-reset VBUS; because the MCU is VBUS-powered with no
battery, that is a **cold power cycle** of the device mid-interaction. The UX
must treat "the screen goes black and the device reboots" as an ordinary,
expected outcome of Apply.

## UX

A dedicated screen, entered from a new main-menu row (working name:
**PD Source**). Not on the USB-PD Input screen (a mid-apply reboot on a shared
status screen is disorienting) and not in Settings (the option list is a
property of the attached charger, not of the device).

Screen layout, top to bottom:

- One row per source-advertised **fixed** PDO: `9.0V  3.0A  27W`.
- The row matching the live contract (`pd_contract.source_position`) carries a
  persistent **active** marker, visually distinct from the navigation cursor.
- `Apply` and `Cancel` rows at the bottom.
- One-boot banner line for post-reboot results (see below).

Interaction: turn moves the cursor; click on a PDO row arms it as the pending
choice (armed row gets a third visual state); click `Apply` executes; click
`Cancel` or long-press (the existing back gesture) discards and leaves.

`Apply` is disabled (dimmed) unless `outputs_inactive()` — the same admission
rule the USB `SOUR:PDO:SET` path already enforces. A hint row ("outputs must
be off") beats a silent dead control.

## Data flow

- **On screen entry**: live `read_source_capabilities` over the PD soft-I2C
  bus, filtered through `decode_fixed_pdo` (this is the `PdList` path, which
  already drops malformed/augmented leading objects some chargers advertise).
  Cache the list in screen state; re-read on contract-change events, not per
  frame. An I2C failure renders an error row and keeps `Apply` disabled.
- **Apply** (outputs verified off):
  1. Append a settings-journal record with a new `pdo_apply_pending` field =
     requested PDO (index + millivolts, so the post-reboot banner can name it
     even if the source's list changed).
  2. `set_sink_pdo` — reprofile the STUSB4500.
  3. Trigger renegotiation.
  4. Two outcomes:
     - **In-place renegotiation**: a `PdNegotiated`/`PdFailed` event arrives;
       the screen updates the active marker; clear the pending flag (next
       journal write).
     - **VBUS hard reset → cold boot**: see boot routing.

## Boot routing

At boot, if the latest journal record carries `pdo_apply_pending`:

- Route the UI directly to the PD Source screen instead of the main menu.
- Show a requested-vs-actual banner once the STUSB reports the negotiated
  contract (it negotiates autonomously from NVM at attach; firmware only
  observes). "Requested 12.0V — active 12.0V" or "Requested 12.0V — source
  gave 20.0V".
- Clear the flag after one boot, unconditionally.

### Boot-loop guard (critical)

This repo's history includes a charger-provoked infinite boot loop from PD
writes at startup. The boot path for `pdo_apply_pending` must be
**display-only**: it never re-attempts the apply, never writes the STUSB, and
the flag clears after a single boot regardless of outcome. A pathological
charger therefore converges to a normal boot showing "didn't stick" rather
than looping. Do not "helpfully" retry.

Clearing the flag requires a journal append at boot; that write must obey the
existing rule (outputs physically off — true at boot) and must tolerate a
full journal (compaction path already exists). If the flag cannot be cleared
(flash error), still boot normally; a sticky banner is annoying but safe.

## Reducer / actions

New state: `Screen::PdSource`, cursor/armed-index/banner in `AppState` (armed
selection is UI state, NOT a persistent preference), plus the cached PDO list
(bounded: the STUSB exposes at most 8 source PDOs; a `heapless::Vec<PdoRow, 8>`
or fixed array + count).

New actions (illustrative): `PdSourceListLoaded([...])`, `ArmPdo(u8)`,
`ApplyArmedPdo`, `PdoApplyRecorded`, `PdoApplyResult { requested, actual }`.
Follow the invariants section of the firmware README:

- Reducer arms change state only; the I2C reads/writes are driven from
  main.rs off state transitions (or explicit main-loop handling like the
  existing `UsbIntent::PdoSet` arm — acceptable since PD I/O is already
  main-loop-owned rather than planner-owned).
- The fuzz coverage match (`action_fuzz_coverage` in `tests/fuzz.rs`) will
  fail the build until the new actions are enrolled or excluded with reasons.
  Enroll them: random `ArmPdo`/`ApplyArmedPdo` under fuzz must uphold
  "no apply while outputs live" — add that as a harness invariant.

## Safety notes

1. **Outputs-off is load-bearing, twice.** It gates Apply (a contract change
   collapses the input rail under any enabled output) and it is the
   precondition for the journal write. Keep one check, early.
2. **The planner already treats contract changes as global-shutdown
   triggers** (`contract_lost`, `contract_limit_changed`). Verify the new
   flow composes rather than double-triggering.
3. **Recovery paths must survive a bad selection**: factory defaults already
   restore canonical STUSB NVM, and `SYST:PD:NEGOTIATE` exists over USB.
   Mention both in the user manual section for this feature.
4. **Do not block the loop during capability reads.** `read_source_
   capabilities` is a multi-ms soft-I2C pass; it must not run while the AWG
   is hot (reuse the `awg_hot` suspension pattern) and should not run per
   frame.

## Flash budget — likely the hard part

Free flash at time of writing: **~2.2 KiB**. A new screen (list rendering,
strings, boot routing, journal field, new reducer arms) plausibly costs
1.5–3 KiB. Assume a size-golf pass is part of this feature. Candidates, in
order: audit `core::fmt` monomorphizations (formatting machinery is the
classic thumbv6 offender), shorten/merge help text, share row-rendering code
with the existing menu screens instead of writing a new list renderer.
CI enforces the partition limit, so an overrun fails loudly, not subtly.

## Journal format note

`pdo_apply_pending` extends `PersistentSettings`/record layout. The decoder
ignores torn/corrupt records but records are versioned — bump the record
version and keep the old-version decode path so a downgrade/upgrade across
this feature doesn't discard the user's persisted limits. Add a
round-trip + old-version-decode test alongside the existing settings tests.

## Testing plan

- Reducer/screen: unit tests for cursor wrap, arm/disarm, apply-guard
  (outputs live ⇒ no state change), banner lifecycle.
- Journal: round-trip with the new field; old-record compatibility; full
  journal + compaction with the flag set.
- Integration (harness): full flow — arm, apply, simulate the in-place
  renegotiation outcome; and the cold-boot path by constructing a fresh
  harness from a store whose latest record has the flag set, asserting it
  boots to the screen, shows the banner, clears the flag, and **performs no
  PD writes** (extend the mock/PD-bus recording to assert absence).
- Fuzz: enroll the new actions; add "no apply while outputs live" to
  `assert_invariants`.
- Bench (manual): apply against at least one charger that hard-resets and one
  that renegotiates in place; confirm the banner is truthful in both, and
  that a charger that refuses the request converges to a normal boot with the
  "didn't stick" banner (no loop).

## Open questions for the implementer

1. Does `set_sink_pdo` + renegotiate reliably take effect in-place on any
   bench source, or is the cold-attach path effectively always taken? (If
   always cold, the in-place UI path still must exist but can be simpler.)
2. Should the armed choice also become the persistent boot preference (NVM
   PDO2), or apply-once? Current lean: it already is the NVM preference by
   mechanism (`set_sink_pdo` writes NVM), so document that Apply == new boot
   default, and rely on factory defaults as the escape hatch.
3. Menu row name: "PD Source" vs "Input PDO" vs "USB-PD". Pick whatever fits
   the 5→6 menu-row layout without re-spacing.
