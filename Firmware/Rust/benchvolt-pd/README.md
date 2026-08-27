# BenchVolt PD

This is the Rust hardware proof of concept. It starts with every power output
disabled, displays the five-channel application overview with live ADC
measurements and TMP1075 temperature, and exposes a USB CDC diagnostic and
output-control interface.

Output requests go through the same typed application action whether they come
from the encoder or USB. The reducer changes requested state only. A transition
observer derives a power effect from `(old_state, new_state)`, the power service
executes dependency-aware hardware operations, and a typed completion or fault
action updates physical state. Reducers and views never access GPIO, ADC, I2C,
converter registers, or delays. A failed global shutdown latches a hardware
fault on every channel but leaves `physical_enabled` untouched, so flash
compaction and boot-time PD settling keep waiting for a verified off state; the
firmware additionally escalates to a raw register-level emergency shutdown.

The driver fails closed on I2C NACK, register mismatch, converter fault status,
or GPIO readback mismatch (EN pins are verified through IDR, the electrical
pin level, not the output latch). CH1/CH2 require DC1; CH3/CH4 require DC2; CH4 programs
its inverted MCP4725 setpoint before raising its gate; CH5 always performs a
full hardware-EN cold start before programming and enabling its TPS55289.
Protection samples voltage/current every 20 ms and temperature every 100 ms.
While a waveform runs, every periodic software service — the ADC sweep,
TPS/TMP1075 health reads, the PD contract watchdog, and display measurement
sync — is suspended so the loop stays dedicated to the 2 kHz sampler: any
multi-hundred-microsecond pass shows up as visible timing jitter on the
output, and a 20 ms point sample of a deliberately moving current is poor
protection anyway (it false-trips on waveform peaks and misses events between
samples). During a run the converters' cycle-by-cycle hardware OCP/OVP/SCP
protect the output; all software services re-arm the moment the run stops.
CH5 waveform samples also use an unverified single-write register update —
the verified write-and-read-back used for setpoint changes takes longer than
the 500 us sample period, and a corrupted sample self-corrects on the next
one.
Newly enabled outputs get a bounded 200 ms startup qualification interval while
their hardware current limiting remains active. After that, three consecutive
20 ms current or voltage-window violations are required to latch a fault;
invalid measurements still fail immediately. Latched TPS STATUS-register
faults use a two-read confirmation instead, because STATUS is
read-to-clear. Live CH4/CH5 voltage edits update
the requested setpoint immediately but slew the physical drive in bounded
200 mV control steps. Each verified drive step starts a bounded 500 ms
voltage-settling interval. Current protection and hardware-I/O failure handling
remain active throughout that interval.
USB remains interrupt-owned during all foreground hardware and display work.
Visible voltage/current values are ten-sample averages published every 200 ms;
this display filtering does not slow or filter the protection path. Encoder
clock edges are captured by a bounded, lowest-priority EXTI queue so TFT work
cannot lose a quick rotary spin; queued detents are coalesced into one state
transition and one selective repaint.

USB transport is owned by the STM32 USB interrupt. The ISR performs only USB
polling and moves bytes through bounded command and response queues. Command
parsing, application state access, ADC/I2C work, and display drawing remain in
the main loop. Queue overload fails boundedly with `ERR:BUSY`; if even the error reply
cannot be queued, one `ERR:OVERFLOW` line is emitted as soon as the
response queue drains so the host never waits on a silently dropped
reply. Display or sensor latency cannot prevent USB reset, enumeration,
or endpoint servicing.

The companion C bootloader and this crate share a hardened partition contract.
This application links at `0x08008000` and is limited to the 92 KiB application
partition ending before `0x0801f000`; upload erase/write validation cannot enter
the settings page or final boot-metadata page.

