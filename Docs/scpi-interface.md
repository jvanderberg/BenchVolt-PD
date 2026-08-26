# BenchVolt-PD SCPI Interface Reference

The Rust firmware exposes a SCPI-style, line-oriented ASCII command interface
over the USB-C connector (USB CDC-ACM virtual serial port). This is the
interface the desktop GUI uses; anything the GUI does can be scripted directly.

## Transport and conventions

- **Port**: USB CDC-ACM (`/dev/tty.usbmodem*` on macOS, `/dev/ttyACM*` on
  Linux, `COMx` on Windows). The baud rate setting is ignored by the device;
  115200 is conventional.
- **Framing**: commands are ASCII lines terminated by `\n` (a trailing `\r` is
  tolerated). Replies are terminated by `\r\n`.
- **Channels**: `CH1`–`CH3` are the fixed 1.8 V / 2.5 V / 3.3 V outputs.
  `CH4` is the 0.5–5 V adjustable output, `CH5` the 0.8–22 V adjustable
  output. Waveform commands exist only for `CH4` and `CH5`.
- **Deferred acknowledgement**: commands that change a physical output
  (`OUTP:CHn`, waveform `RUN`, ARB `START`) reply only after the hardware
  transition has actually completed and been verified — an `OK` means the
  output really changed, and an error means it really didn't.
- **Unknown commands** reply `ERR:UNKNOWN_COMMAND`.

### Error replies

| Reply | Meaning |
|---|---|
| `ERR:UNKNOWN_COMMAND` | Command not recognized |
| `ERR:SYNTAX` | Recognized command, malformed arguments |
| `ERR:RANGE` | Argument outside the accepted range |
| `ERR:BUSY` | Operation conflicts with an active run or pending command |
| `ERR:SENSOR` | Measurement invalid (ADC failure) |
| `ERR:OVERCURRENT` / `ERR:OVERTEMP` / `ERR:HARDWARE` | Output transition failed and latched the named fault |
| `ERR:INCOMPLETE` / `ERR:SEQUENCE` | ARB upload not contiguous / complete |
| `ERR:PD:...` | USB-PD operation failed (`BUS`, `DEVICE`, `DETACHED`, `TIMEOUT`, `CAPS`, `NO_PDO`, `CONTRACT`, `NVM`) |

## Identity and system

| Command | Reply | Notes |
|---|---|---|
| `*IDN?` | `BenchVolt-PD,RUST,S/N:2026-01` | |
| `SYST:BUILD?` | `BenchVolt-PD v<version> <git-rev> ...` | Firmware version and build info |
| `SYST:TICK?` | `1234` | Free-running hardware milliseconds, for timing diagnostics |
| `SYST:LOOP?` | `3` | Worst gap between main-loop passes since the last command, in 0.5 ms AWG scheduler ticks. The waveform-health canary: 2-3 is a dedicated loop; sustained large values mean something is starving the 2 kHz sampler. Any command resets it. |
| `SYST:REBOOT` | — | Safe reboot: shuts down all outputs and shared rails first |
| `JUMP:BOOTLOADER` | — | Shuts down all outputs, erases the boot seal, and enters the C bootloader for a firmware update |

## Measurements

| Command | Reply example | Notes |
|---|---|---|
| `MEAS:CH1?` … `MEAS:CH5?` | `3.300V,1.250A` | Live voltage and current; `ERR:SENSOR` if the sample is invalid |
| `MEAS:VOLT:CH1?` … `CH5?` | `3.30` | Bare voltage in volts (`nan` if invalid) |
| `MEAS:SINK?` | `20.000V,2.500A,50.000W` | USB-PD input (sink) measurement |
| `MEAS:TEMP?` | `34.50` | Board temperature in °C (may be negative); `ERR:SENSOR` if invalid |
| `MEAS:ALL?` | 27 comma-separated fields | See below |

`MEAS:ALL?` returns, in order:

1. – 10. `V1,I1,V2,I2,V3,I3,V4,I4,V5,I5` — per-channel voltage/current
2. 11.–12. `Vsink,Isink` — USB-PD input
3. 13\. temperature (°C)
4. 14.–18. output enable flags for CH1–CH5 (`0`/`1`, physical state)
5. 19.–20. ARB-active flags for CH4, CH5 (`0`/`1`)
6. 21.–25. current-limit settings for CH1–CH5 (amps)
7. 26.–27. voltage setpoints for CH4, CH5 (volts)

