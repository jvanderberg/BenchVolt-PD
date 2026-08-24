# BenchVolt-PD Rust Port Hardware Contract

This document is the compatibility record for replacing the C application with Rust. The existing C application is the primary evidence for board wiring, calibration, converter configuration, and protection behavior. A behavior listed as a C defect is evidence about the hardware path, but is not behavior to reproduce.

No output may be enabled until the relevant setpoint, current limit, and converter configuration have been applied successfully.

## Platform and boot boundary

- The target is an STM32F070RB with 128 KiB flash and 16 KiB RAM.
- The existing bootloader occupies `0x08000000..0x08007fff`.
- The application starts at `0x08008000`.
- The bootloader copies the application vector table to SRAM at `0x20000000`, remaps SRAM to address zero, sets MSP from the application vector, and jumps to the reset vector.
- The existing C application reserves the first 192 bytes of SRAM for that copied vector table and links RAM from `0x200000c0`.
- The bootloader stores application CRC and byte length in the final 2 KiB flash page at `0x0801f800`.
- The bootloader currently accepts an application length up to 96 KiB. This nominal range overlaps its own final-page metadata and leaves no application-settings page. The Rust application will end before `0x0801f000`, limiting it to 92 KiB. Settings use `0x0801f000..0x0801f7ff`, and boot metadata remains at `0x0801f800..0x0801ffff`.
- Retain the custom USB CDC bootloader, application address, CRC algorithm, and desktop upload protocol. Before relying on it for Rust updates, harden its C implementation to reject images, erase ranges, chunks, and write addresses outside the 92 KiB application partition. This is a bounded bootloader correction, not a Rust bootloader rewrite.

## Output topology and control

The user-facing channel order is fixed:

1. CH1, 1.8 V, channel gate `EN3` on PC12
2. CH2, 2.5 V, channel gate `EN2` on PA15
3. CH3, 3.3 V, channel gate `EN1` on PB15
4. CH4, adjustable low output, channel gate `EN4` on PB6
5. CH5, adjustable high output, hardware enable `EN5` on PB7

Shared preregulators:

- DC1 is a TPS55289 at 7-bit I2C address `0x75` on the PC8/PC9 software I2C bus. Its hardware enable is `EN_DC1` on PC13. The C application programs it to 3.0 V and a 6.0 A hardware current limit. It supplies the low fixed-output group.
- DC2 is a TPS55289 at 7-bit I2C address `0x74` on the PC8/PC9 software I2C bus. Its hardware enable is `EN_DC2` on PB2. The C application programs it to 5.5 V and a 6.0 A hardware current limit. It supplies CH3 and CH4.
- CH5 has its own TPS55289 at 7-bit I2C address `0x75` on the PC6/PC7 software I2C bus. `EN5` is its hardware enable. Its output-enable bit is also controlled over I2C.
- CH4 uses an MCP4725 at 7-bit I2C address `0x60` on PC6/PC7 to control an analog voltage-margin path.
- Board temperature uses a TMP1075 at 7-bit I2C address `0x48` on PC8/PC9.
- The STUSB4500 USB-PD sink controller is on a separate software I2C bus on PA8/PA9.

Shared converters and individual channel gates are different controls. Disabling an individual channel must not accidentally remove a shared preregulator needed by another enabled channel. Disabling a shared preregulator must make all dependent channels physically off and update their logical state.

## Safe startup

The GPIO output latches must be driven low before their pins become outputs. This includes all five channel enables and both shared converter enables.

The required Rust startup sequence is:

1. Establish clocks, vector relocation compatibility, and a monotonic timer.
2. Drive all five channel enables and both shared converter enables low.
3. Initialize GPIO, ADC, display, and the three software I2C buses.
4. Calibrate the ADC. Calibration failure is fatal and outputs remain off.
5. Load and validate persisted settings. Invalid records use conservative defaults.
6. With outputs still physically off, initialize each TPS55289 for internal feedback.
7. Program each converter voltage and hardware current limit.
8. Program the CH4 DAC setpoint.
9. Verify I2C acknowledgements and, where readable, converter register values or status.
10. Leave all outputs disabled. Outputs are enabled only by an explicit DC or AWG user action.

PB8 and PB9 are global red and blue status LED outputs, not per-channel fault
inputs. The bootloader toggles both as its waiting heartbeat and jumps without
resetting GPIO configuration, so the application must claim both pins and drive
them low during safe startup. The original C application later sets PB8 when a
foreground iteration exceeds 4 ms; that diagnostic behavior is not evidence of
a converter fault and must never be wired into output protection semantics.

