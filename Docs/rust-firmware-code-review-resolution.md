# Rust firmware code-review resolution

This ledger reconciles the Claude review dated 2026-08-23 (performed against
`explore/benchvolt-firmware` at `63ce8e7`) with the test-first hardening work on
`benchvolt/review-hardening`. Line numbers in the original review are stale
because `main.rs` has been reduced from 2,663 to 1,012 lines.

## Resolved safety findings

- IWDG supervision is active before boot metadata access and is fed only at
  bounded boot checkpoints and after a complete foreground pass.
- Panic and HardFault paths synchronously force every GPIO-controlled output
  and shared rail off before reset. GPIO clocks, low latches, and output modes
  are established before the first fallible watchdog operation (`3c97fa4`).
- Flash busy waits are bounded and execute from RAM. Settings and boot-seal
  mutations are deferred until outputs are physically off.
- Power settling is deadline-driven rather than a blocking 50 ms delay, so
  input, PD, and protection services continue to run.
- Protection timing preserves scheduler phase and fails closed after repeated
  late measurement windows (`6e434c0`).
- STUSB4500 support models and supervises the active PD contract. Startup
  discovery is read-only; after three healthy seconds with outputs physically
  off, a sub-20 V contract receives the exact RAM-PDO plus PD Soft Reset
  sequence recovered from the archived original `main.7z`. The boot seal stays
  invalid through the bounded transition, preserving bootloader recovery if
  the source drops VBUS. Detach, RDO
  identity change, invalid contract voltage, or missing
  contract while outputs are active causes global shutdown.
- The STUSB4500 software-I2C clock now uses a tested 2 us half-cycle. This
  keeps its requested clock within the controller's 400 kHz Fast-mode limit
  instead of relying on incidental GPIO overhead to slow a nominal 500 kHz
  request. Connected testing showed that Get Source Capabilities consistently
  dropped VBUS before the RAM PDO could be changed. Replaying the archived
  sequence in the correct order—write all RAM PDOs first, then send PD Soft
  Reset—negotiated 20.03 V without resetting the MCU. The explicit negotiation
  command also programs the STUSB4500 `REQ_SRC_CURRENT` NVM bit once and verifies
  the complete eight-byte sector.
  The software-I2C transport now supports the NVM sector's eight-byte write;
  its former four-byte limit was sufficient for PDOs but rejected NVM writes.
- Passive RDO import rejects capability mismatch, impossible current fields,
  and fixed-supply current above 5 A (`8026e56`). Operating current above the
  matched sink PDO is accepted only when it equals the RDO maximum, which is
  the STUSB4500 `REQ_SRC_CURRENT` representation of the matched source's full
  current. Input protection uses the lower of the user limit and negotiated
  operating current (`066a3be`, `b8e4325`).
- TPS55289 conversions match the reference C equations; invalid register
  readback fails closed; the two shared rails use the reference hardware's 6 A
  limit; CH5 status read failure is an immediate hardware fault.
- The 32 V claim in the review is not valid for the r3 measurement divider.
  With 6.8 kΩ / 1 kΩ scaling, 32 V would present about 4.10 V to a 3.3 V MCU
  ADC. The enforced 22 V maximum presents about 2.82 V and is regression-tested.

## Resolved architecture and test findings

- Hardware effects execute after reduction through Reducto's typed transition
  effect path; hardware submission no longer occurs inside dispatch.
- Reducer guard paths share invariant cleanup, and boot-seal restoration is
  dispatched back into visible application state.
- USB compatibility parsing/projection, voltage mutation, output completion,
  PD completion, arbitrary-waveform coordination, persistence policy, service
  cadence, protection policy, and view damage decisions have host tests.
- The legacy GUI protocol surface needed by existing clients is supported,
  including `MEAS:ALL?`, voltage queries, legacy output mutation forms, and
  remote voltage setting.
- View transitions repaint only damaged regions. Framed numeric controls paint
  only their value interior on numeric changes and repaint their frame only on
  focus changes (`14a92b2`).
- Duplicate segment truth tables and menu/help contracts now have one tested
  owner (`b5fa7cb`, `38c742f`).
- ARB runtime storage and hardware diagnostics no longer live as loose globals
  in `main` (`ab98839`, `c5f2bb5`).
- `tools/check.sh` is the headless regression gate: host tests/lints, uploader
  tests, Thumb release lint/build/image validation, and fatal diagnostics for
  both C firmware projects.

## Reducto resolution

Reducto 0.1.0 was cleaned up, licensed MIT, tested, documented, tagged, pushed
to the verified `jvanderberg` remote, and published to crates.io. BenchVolt uses
the typed effect API rather than treating an observer callback as an effect
executor.

## Remaining work and hardware evidence required

- Connected-device testing now has a read-only `SYST:PD:RAW?` snapshot including
  all twelve live sink-PDO bytes. With the PPS source attached, autonomous NVM
  startup initially produced 5 V / 3 A. Three attempts to request Source
  Capabilities each reset VBUS before a RAM PDO could be installed. The archived
  original `main.7z` revealed the missing order: it writes RAM PDOs first and
  then sends PD Soft Reset. Replaying that exact sequence produced source PDO5,
  a 20 V / 5 A contract, and 20.03 V measured VBUS without a reset. A fresh
  application boot subsequently reached the same contract automatically without
  any CDC command. All outputs remained off throughout. The r3 schematic still
  shows that both receptacles share the upstream `VBUS` net; simultaneous powered
  COM and PD-source connections are therefore not electrically isolated. The
  separate receptacle-A CC/data-routing and enumeration limitation remains
  documented in `Docs/r3-usb-c-routing-erratum.md`.

- Passive startup can recover RDO current exactly, but the RDO contains no
  nominal voltage. If the transient Source Capabilities message was missed,
  voltage is inferred by matching independently measured VBUS to an enabled
  local fixed sink profile. This is deliberately fail-closed but remains an
  inference rather than exact source metadata.
- Connected-hardware coverage is still required for detach under load,
  watchdog reset with energized outputs, and injected I2C
  faults. The headless gate does not claim to replace those tests.
- `main.rs` is substantially smaller but still owns board construction and the
  hardware side of USB intents. Further extraction should keep reset/flash/GPIO
  mutations explicit and must be accepted only with no behavioral regression
  and an acceptable flash-size result.