CH1–CH5 current limits, CH4/CH5 voltage setpoints, the CH4/CH5 CV/CC modes,
USB-PD input protection limit, and temperature unit are persisted as versioned,
CRC-checked append-only records in the
reserved `0x0801f000..0x0801f7ff` settings page after an edit has remained
quiet. Torn or corrupt records are ignored, and persisted AWG configuration is
range-validated before the reducer may index channels with it. Runtime saves only program a blank
slot. If the journal fills, page erase/compaction is deferred until every power
output is physically off and, at boot, until the loop has proven healthy —
never inside the attach window, where a source hard reset could interrupt
the page erase and blank the only settings page. A record-program failure
skips the dirty slot rather than wedging persistence for the session. Three explicit profile slots store validated snapshots
of those settings. Loading a profile or Factory Defaults first performs a global
hardware shutdown. Output states, faults, UI location, and active operation are
never persisted. As a fail-safe, Factory Defaults also restores the STUSB4500's
NVM to its canonical configuration (20 V sink profile, request the source's
full advertised current, USB communication capable), recovering a unit whose
PD voltage was profiled to something unusual; the NVM change takes effect at
the next cold attach, so replug the power cable afterwards.

The main menu contains DC Power, AWG, Settings, PD Source, System, and Help.

The PD Source screen lists the attached source's advertised fixed PDOs
(filtered like the GUI's PDO list and capped at the 20 V board input
ceiling), marks the live contract's row ACTIVE, and lets
a row be armed by click and applied from the front panel. The capability read
transmits Get_Source_Cap, which restarts negotiation — and some sources
answer that with a VBUS hard reset that cold-boots this VBUS-powered board —
so it runs at most once per boot, only on user entry (a banner boot after a
VBUS-reset apply is strictly display-only), only with a live settled
contract and every output inactive, and never repeats on contract events
(that feedback loop was observed on hardware). The cache cannot go stale: a
source swap always cold-boots the board. Apply (and USB `SOUR:PDO:SET`)
additionally requires the live contract, because the apply programs
STUSB4500 NVM and must not race a renegotiation's hard reset. Apply obeys the same admission rule
as `SOUR:PDO:SET`: every output must be inactive (a hint banner explains a
dimmed Apply or a stalled list). Applying first appends a settings-journal
record carrying the requested voltage, then reprofiles the STUSB4500 NVM PDO2
and triggers renegotiation. Because the MCU is VBUS-powered, an apply that
hard-resets VBUS cold-boots the device: the journaled flag routes that boot
directly back to the PD Source screen with a requested-vs-actual banner. That
boot path is display-only — it never re-attempts the apply or writes the
STUSB — and the flag is cleared after a single boot, so a source that refuses
or drops the request converges to a normal boot instead of a boot loop.
Apply persists the choice as the STUSB4500's cold-attach boot preference (the
same NVM PDO2 mechanism as `SOUR:PDO:SET`); Factory Defaults restores the
canonical 20 V profile and `SYST:PD:NEGOTIATE` remains the USB escape hatch. AWG supports
CH4/CH5 selection, square/triangle/ramp/sine waveforms, frequency, square-wave
duty cycle from 1% to 99%, low/high voltage, and Start/Stop. Duty is unavailable
and electrically inert for non-square waveforms. A field click enters or leaves
edit mode. Starting AWG
first globally shuts down every DC output, then enables only the selected
channel at the low voltage. Leaving AWG, stopping it, a protection trip, or an
I2C failure shuts the waveform output down. Scheduling uses absolute monotonic
deadlines and a phase accumulator; late service emits one phase-correct sample
instead of stale catch-up writes. The local generator runs from a dedicated
2 kHz scheduler; square waves reach
125 Hz, and triangle/ramp/interpolated-sine waves reach 120 Hz.