The C application does not follow the safe order above. It raises all seven hardware enables before initializing and programming the converters. That ordering is not a requirement to preserve.

## TPS55289 configuration and re-enable sequence

The converter driver uses internal feedback by clearing the external-feedback selection bit and setting the feedback range bits in `VOUT_FS` to `0b11`.

The voltage reference calculation used by all TPS55289 instances is based on:

- 45 mV reference offset
- 0.5645 mV reference LSB
- 0.0564 feedback ratio
- reference code clamped to `0x000..0x7fe`
- device voltage clamped to 0.8 V through 22.0 V

The TPS55289 hardware current-limit calculation uses a 10 milliohm sense resistor and 0.5 mV register LSB. A positive limit sets the register enable bit. A non-positive value writes zero and disables the converter's programmable current limit. This formula describes the IC and the original source; it does not prove that the CH5 board path has a usable differential output-current sense connection.

Connected-board testing is authoritative for CH5: programming codes corresponding to 0.10 A and 0.50 A did not limit load current, and both attempts rose above 2.3 A before independent ADC protection shut the channel down. Therefore CH5 CV/CC control must not use the TPS55289 `IOUT_LIMIT` register as its regulation actuator or claim that register as an effective hardware backstop. Keep it at a fixed 3.0 A configuration ceiling and implement CH5 CC as a digital loop whose input is the independent ADC measurement and whose output is a bounded TPS55289 voltage-setpoint side effect. The configured CH5 voltage is the CC compliance ceiling. The software runaway monitor remains independent and must trip if the load current diverges. This prohibition may be revisited only after the board's ISP/ISN path is traced and verified electrically.

CH4 CC uses the same application-level digital loop with CH4's independent ADC current measurement and the MCP4725 voltage-margin path as its actuator. Its configured voltage is also a compliance ceiling. Both CC loops remain pure reducer transitions followed by typed voltage side effects; neither reducer may access I2C or a driver directly.

Driving a TPS55289 hardware EN pin low resets its registers. Therefore CH5 recovery after overcurrent must not merely raise `EN5`. A CH5 enable attempt must perform a cold-enable transaction:

1. Keep output request logically off while reconfiguration occurs.
2. Raise `EN5` and allow the device's required startup delay.
3. Reapply internal-feedback configuration.
4. Reapply the persisted CH5 voltage and the CH5 hardware current limit.
5. Read the read-to-clear STATUS register twice, matching the original C
   initialization and discarding power-up/configuration history.
6. Set the converter output-enable bit.
7. Confirm output or status within a bounded timeout.
8. Mark the channel enabled only after successful completion.

If the fault is still present, the normal protection path disables the channel again. All TPS55289 I2C read-modify-write operations must fail closed. An I2C NACK or invalid `0xff` read must never result in enabling an unconfigured output.

## CH4 voltage calibration

CH4 has an inverted DAC-to-output relationship. The C calibration is a two-point linear mapping:

- 0.50 V output maps to DAC code 3975.
- 5.00 V output maps to DAC code 340.

The calculated DAC code is rounded and clamped to the 12-bit range `0..4095`. The user setting is clamped to 0.50 V through 5.00 V before this conversion. Runtime DAC writes update the volatile DAC register only. The MCP4725 EEPROM must not be written on each adjustment.

This calibration is a board-specific requirement until measurements establish a revised calibration.

## Measurements

- The ADC is calibrated at startup and operated at 12-bit resolution with a 3.3 V reference assumption.
- The C application configures ADC inputs 0 through 15 plus the internal temperature channel as a forward scan.
- User-facing measurements are mapped as follows:
  - CH1 current ADC1 IN3, voltage IN15
  - CH2 current IN2, voltage IN14
  - CH3 current IN1, voltage IN7
  - CH4 current IN4, voltage IN8 with divider factor 2.0
  - CH5 current IN5, voltage IN9 with divider factor 7.8
- Current channels use a conversion factor of 2.0 A per ADC-input volt, consistent with a 10 milliohm shunt and gain of 50.
- Power is calculated from the same measurement snapshot as voltage multiplied by current.
- The external TMP1075 value is a signed 12-bit quantity with 0.0625 degrees C per LSB.

