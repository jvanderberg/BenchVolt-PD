# BenchVolt-PD

Desktop control application for the BenchVolt-PD USB PD power supply
(STM32-based). Talks to the hardware over USB CDC (virtual serial port)
using a simple SCPI-style command set, and provides a GUI for setting
voltages, PDO profiles, output channel control, waveform/ARB upload,
and firmware updates via the bootloader protocol.

## Downloads

Prebuilt executables are attached to the [v1.0 release](https://github.com/sydundar/BenchVolt-PD/releases/tag/v1.0) — no Python install needed:

- 🐧 [BenchVolt-PD-linux.tar.gz](https://github.com/sydundar/BenchVolt-PD/releases/download/v1.0/BenchVolt-PD-linux.tar.gz) — Linux (extract and run)
- 🪟 [BenchVolt-PD.exe.zip](https://github.com/sydundar/BenchVolt-PD/releases/download/v1.0/BenchVolt-PD.zip) — Windows

## Contents

| File                     | Purpose                                             |
|---------------------------|------------------------------------------------------|
| `BenchVolt-PD.py`         | Main application (GUI, bootloader protocol, plotting) |
| `benchvolt_remote.py`     | Serial/SCPI communication layer with the STM32        |
| `BenchVolt-PD.spec`       | PyInstaller build spec (used for both Linux & Windows) |
| `build_windows.bat`       | One-click Windows build script                        |
| `BenchVolt-PD-linux.tar.gz` | Prebuilt Linux executable (also on the Releases page) |
| `BenchVolt-PD.exe.zip`        | Prebuilt Windows executable (also on the Releases page) |

## Requirements

- Python 3.10+
- Packages: `customtkinter`, `numpy`, `matplotlib`, `pyserial`
- Linux only: `python3-tk` (Tkinter) must be installed via your system
  package manager, e.g. `sudo apt install python3-tk`

Install Python dependencies:
```bash
pip install customtkinter numpy matplotlib pyserial
```

## Running from source

```bash
python BenchVolt-PD.py
```

Make sure `benchvolt_remote.py` is in the same folder — it's imported
directly by the main script.

## Running the prebuilt Linux executable

```bash
tar -xzf BenchVolt-PD-linux.tar.gz
./BenchVolt-PD-linux.bin
```

The executable bit is preserved by the tar archive, so no `chmod`
should be needed. If you do hit a permissions error:
```bash
chmod +x BenchVolt-PD-linux.bin
```

Notes:
- Requires a desktop environment (it's a Tkinter GUI app); it will not
  run on a headless server.
- Built on a recent glibc (Ubuntu 24.04-based); very old distros
  (pre-2022) may report a missing `GLIBC_2.3x` symbol.
- To access the serial port (`/dev/ttyACM0` / `/dev/ttyUSB0`) without
  root, add your user to the `dialout` group, then log out/in:
  ```bash
  sudo usermod -aG dialout $USER
  ```

## Running the prebuilt Windows executable

Download `BenchVolt-PD.exe` from the [v1.0 release](https://github.com/sydundar/BenchVolt-PD/releases/tag/v1.0) and double-click it — no installation required. Windows SmartScreen may warn about an unsigned executable on first run; click "More info" → "Run anyway" to proceed.

## Building the Windows executable

PyInstaller does not cross-compile — a Windows `.exe` must be built on
an actual Windows machine.

1. Copy these four files into the same folder on Windows:
   - `BenchVolt-PD.py`
   - `benchvolt_remote.py`
   - `BenchVolt-PD.spec`
   - `build_windows.bat`
2. Install Python from https://www.python.org/downloads/ if not
   already installed (check "Add Python to PATH" during setup).
3. Double-click `build_windows.bat`. It will:
   - install `pyinstaller`, `customtkinter`, `numpy`, `matplotlib`,
     `pyserial` via pip
   - build the executable using `BenchVolt-PD.spec`
4. Output: `dist\BenchVolt-PD.exe`

## Building the Linux executable manually

```bash
pip install pyinstaller customtkinter numpy matplotlib pyserial
pyinstaller BenchVolt-PD.spec
```
Output: `dist/BenchVolt-PD`

## Hardware protocol notes

Full command syntax, reply formats, and scripting examples are in the
[SCPI interface reference](../Docs/scpi-interface.md).

- SCPI-style commands over USB CDC, e.g. `SOUR:VOLT:CH4 5.00`,
  `OUTP:CH1:STAT 1`, `MEAS:VOLT:CH2?`.
- Standard waveforms (square, triangle, ramp, sine) run on the firmware's
  built-in AWG engine via `SOUR:WAVE:CHn:FUNC <SQU|TRI|RAMP|SIN>,<freq_millihz>,
  <duty_pct>,<low_mv>,<high_mv>` plus `SOUR:WAVE:CHn:RUN` / `:STOP` / `:STAT?`.
  The limits match the on-device UI: 125 Hz square, 120 Hz shaped, 0.1 Hz
  minimum. `RUN` acknowledges only after the output is actually running.
- Custom ARB uploads retain the original `ARB:DATA` / `ARB:START` protocol;
  CSV files may specify `MULTIPLIER=0.5` (one 2 kHz scheduler tick per dwell
  unit) or a legacy positive integer (milliseconds). ARB timing is command
  rate, not guaranteed analog bandwidth.
- `SYST:PD:CONTRACT?` reports the negotiated PD contract as
  `position,millivolts,milliamps` (or `NONE`); the GUI uses it to auto-select
  the active PDO after connecting.
- PDO list query (`SOUR:PD:LIST?`) streams data between
  `UI_PDO_LIST_START` / `UI_PDO_LIST_END` markers.
- Firmware bootloader protocol uses `CMD_START` / `CMD_DATA` /
  `CMD_END` / `CMD_JUMP_ONLY` / `CMD_ERASE_CRC` frames with CRC32
  validation before flashing to `MAIN_APP_ADDR` (`0x08008000`).
