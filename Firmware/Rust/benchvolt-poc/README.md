# BenchVolt Rust POC

This is the Rust hardware proof of concept. It starts with every power output
disabled, displays the five-channel application overview with live ADC
measurements and TMP1075 temperature, and exposes a USB CDC diagnostic and
output-control interface.

Output requests go through the same typed application action whether they come
from the encoder or USB. The reducer changes requested state only. A transition
observer derives a power effect from `(old_state, new_state)`, the power service
executes dependency-aware hardware operations, and a typed completion or fault
action updates physical state. Reducers and views never access GPIO, ADC, I2C,
converter registers, or delays.

The driver fails closed on I2C NACK, register mismatch, converter fault status,
or GPIO latch mismatch. CH1/CH2 require DC1; CH3/CH4 require DC2; CH4 programs
its inverted MCP4725 setpoint before raising its gate; CH5 always performs a
full hardware-EN cold start before programming and enabling its TPS55289.
Protection samples voltage/current every 20 ms and temperature every 100 ms.
Newly enabled outputs get a bounded 200 ms startup qualification interval while
their hardware current limiting remains active. After that, three consecutive
20 ms current or voltage-window violations are required to latch a fault;
invalid measurements still fail immediately. Live CH4/CH5 voltage edits update
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
the main loop. Queue overload fails boundedly with `ERR:BUSY`; display or sensor
latency cannot prevent USB reset, enumeration, or endpoint servicing.

The companion C bootloader and this crate share a hardened partition contract.
This application links at `0x08008000` and is limited to the 92 KiB application
partition ending before `0x0801f000`; upload erase/write validation cannot enter
the settings page or final boot-metadata page.

CH1–CH5 current limits, CH4/CH5 voltage setpoints, the CH4/CH5 CV/CC modes,
USB-PD input protection limit, and temperature unit are persisted as versioned,
CRC-checked append-only records in the
reserved `0x0801f000..0x0801f7ff` settings page after an edit has remained
quiet. Torn or corrupt records are ignored. Runtime saves only program a blank
slot. If the journal fills, page erase/compaction is deferred until every power
output is physically off. Three explicit profile slots store validated snapshots
of those settings. Loading a profile or Factory Defaults first performs a global
hardware shutdown. Output states, faults, UI location, and active operation are
never persisted.

The main menu contains DC Power, AWG, Settings, System, and Help. AWG supports
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

For recovery testing, the POC erases the bootloader CRC metadata page once at
startup. It continues running normally. On the next reset, the unchanged
bootloader sees no valid application seal and remains available for upload.

USB commands:

- `*IDN?`
- `SYST:BUILD?`
- `SOUR:WAVE:CH4:ARB:DATA ...` / `SOUR:WAVE:CH5:ARB:DATA ...`
- `SOUR:WAVE:CH4:ARB:START ...` / `SOUR:WAVE:CH5:ARB:START ...`
- `SOUR:WAVE:CH4:ARB:STOP` / `SOUR:WAVE:CH5:ARB:STOP`
- `SOUR:WAVE:CH4:ARB:STAT?` / `SOUR:WAVE:CH5:ARB:STAT?`
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
screens; holding for 500 ms returns to the main menu immediately without
waiting for release. Encoder acceleration tracks successive detents over real
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
10 mA steps. Startup is deliberately passive: the firmware never transmits a
PD request or automatically retries one during boot, because some VBUS-powered
sources hard-reset the supply in response and can create a reboot loop. After
the recommended 500 ms attach interval, read-only status polling can import a
contract negotiated autonomously by the STUSB4500. Import requires sink-ready
state, a valid non-mismatch RDO, a valid input ADC sample, and measured VBUS to
match one of the controller's enabled fixed sink PDO voltages. It never sends a
PD message. From a terminal, `SYST:PD:NEGOTIATE` (or the legacy
`SOUR:PD:CONF:MAX`) explicitly starts one bounded active attempt. The firmware
selects the highest-power fixed PDO at or below 20 V, caps requested current to
the configured sink limit, writes the STUSB4500's third sink PDO in RAM,
requests renegotiation, and verifies the resulting RDO. The command receives
`OK` only after verification, or a typed `ERR:PD:*` terminal cause. Failed
active attempts never retry without another explicit command. A valid contract
is required before any output can enable; after changing the limit, explicitly
negotiate again while outputs are off.

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
It deliberately avoids read-clear alert registers and never transmits a PD
message.

## Headless build and flash runbook

Before connecting both USB receptacles on an r3 board, read the repository's
[r3 USB-C routing erratum](../../../Docs/r3-usb-c-routing-erratum.md). Both
receptacles share VBUS without source isolation. An ordinary powered USB-A COM
cable must not be used at the same time as a separate PD source. The schematic-
supported one-cable arrangement uses the PD/COMM receptacle, S2 in the USB-A
data position, and a USB-C host or dock that provides both data and power. That
mode has not yet enumerated successfully on the tested board.

Use the checked-in uploader rather than importing the desktop GUI. It uses the
hardened protocol's 60-byte payloads (one 64-byte CDC packet including the
header), validates every ACK, and computes the bootloader-compatible STM32
CRC. Both endpoints enforce the 92 KiB partition. It never writes the
bootloader, option bytes, or protection settings.

Before hardware work, run the repository-native regression gate from this
directory:

```sh
tools/check.sh
```

It runs the host reducer and service tests, Rust lints, uploader tests, the
Thumb release build, exact binary partition validation, and host-Clang syntax
checks for both C firmware projects. It does not connect to or flash a device.
Whole-tree formatting is not part of this gate yet because committed legacy
modules still carry pre-existing rustfmt differences.

One-time Python setup:

```sh
python3 -m venv /tmp/benchvolt-flash
/tmp/benchvolt-flash/bin/pip install pyserial
```

Build against the published MIT-licensed Reducto crate and produce the binary:

```sh
cargo build --release
arm-none-eabi-objcopy -O binary \
  target/thumbv6m-none-eabi/release/benchvolt-poc \
  target/thumbv6m-none-eabi/release/benchvolt-poc.bin
```

Run the reducer/power-service safety tests on the host before building for the
MCU. They inject a failure at every driver boundary and run 10,000 randomized
request/failure transitions:

```sh
host_triple=$(rustc -vV | sed -n 's/^host: //p')
cargo test --lib --target "$host_triple"
```

If the Rust application is running, send `JUMP:BOOTLOADER\n` over its CDC port.
Then identify the re-enumerated stock `STM32 Virtual ComPort` and flash:

```sh
/tmp/benchvolt-flash/bin/python tools/flash_poc.py \
  /dev/cu.usbmodemBOOTLOADER_PORT \
  target/thumbv6m-none-eabi/release/benchvolt-poc.bin
```

On any missing ACK, NACK, or CRC mismatch, the uploader stops immediately and
does not send `CMD_END` again. The device remains in the stock bootloader. The
application invalidates only the metadata page at `0x0801f800` before all
peripheral initialization. It restores the prior seal only after three seconds
of healthy execution, so an early crash returns to the bootloader while a
normal power cycle relaunches the application. `JUMP:BOOTLOADER` deliberately
leaves the metadata erased.

## Live power acceptance

Use current-limited input power and test one channel at a time before testing
shared dependencies. For each channel: read the disabled baseline, request ON,
require `OK`, verify measured voltage/current, request OFF, require `OK`, and
allow output capacitance to discharge before proceeding. Then test CH1+CH2 and
CH3+CH4 pairs; disabling one sibling must leave the other regulated. End by
turning every output off and querying all five measurements.