Protection must consume a stable measurement pipeline with explicit sample timing. Display work, USB traffic, and encoder handling must not suspend protection sampling. ADC timeouts or stale samples must be represented as invalid data, not zero current.

The C implementation takes one scan sample and applies the current threshold immediately, with no persistence filter or hysteresis. This is evidence of the threshold path, not a sufficient noise policy for the Rust port. The Rust protection monitor gives a newly enabled output a bounded 200 ms startup qualification window while hardware current limiting remains active, then requires three consecutive 20 ms violations before latching overcurrent or output-voltage faults. A live CH4/CH5 voltage edit changes the requested setpoint immediately but slews the physical drive through bounded 200 mV reducer steps and typed voltage effects. After each verified adjustable-voltage side effect, voltage tracking alone receives a bounded 500 ms settling interval so output capacitance can follow the command. Overcurrent checking, invalid-sensor handling, and hardware-I/O failure shutdown remain active during that interval.

## Protection behavior

- The maximum user current limit is 3.00 A for each of the five outputs.
- A limit of zero means that any meaningful positive measured output current will trip the software protection. It must not silently disable protection.
- When measured current exceeds the channel's limit, immediately drive that channel's physical enable low and stop AWG if it owns that channel.
- Record a distinct fault state and cause. Enabled, requested-enabled, physically-enabled, and faulted are separate state values.
- Re-enabling a faulted channel is the user action that clears the latched software fault and makes one bounded restart attempt. If the overload remains, it trips again.
- A disabled indicator is not a fault indicator. The UI turns red only for a fault.
- Board overtemperature threshold in the C application is 75 degrees C from the TMP1075. Overtemperature disables all five channel enables and both shared converter enables.
- Overtemperature must stop AWG and invalidate converter initialization for every TPS55289 whose hardware EN was lowered.
- The overtemperature trip threshold is 75 degrees C. Recovery becomes eligible below 70 degrees C, providing 5 degrees C hysteresis. Automatic output restart after cooling is not allowed.
- An enable request while temperature remains at or above 70 degrees C, or temperature is invalid or stale, must fail closed.
- A temperature-sensor communication failure or stale reading forces all outputs off and records a temperature-sensor fault.

The C application has no proper fault object, no overcurrent debounce, and no automatic TPS55289 reinitialization after hardware EN is lowered. Those are defects, not compatibility requirements.

## DC enable and disable policy

- Power-on startup state is all outputs disabled.
- Entering DC Power from the main menu enables all five outputs using dependency-aware sequencing.
- Returning from DC Power to the main menu leaves the current DC output states unchanged. Keep this as an isolated navigation policy so it can be changed later without changing hardware sequencing.
- The All On action does not retry faulted channels. A faulted channel remains off until the user explicitly enables that channel.
- A dependent channel can be enabled only after its shared preregulator is configured, enabled, and allowed to settle.
- On an all-off request, disable individual channel gates first, then disable shared preregulators and CH5 converter output. Lower TPS55289 hardware EN only when the chosen power policy requires it. If hardware EN is lowered, mark that converter uninitialized.
- Any output transition must update logical state from the result of the hardware operation. The UI must not claim ON merely because ON was requested.

Exact inter-stage settle delays must be verified from component data sheets and on connected hardware before they are shortened from the conservative 50 ms delays used by the C application.

## AWG constraints

- AWG is available only on CH4 and CH5.
- On-device AWG controls are channel, waveform, frequency, duty, low voltage, high voltage, and Start/Stop. Duty is a persisted 1% through 99% HIGH-time setting for square waves only; it is unavailable and must have no scheduler effect for triangle, ramp, or sine.
- Built-in waveforms are square, triangle, ramp, and sine. Custom ARB remains available through the SCPI-like interface and is not edited on-device.
- Only one AWG channel may be active.
- Entering or starting AWG disables every DC output first. The selected AWG channel is configured and enabled only after the others are confirmed off.
- Leaving the AWG section stops waveform scheduling and turns all outputs off.
- A channel disable, overcurrent, overtemperature, I2C failure, or scheduler failure stops AWG and disables its output.
- While CH5 is physically enabled, the power service reads the TPS55289 latched,
  read-to-clear STATUS register every 20 ms. A single fault-bearing read can
  describe a completed startup transient or current-regulation event, so it is
  not by itself a shutdown request. The same fault class must reassert on the
  next 20 ms read before the service dispatches the normal typed protection trip
  and shuts down AWG. The converter's independent hardware OCP/SCP/OVP remains
  active throughout this bounded confirmation. A failed health read still fails
  closed immediately. The last raw byte remains diagnostic state after shutdown;
  UI or SCPI code may observe it but may not read or clear the converter register
  directly.
