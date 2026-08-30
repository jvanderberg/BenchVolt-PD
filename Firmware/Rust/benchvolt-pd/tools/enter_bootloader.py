#!/usr/bin/env python3
"""Safely move a running BenchVolt application into its stock bootloader."""

import argparse
import sys
import time


def query_line(port, command: bytes) -> bytes:
    port.reset_input_buffer()
    port.write(command + b"\n")
    port.flush()
    response = port.readline().strip()
    if not response:
        raise RuntimeError(f"{command.decode()}: timed out")
    return response


def require_outputs_off(response: bytes) -> None:
    fields = response.split(b",")
    if len(fields) != 27:
        raise RuntimeError(f"MEAS:ALL?: expected 27 fields, received {len(fields)}")
    active = fields[13:20]
    if active != [b"0"] * 7:
        raise RuntimeError(
            "refusing bootloader transition: an output or ARB channel is active"
        )


def enter(application_port: str) -> str:
    import serial
    from serial.tools import list_ports

    with serial.Serial(application_port, 115200, timeout=3, write_timeout=3, exclusive=True) as port:
        time.sleep(0.25)
        require_outputs_off(query_line(port, b"MEAS:ALL?"))
        response = query_line(port, b"JUMP:BOOTLOADER")
        if response != b"OK:JUMPING_TO_BOOTLOADER":
            raise RuntimeError(f"bootloader request failed: {response.decode(errors='replace')}")

    # The application presents the bootloader's chip-unique USB serial so the
    # desktop GUI can reopen one port name across the jump; the bootloader is
    # recognized by its distinct product string, not by a new port name.
    # 30 s: the v2 boot core holds a ~2 s disconnect during handover and
    # host driver attach can add several more on a busy bus.
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        for candidate in list_ports.comports():
            description = candidate.description or ""
            if "STM32 Virtual ComPort" in description:
                return candidate.device
        time.sleep(0.25)
    raise RuntimeError("updater port did not enumerate within 30 seconds")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("application_port")
    args = parser.parse_args()
    try:
        print(enter(args.application_port))
    except (OSError, RuntimeError) as error:
        print(error, file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