The desktop-compatible arbitrary-waveform interface accepts contiguous chunks
of up to eight `(centivolts,dwell)` pairs with
`SOUR:WAVE:CHn:ARB:DATA start,...`, followed by
`SOUR:WAVE:CHn:ARB:START count,multiplier,repetitions`. Integer multipliers
retain the original millisecond meaning; multiplier `0.5` selects one 2 kHz
scheduler tick per dwell unit. Uploads are limited to 1024 validated points and
the physical range of the selected channel. `DATA` ACKs mean the chunk was
stored; the final `START` ACK is sent only after global shutdown and verified
enable of the selected output. Repetition zero runs continuously. Finite
completion, `OUTP:CHn OFF`, a UI conflict, or any protection/driver fault safely
disables the waveform output. Remote ARB metadata is operational state and does
not overwrite the persisted on-device AWG configuration. The Rust extension
also provides `SOUR:WAVE:CHn:ARB:STOP` and `...:ARB:STAT?`; status reports the
current point, completed cycles, late updates, and cycles skipped to preserve
absolute time.

The built-in waveform engine is also remote-controllable:
`SOUR:WAVE:CHn:FUNC <SQU|TRI|RAMP|SIN>,<freq_millihz>,<duty_pct>,<low_mv>,<high_mv>`
configures it (validated by the same reducer rules the front panel uses, and
rejected with `ERR:BUSY` while any waveform is active), `SOUR:WAVE:CHn:RUN`
starts it (the `OK` ack is deferred until the output is verified running, like
ARB `START`), `SOUR:WAVE:CHn:STOP` performs a confirmed global shutdown, and
`SOUR:WAVE:CHn:STAT?` reports the engine status for the owning channel.

The application never touches the bootloader's CRC seal at startup, so an
interrupted boot cannot strand the device in the bootloader. `JUMP:BOOTLOADER`
erases the seal deliberately for a firmware upload, and SWD remains the
recovery path for an application build that crashes before USB comes up.

USB commands (full syntax, reply formats, and scripting examples are in the
[SCPI interface reference](../../../Docs/scpi-interface.md)):

- `*IDN?`
- `SYST:BUILD?`
- `SOUR:WAVE:CH4:ARB:DATA ...` / `SOUR:WAVE:CH5:ARB:DATA ...`
- `SOUR:WAVE:CH4:ARB:START ...` / `SOUR:WAVE:CH5:ARB:START ...`
- `SOUR:WAVE:CH4:ARB:STOP` / `SOUR:WAVE:CH5:ARB:STOP`
- `SOUR:WAVE:CH4:ARB:STAT?` / `SOUR:WAVE:CH5:ARB:STAT?`
- `SOUR:WAVE:CH4:FUNC ...` / `SOUR:WAVE:CH5:FUNC ...` (built-in engine config:
  `<SQU|TRI|RAMP|SIN>,<freq_millihz>,<duty_pct>,<low_mv>,<high_mv>`)
- `SOUR:WAVE:CH4:RUN` / `SOUR:WAVE:CH5:RUN`
- `SOUR:WAVE:CH4:STOP` / `SOUR:WAVE:CH5:STOP`
- `SOUR:WAVE:CH4:STAT?` / `SOUR:WAVE:CH5:STAT?`
- `SOUR:WAVE:FUNC?` (the on-device engine configuration:
  `CH<n>,<waveform>,<freq_millihz>,<duty_pct>,<low_mv>,<high_mv>`)
- `SYST:PD:CONTRACT?` (negotiated PD contract: `position,millivolts,milliamps`,
  or `NONE`)
- `SYST:TICK?` (free-running hardware milliseconds, for timing diagnostics)
- `MEAS:TEMP?`
- `MEAS:CH1?` through `MEAS:CH5?`
- `MEAS:SINK?`
- `SYST:PROT:CH1?` through `SYST:PROT:CH5?` (raw protection
  sample, peak current, grace/counter state, and the last latched trip sample)
- `SYST:TPS:CH5?` (last raw TPS55289 STATUS byte; `0x80` SCP, `0x40`
  OCP, `0x20` OVP, and `0xFF` means the health read failed; STATUS is
  read-to-clear and a shutdown requires the same fault class to reassert on
  the following 20 ms poll)