- ADC voltage-window tracking is disabled while AWG owns a channel because the 50 Hz protection sample is not phase-synchronized to waveform commands and aliases at higher frequencies. Overcurrent sampling, temperature, sensor validity, GPIO/converter verification, and I2C write/readback failure protection remain active.
- Waveform timing must use absolute monotonic deadlines and a phase accumulator. It must not set the next schedule origin to the delayed time of the previous sample.
- When an update is late, advance phase by elapsed ticks and emit the current sample. Do not issue a burst of stale catch-up setpoints.
- I2C writes occur outside interrupt handlers. The timer interrupt may only advance bounded scheduler state and signal pending work.
- The displayed frequency must be the realizable frequency derived from the tuning word and update clock.
- Frequency and waveform limits must be clamped per channel using measured output settling behavior, not only DAC or I2C bus speed.
- CH4's MCP4725 has a typical 6 microsecond DAC settling time, but the downstream voltage-margin loop and power stage are the practical limit.
- CH5 is a closed-loop buck-boost converter with programmable slew behavior. Its switching frequency is not the AWG bandwidth.
- CH5 AWG startup configures the TPS55289 for forced-PWM and its fastest documented voltage slew. PFM is prohibited for CH5 AWG because it prevents reverse inductor current at light/no load, leaving the output capacitor charged and destroying every falling waveform edge. Ordinary DC mode retains PFM. Forced-PWM selection is a typed startup side effect and must be verified before OE is enabled.
- The desktop ARB command format and integer multiplier retain their original millisecond semantics. The Rust adapter additionally accepts multiplier `0.5`, making one dwell unit one dedicated 2 kHz timer tick. Uploaded ARBs are limited to 1024 contiguous, fully initialized points inside the selected channel's voltage range. Missing chunks, zero dwell, cross-channel uploads, multiple owners, and out-of-range start parameters are rejected before any output transition. A DATA ACK means only that a chunk was accepted; START success is returned only after global shutdown and verified enable. Finite completion disables the output rather than leaving the last point energized as the C implementation does.
- The local built-in generator uses the same voltage bounds, a 125 Hz square limit, and 120 Hz triangle/ramp/interpolated-sine limits on a dedicated 2 kHz timer. This gives a 120 Hz shaped waveform about 17 command points per cycle. Absolute phase scheduling skips stale updates when foreground work is late. Oscilloscope characterization may establish lower product-output limits for large-signal operation, but the UI must not substitute an arbitrary low-frequency cap for that measurement.

The existing ARB implementation uses shared 1024-element voltage and dwell arrays, services CH4 before CH5, and schedules each point relative to the time it was actually serviced. Its timing drift and mutual-exclusion behavior are evidence for the replacement requirements, not an implementation to copy.

## Encoder and display isolation