## Output control

| Command | Notes |
|---|---|
| `OUTP:CHn ON` / `OUTP:CHn OFF` | Also accepted: `OUTP:CHn:STAT 1` / `0` (and the space form `OUTP:CHn STAT 1`). Replies `OK` only after the output transition completes; otherwise a fault error. |
| `OUTP:CHn?` | `ON`, `OFF`, or `FAULT:<CAUSE>` (e.g. `FAULT:OVERCURRENT`) |

## Setpoints, limits, and regulation mode

| Command | Notes |
|---|---|
| `SOUR:VOLT:CH4 5.00` / `SOUR:VOLT:CH5 12.50` | Voltage setpoint in volts. CH4 accepts 0.50–5.00, CH5 0.80–22.00. Live changes slew in verified 200 mV steps. |
| `SOUR:CURR:CHn 0.400` | Per-channel current limit in amps |
| `SOUR:CURR:CHn?` | e.g. `2.050A` |
| `SOUR:MODE:CH4 CV` / `CC` (and CH5) | Regulation mode for the adjustable channels |
| `SOUR:MODE:CH4?` / `CH5?` | `CV` or `CC` |
| `SINK:LIMIT 4.250` | USB-PD input current limit in amps |
| `SINK:LIMIT?` | e.g. `4.250A` |

## Protection and hardware diagnostics

| Command | Notes |
|---|---|
| `SYST:PROT:CH1?` … `CH5?` | Raw protection state: last sample, peak current, grace/strike counters, and the last latched trip sample |
| `SYST:TPS:CH5?` | Last raw TPS55289 STATUS byte: `0x80` SCP, `0x40` OCP, `0x20` OVP; `0xFF` means the health read failed |

## USB-PD input

| Command | Notes |
|---|---|
| `SOUR:PD:LIST?` | Streams the source's fixed PDOs between `UI_PDO_LIST_START` and `UI_PDO_LIST_END` marker lines. Each row is `index,millivolts,milliamps,milliwatts`. |
| `SYST:PD:CONTRACT?` | The negotiated contract as `position,millivolts,milliamps`, or `NONE`. The GUI uses this to auto-select the active PDO. |
| `SOUR:PDO:SET <slot> <millivolts> <milliamps>` | Writes a sink PDO slot (1–3; slot 3 is the active request). Limits: 20000 mV, 5000 mA. |
| `SYST:PD:NEGOTIATE` (alias `SOUR:PD:CONF:MAX`) | Re-runs PD negotiation |
| `SYST:PD:RAW?` | Raw STUSB4500 diagnostic dump (register/status fields) |

## Built-in waveform engine (CH4/CH5)

These commands drive the same phase-accurate generator the front panel uses: a
dedicated 2 kHz scheduler producing square, triangle, ramp, and interpolated
sine. Limits match the on-device UI: **0.1–125 Hz for square, 0.1–120 Hz for
the other shapes** (~17 setpoints per cycle at the ceiling), duty cycle 1–99 %
(square only), and the physical voltage range of the selected channel.

| Command | Notes |
|---|---|
| `SOUR:WAVE:CHn:FUNC <SQU\|TRI\|RAMP\|SIN>,<freq_millihz>,<duty_pct>,<low_mv>,<high_mv>` | Configure. Validated by the same reducer rules as the front panel; replies `OK`, `ERR:RANGE`, or `ERR:BUSY` if any waveform is active. |
| `SOUR:WAVE:CHn:RUN` | Start. The channel must match the configured channel. The `OK:WAVE_STARTED` ack arrives only after a confirmed global shutdown, verified enable of the output, and the engine actually running; a failed start replies `ERR:HARDWARE`. |
| `SOUR:WAVE:CHn:STOP` | Confirmed global shutdown of the run. Replies `OK` (also when nothing was running). |
| `SOUR:WAVE:CHn:STAT?` | `RUNNING`, `STARTING`, `STOPPING`, `FAULT`, or `STOPPED` — for the built-in engine on that channel only. |
| `SOUR:WAVE:FUNC?` | The on-device engine configuration: `CH<n>,<SQU\|TRI\|RAMP\|SIN>,<freq_millihz>,<duty_pct>,<low_mv>,<high_mv>`. The device configuration is the single source of truth; the GUI syncs its waveform panel from this on connect. |