- `SINK:LIMIT?`
- `SINK:LIMIT 4.250`
- `SOUR:CURR:CH1?` through `SOUR:CURR:CH5?`
- `SOUR:CURR:CH1 0.400` through CH5
- `SOUR:MODE:CH4?` / `SOUR:MODE:CH5?`
- `SOUR:MODE:CH4 CV` / `SOUR:MODE:CH4 CC` (and CH5)
- `OUTP:CH1 ON` / `OUTP:CH1 OFF` through CH5
- `JUMP:BOOTLOADER`

An output command returns `OK` only after the matching hardware completion has
been reduced. It can instead return `ERR:OVERCURRENT`, `ERR:OVERTEMP`,
`ERR:SENSOR`, or `ERR:HARDWARE`. Entering the bootloader first attempts every
independent output-off control and both shared-rail disables.

On a channel detail screen, a short encoder press cycles control focus. CH1–CH3
cycle Output, current limit, and none; CH4 and CH5 cycle Output, voltage
compliance, CV/CC mode, current limit, and none. Their lower control row places
the mode toggle between the voltage and current settings. Rotate either
direction while Output is focused to toggle it. Rotate while a
setting is focused to adjust it in 10 mV or 10 mA steps. Settings are locked
only during an output transition. Live CH4/CH5 voltage changes pass through
verified hardware side effects; software current thresholds change immediately.
With no focus, rotation navigates
screens; holding for 500 ms navigates back one level immediately without
waiting for release (channel detail and USB-PD input screens return to the
DC overview; everything else returns to the main menu). Encoder acceleration tracks successive detents over real
elapsed time and ramps through 2x/4x/8x/16x, while an isolated detent retains
10 mV/10 mA precision.

The overview appends a cyan `CC` marker to the STATE cell for CH4 or CH5 when
that channel is configured for constant-current regulation. The marker and the
detail-screen `CC` label turn green when the physical drive is below the
voltage-compliance setting, indicating that the current-control loop is
actively governing the output. Holding the
encoder button for three seconds requests a safe reboot; the firmware first
shuts down every output and shared rail. The 500 ms main-menu action does
not stop the hold timer from reaching that reboot threshold.

On Overview, short clicks focus the compact output switches in row order from
CH1 through CH5. Rotating either direction toggles the focused output through
the same typed power effect used by the detail screen. One more click after
CH5 clears focus, returning rotation to screen navigation. ON, OFF, transition,
and fault states use distinct green, neutral, amber, and red switch tracks.

CH4 and CH5 CC are digital control loops: each independent ADC current
measurement is reduced into a bounded voltage-drive change, and only the
resulting transition may emit an MCP4725 or TPS55289 voltage side effect. The
displayed voltage setting remains the compliance ceiling. The CH5 TPS55289
current-limit register stays at a fixed
3 A configuration ceiling because connected-board tests showed that its
IOUT_LIMIT loop does not control CH5 load current. Independent software
runaway protection remains
active independently of the CC loop. A constant-power/negative-impedance load
cannot be regulated below its required compliance voltage by conventional CC;
use a resistor or an electronic load in constant-resistance mode for CC tests.