- Encoder edge processing must debounce and enqueue an input event only. It must not draw, perform I2C, write flash, or execute an output transition inside the interrupt.
- Button processing must distinguish short release from long press without blocking. A continuous 500 ms hold returns to the main menu immediately without waiting for release. Keeping the same press held for three seconds requests a safe reboot, which must globally shut down hardware before resetting. Both thresholds use a free-running hardware millisecond counter and must be verified against wall-clock time.
- DC detail screens do not display or focus a Menu control. They rely on long press for direct return to the main menu.
- The DC screen carousel is Overview, CH1, CH2, CH3, CH4, CH5, and USB PD Input.
- CH1 through CH3 control focus cycles Output, Current Limit, and none. CH4 and CH5 focus cycles Output, Voltage Compliance, CV/CC Mode, Current Limit, and none. The mode control is rendered between the voltage and current settings.
- Rotating either direction while Output is focused toggles that channel on or off. Direction only matters for navigation and numeric adjustment.
- Every DC Power screen displays the live board temperature. This includes Overview, all five channel-detail screens, and USB PD Input. Invalid or stale temperature is shown explicitly rather than as a numeric value.
- The Overview STATE cell appends a `CC` marker for CH4 or CH5 whenever that channel is configured for constant-current regulation. The overview marker and detail mode label turn green only while the output is physically enabled and its active drive is below the voltage-compliance setting; this distinguishes active current control from merely selecting CC mode.
- Overview short clicks cycle focus through the compact CH1, CH2, CH3, CH4, and CH5 output switches, then back to no focus. Rotating either direction while an overview switch is focused dispatches the same typed output-toggle action as the detail screen. With no focus, rotation navigates screens.
- Encoder acceleration is based on elapsed time across successive detents, not on how many edges happen to remain queued in one foreground pass. Direction reversal and an 80 ms idle pause reset acceleration to fine mode.
- Display rendering is immediate and has no retained drawing tree or framebuffer requirement. Reducto state is the application state, not retained drawing state.
- AWG Channel, Waveform, Frequency, Duty, Low, High, and Output are independent view projections. An encoder edit repaints only the changed value cell; focus changes repaint only the old and new focused rows; waveform scheduler samples repaint no AWG controls. A waveform change also repaints Duty because its availability is waveform-dependent. A blanket AWG-screen or all-row invalidation for a single field change is prohibited.
- Frequency, waveform, square-wave duty, low voltage, and high voltage remain editable while AWG is running and take effect through the scheduler without restarting or energizing another output. Channel ownership is locked until Stop because changing it requires a global shutdown and a new safe-enable transaction.
- Display SPI/DMA has one owner. Rendering is serialized, bounded, and lower priority than measurement and protection.
- The C display code busy-waits indefinitely for DMA readiness and previously exhibited UI lockups when drawing from rotary paths. This must not be reproduced.

## Application API and hardware boundary

The on-device UI and USB SCPI adapter are peers. Neither is allowed to touch a driver, peripheral, converter register, persisted record, or mutable global setting directly. Both translate input into the same typed application commands and read the same immutable snapshots.

The application-facing command set should cover domain intent rather than electrical mechanisms:

- set one channel enabled or disabled
- set all DC channels enabled or disabled
- set CH4 or CH5 voltage
- set one channel current limit
- select DC or AWG operating mode
- configure AWG channel, waveform, frequency, and voltage range
- start or stop AWG
- request a deliberate USB-PD operation
- request entry to the bootloader

Commands carry fixed-point units such as millivolts, milliamps, and millihertz. Floating point and text parsing do not cross the API boundary. Every command is validated against channel capability and safe range before state changes or hardware effects are emitted.

The application publishes a coherent snapshot containing:

- operating mode and transition state
- persisted settings and active requested settings
- requested, physical, pending, and faulted state for each channel
- live voltage, current, power, sample age, and measurement validity
- board temperature and sensor validity
- AWG requested and realizable parameters, running state, and underrun count
- hardware communication health and last operation error

State changes are serialized through one bounded action queue. UI and SCPI commands cannot race each other. A long-running hardware transition has an operation identifier and completes through a hardware-result action. A command that conflicts with an in-progress safety transition returns busy rather than partially applying.

The reducer may update requested state but never emits effects or performs I/O. The power service observes old/new semantic transitions through Reducto's transition-observer boundary. It owns dependency-aware sequencing, timeouts, converter initialization state, and fail-closed rollback. It calls narrow hardware drivers for GPIO, TPS55289, MCP4725, ADC, TMP1075, STUSB4500, USB, and flash; display rendering remains exclusively in the view. Driver completion and fault events return to the application as typed actions.

An `OK` response means the requested operation completed, not merely that its text parsed. If completion is asynchronous, the SCPI adapter waits for the matching operation result with a bounded timeout. Syntax, range, mode-conflict, busy, hardware-I/O, protection, and timeout failures have distinct responses. Queries read one coherent snapshot and never initiate hardware work unless the command explicitly specifies an active diagnostic.

## SCPI-like USB compatibility

Preserve the existing USB CDC transport and established command spellings where they can be mapped safely to the typed API. The parser is an adapter only. It must not use extern globals or call GPIO and converter functions as the C parser currently does.

Compatibility targets include identification and build queries, per-channel output control, per-channel current limits, CH4 and CH5 voltage settings, per-channel and bulk measurements, converter status queries, explicit USB-PD commands, ARB upload/start, and bootloader entry. Accept the colon form emitted by the desktop client and the space form documented by the C source where both have historically worked.