Example — 60 Hz sine between 1 V and 5 V on CH4:

```
SOUR:WAVE:CH4:FUNC SIN,60000,50,1000,5000   →  OK
SOUR:WAVE:CH4:RUN                            →  OK:WAVE_STARTED
SOUR:WAVE:CH4:STAT?                          →  RUNNING
SOUR:WAVE:CH4:STOP                           →  OK
```

Behavioral notes:

- A remote start switches the device display to the AWG screen with the
  Start/Stop row highlighted, so the physical stop control is immediately
  usable at the bench.
- While a waveform runs, the firmware dedicates its loop to the 2 kHz
  sampler: display updates and all periodic software monitoring (including
  the software current-limit check, temperature, and the PD watchdog) are
  suspended for waveform purity. The converters' cycle-by-cycle hardware
  OCP/OVP/SCP protect the run; software protection re-arms when it stops.
  `MEAS:*` values for the driven channel are stale during a run.
- Only one waveform (built-in or ARB, either channel) can run at a time.
- Any protection trip, `OUTP:CHn OFF`, or front-panel navigation away from the
  AWG screen safely shuts the waveform down.
- The frequency argument is in **millihertz** (e.g. `60000` = 60 Hz,
  `100` = 0.1 Hz); voltages are in millivolts.

## Arbitrary waveforms (CH4/CH5)

Custom point lists upload in chunks and then start as a scheduled run. Uploads
are limited to 1024 points, validated against the physical range of the
selected channel (CH4: 0.50–5.00 V; CH5: 0.80–22.00 V).

| Command | Notes |
|---|---|
| `SOUR:WAVE:CHn:ARB:DATA <start>,<cv1>,<dwell1>,...` | Up to eight `(centivolts,dwell)` pairs per chunk; `start` is the index of the first point. A chunk starting at 0 begins a fresh upload. Each chunk is acknowledged `OK:ACK:CHn`. |
| `SOUR:WAVE:CHn:ARB:START <count>,<multiplier>,<repetitions>` | Starts the uploaded waveform. Integer multipliers are milliseconds per dwell unit (the original C-firmware meaning); multiplier `0.5` selects one 2 kHz scheduler tick (0.5 ms). Repetitions `0` runs continuously. The final ack `OK:CHn_ARB_STARTED_PTS:<count>` is deferred until the output is verified running. |
| `SOUR:WAVE:CHn:ARB:STOP` | Confirmed global shutdown of the run |
| `SOUR:WAVE:CHn:ARB:STAT?` | `<status>,INDEX:<n>,CYCLES:<n>,LATE:<n>,SKIP:<n>` — current point, completed cycles, late updates, and cycles skipped to preserve absolute time |

ARB timing is command rate, not guaranteed analog bandwidth: each point is one
DAC/converter update, and the usable rate depends on channel, voltage swing,
and load. A failed or incomplete upload replies `ERR:SEQUENCE`,
`ERR:INCOMPLETE`, or `ERR:RANGE` instead of starting.

## Scripting example (Python)

```python
import serial, time

dev = serial.Serial("/dev/tty.usbmodem1101", 115200, timeout=0.5)

def scpi(cmd, wait=0.1):
    dev.reset_input_buffer()
    dev.write((cmd + "\n").encode())
    time.sleep(wait)
    return dev.readline().decode().strip()

print(scpi("*IDN?"))                       # BenchVolt-PD,RUST,S/N:2026-01
print(scpi("SYST:PD:CONTRACT?"))           # 4,20000,5000

scpi("SOUR:CURR:CH4 1.500")                # 1.5 A current limit
print(scpi("SOUR:WAVE:CH4:FUNC SIN,60000,50,1000,5000"))  # OK

# RUN acks only after the output is verified running — allow a few seconds.
dev.timeout = 3.0
print(scpi("SOUR:WAVE:CH4:RUN", wait=0))   # OK:WAVE_STARTED

time.sleep(5)
print(scpi("SOUR:WAVE:CH4:STAT?"))         # RUNNING
print(scpi("SOUR:WAVE:CH4:STOP"))          # OK
```