The final screen is `USB PD Input`. It shows the measured sink voltage,
current, and power in the same tabular format as the overview. A short press
focuses the sink current protection limit; rotation adjusts it from 0 to 5 A in
10 mA steps. The firmware boots to the main menu; this diagnostics screen is
last in the DC-screen rotation. Passive discovery errors such as `DETACHED` or
`BUS` remain visible there, while an imported contract displays its PDO number,
voltage, and current.
Startup first imports the STUSB4500's autonomous contract without transmitting.
The boot contract requires no firmware involvement: the STUSB4500 autonomously
negotiates its NVM PDO2 voltage preference at every cold attach, before the
application runs. `SOUR:PDO:SET 3 <mv> <ma>` (the GUI's PDO selector) persists
the chosen voltage into NVM PDO2 first, then applies a volatile RAM override
and re-advertises source capabilities to renegotiate live. A downward voltage
transition can reboot this VBUS-powered board; because the preference is
already in NVM, the re-attach lands directly on the chosen voltage. Writing
the RAM override also raises `DPM_PDO_NUMB` to cover slot 3 — the NVM loads a
count of 2, which silently disabled the archived C firmware's slot-3 override.
After three seconds of healthy execution with every output physically off, a
one-time check programs the STUSB4500 NVM `USB_COMM_CAPABLE` flag if it is
clear, so PD requests declare USB data support and macOS keeps the port's data
connection alive; the flag takes effect at the next cold attach.

Read-only status polling imports the resulting contract. Import requires sink-ready
state, a valid non-mismatch RDO, a valid input ADC sample, and measured VBUS to
match one of the controller's enabled fixed sink PDO voltages. It never sends a
PD message. From a terminal, `SYST:PD:NEGOTIATE` (or the legacy
`SOUR:PD:CONF:MAX`) first enables the STUSB4500 NVM `REQ_SRC_CURRENT` mode. That mode
makes the controller autonomously request all current advertised by the matched
source PDO on subsequent cold attachments. The update reads sector 4, changes
only that mode bit, erases and programs only sector 4, then requires an exact
eight-byte readback before returning success. If an interrupted earlier update
left the sector erased, it restores the checked-in legacy NVM sector image. An
already-configured controller is not rewritten; the command instead replays the
same volatile legacy RAM-PDO/Soft-Reset request used at boot. After
`OK:PD:NVM_UPDATED:POWER_CYCLE`, remove all VBUS sources before cold-starting
from the intended PD source. A valid imported contract is required before any
output can enable.

During operation, three consecutive valid ADC samples above the lower of the
configured limit and negotiated operating current latch an input overcurrent
fault and run the global hardware shutdown. Detach, contract downgrade, or PD
communication failure also shuts down globally. A missing sink-current sample
fails closed. The latch clears only after every output is off and ten
consecutive samples are valid and at or below the limit; outputs do not
automatically restart. `SYST:PD?` distinguishes idle, active negotiation,
verified contract, and typed terminal error status over CDC. `SYST:PD:RAW?` is
a read-only hardware diagnostic: it reports the STUSB4500 device ID, attach,
VBUS-monitor, CC state/fault, Type-C FSM, reset, VBUS gate, policy-engine,
configured-PDO-count, and active-RDO registers.
Its `SNK0x` field contains the twelve live PDO register bytes in ascending
register order.
It deliberately avoids read-clear alert registers and never transmits a PD
message.

## Invariants and the contribution pattern

These are the rules every change must preserve. They are enforced three ways:
reducer guards reject violating actions, the fuzz harness asserts them after
every dispatch (`tests/common/mod.rs::assert_invariants` and
`assert_bounded_slew`), and the coverage match in `tests/fuzz.rs`
(`action_fuzz_coverage`) refuses to compile when an `Action` variant is
neither fuzzed nor explicitly excluded with a reason.

1. **Requested vs. physical.** `requested_enabled` records intent;
   `physical_enabled` changes only in completion arms (`OutputApplied`,
   `OutputFailed`, `GlobalShutdownApplied`). A failed shutdown must never
   clear `physical_enabled`.
2. **Token discipline.** Every enable/disable bumps the channel `operation`
   counter; completion arms act only when the token matches the exact pending
   transition. Never add a completion-shaped arm that skips the token compare.
3. **Bounded slew.** While a channel is physically enabled, `drive_mv` moves
   only through `RegulateChannel`'s 200 mV steps (or the AWG sampler for the
   channel it owns). Setpoint edits store intent; they do not touch a live
   drive.
4. **Effects come from the planner.** Reducer arms change state only. If a
   change needs hardware work, express it as state the
   `FirmwareEffectPlanner` diff will notice - never call drivers from a
   reducer or view.
5. **Fail closed.** Any path that cannot verify hardware reached a safe state
   escalates: verified `execute_global_shutdown`, then
   `raw_emergency_shutdown`, then reset with a recorded reason.