ARB compatibility specifically preserves the Python client's `SOUR:WAVE:CHn:ARB:DATA start,v,d,...` grammar, eight-pair chunking, `OK:ACK:CHn` response, `SOUR:WAVE:CHn:ARB:START count,multiplier,repetitions` grammar, centivolt voltage units, integer-millisecond multiplier behavior, zero-as-continuous repetition, and final `OK:CHn_ARB_STARTED_PTS:count` response. Safety validation and the `0.5` ms multiplier extension are intentional strict supersets; do not reproduce the C buffer overflow, dual-owner, drift, or energized-completion behavior.

Preserve direct shared-converter commands such as `OUTP:DCn`, `SOUR:VOLT:DCn`, and `SOUR:CURR:DCn` for initial compatibility. They still pass through typed engineering commands and the power service. The SCPI adapter may not touch hardware directly. Their current syntax and externally useful behavior are retained where safe, while range checks, dependency-state reconciliation, and fail-closed error handling are added. Revisit whether these commands should remain public after compatibility testing.

The bulk measurement response should retain its existing field order for desktop compatibility. New fields require a versioned query rather than silently changing the legacy response. Unknown commands and malformed parameters always receive an error response.

The binary bootloader protocol is not SCPI and remains owned by the existing bootloader. The application command that enters the bootloader may erase only the known boot metadata page after outputs have been safely disabled and USB acknowledgment has been sent.

## Persistence

Persist only:

- CH4 voltage setting
- CH5 voltage setting
- CH4 and CH5 CV/CC regulation modes
- Current limit for CH1 through CH5
- USB PD input current limit
- Temperature display unit
- Three explicitly saved user profiles containing the same editable settings

The ordinary settings journal is the automatic startup configuration. Profile slots are explicit snapshots and do not replace automatic persistence. Loading any profile or Factory Defaults first stops AWG and globally disables every physical output; only after successful shutdown are validated settings applied. Saving or loading a profile must never serialize or restore output enabled state.

Do not persist output enabled states, active faults, current screen, focus, AWG running state, or live measurements. Persist the last valid AWG channel, waveform, frequency, square-wave duty, and voltage range, but never persist an armed or running state.

Settings records require a format version, monotonically increasing sequence, payload length, and CRC. A torn or corrupt record is ignored. Flash writes are deferred until the value has been quiet for a debounce interval and outputs are not in a timing-critical transition. Runtime settings change immediately in RAM. Append into blank slots during ordinary operation. If the settings page is full, defer erase/compaction until every output is confirmed physically off; never stall flash for journal maintenance while delivering power.

The STUSB4500 contains its own NVM code in this repository. That storage is for USB-PD configuration and is not the application-settings store.

## Existing C defects and inconsistencies not to preserve

- Startup enables all outputs and preregulators before converter programming.
- CH5 local UI controls hardware `EN5`, while the USB channel command controls the TPS55289 output-enable bit. These are not equivalent.
- CH5 overcurrent lowers `EN5`, resetting converter registers. Local re-enable raises only `EN5`, leaving the converter unconfigured and its output-enable bit cleared.
- Software overcurrent has no noise filtering, hysteresis, minimum consecutive sample count, or cause latch.
- The UI uses one Boolean as both desired and actual output state. A red circle means OFF as well as fault.
- Overtemperature has no recovery hysteresis and no sensor-failure policy.
- Entering the old limit-setting menu skips ADC acquisition and overcurrent checking. The new design must never suspend protection for a screen.
- Encoder callbacks execute UI drawing and voltage-changing I2C work from the highest-priority EXTI interrupt.
- Several I2C driver calls discard acknowledgement failures, and some reads return `0xff` without a typed error.
- Remote voltage commands do not consistently update, clamp, or validate the corresponding stored setting.
- Remote channel numbering comments disagree with the actual CH1 through CH3 mapping in places.
- Remote DC control has duplicated parsing branches with inconsistent accepted indices.
- ARB start does not clamp sample count to 1024 and does not enforce exclusive ownership when accepting commands.
- ARB timing accumulates lateness because each point's timestamp is set to the delayed service time.
- Active USB-PD negotiation during boot is intentionally disabled because some sources hard-reset VBUS, which can cause a reset loop when the MCU is VBUS-powered. Passive startup behavior must remain the default.

## Verification gate

Before first flash, review the Rust implementation against every statement in this document. Before enabling power on hardware, verify GPIO polarity and idle levels with outputs disconnected. First bring-up uses current-limited input power and read-only USB diagnostics. Test one channel at a time, then shared-rail interactions, overcurrent recovery, overtemperature handling, persistence across reset, and finally AWG with an oscilloscope.