Adding an `Action` variant, step by step: write the reducer arm with its
guards; the build then fails at `action_fuzz_coverage` until you either add
the variant to `random_event` in `tests/fuzz.rs` (preferred) or exclude it
with a written reason; if the variant touches `drive_mv`, tokens, or
enable state, extend the invariant assertions rather than carving out an
exemption; finish with `tools/check.sh`.

## Headless build and flash runbook

Always run the complete gate immediately before a device upload and flash the
binary produced by that run. Never reuse an existing `target/.../benchvolt-pd.bin`
as a recovery shortcut; it may not correspond to the newest source. Record the
uploader's image size and CRC with the connected test result.

Both the checked-in uploader and the desktop GUI's Firmware Update page speak
the same hardened protocol (verified against the bootloader source and on
hardware): 60-byte payloads (one 64-byte CDC packet including the header),
an ACK validated after every stage, and the bootloader-compatible STM32 CRC.
Both endpoints enforce the 92 KiB partition, and neither writes the
bootloader, option bytes, or protection settings. The checked-in uploader
remains the canonical scripted path because it adds host-side image
validation (vector table, stack pointer, partition fit) and, via
`flash_latest.sh`, the outputs-off admission check before the bootloader
jump.

`tools/check.sh` is the single canonical test/build/image command. Do not run a
bare `cargo test` here: `.cargo/config.toml` intentionally defaults Cargo to the
Thumb target, while the test harness must use the detected host target. Before
hardware work, run from this directory:

```sh
tools/check.sh
```

It runs the host reducer and service tests, Rust lints, uploader tests, the
Thumb release build, exact binary partition validation, and host-Clang syntax
checks for both C firmware projects. It does not connect to or flash a device.
The gate rejects an otherwise valid release image if less than 1 KiB remains in
the application partition, preserving room for safe maintenance changes.
Whole-tree formatting is not part of this gate yet because committed legacy
modules still carry pre-existing rustfmt differences.

During hardware iteration, `tools/build_image.sh` is the canonical fast path.
It performs the Thumb release build, always regenerates the raw `.bin` from the
new ELF, and validates the partition bounds. Do not upload a `.bin` after a bare
`cargo build`: Cargo does not refresh that derived file.

For a connected upload, use `tools/flash_latest.sh`. It runs the complete gate,
builds a fresh binary, validates its partition bounds, then invokes the checked-
in uploader. It also reuses an existing pyserial installation instead of
reinstalling it. List serial ports or flash with:

```sh
tools/flash_latest.sh --list
tools/flash_latest.sh /dev/cu.usbmodemBOOTLOADER_PORT
tools/flash_latest.sh --from-app /dev/cu.usbmodemRUST_APPLICATION_PORT
```

The `--from-app` form queries `MEAS:ALL?` and refuses to reset unless all five
physical outputs and both arbitrary-waveform channels are off. It then uses the
firmware's fail-closed `JUMP:BOOTLOADER` path and discovers the re-enumerated
stock STM32 port before uploading.

Only if none of the recognized Python environments has pyserial, create the
project-local environment once:

```sh
python3 -m venv .venv
.venv/bin/pip install pyserial
```

If the Rust application is running, prefer the `--from-app` form above so the
output-state check, safe transition, port discovery, and upload remain one
repeatable operation.

On any missing ACK, NACK, or CRC mismatch, the uploader stops immediately and
does not send `CMD_END` again. The device remains in the stock bootloader. The
application never modifies the seal words at `0x0801f800`, so power
interruptions at any point of a boot always relaunch the application.
`JUMP:BOOTLOADER` deliberately erases the metadata page before resetting; an
application build that crashes before USB comes up needs SWD to recover.

## Live power acceptance

Use current-limited input power and test one channel at a time before testing
shared dependencies. For each channel: read the disabled baseline, request ON,
require `OK`, verify measured voltage/current, request OFF, require `OK`, and
allow output capacitance to discharge before proceeding. Then test CH1+CH2 and
CH3+CH4 pairs; disabling one sibling must leave the other regulated. End by
turning every output off and querying all five measurements.
