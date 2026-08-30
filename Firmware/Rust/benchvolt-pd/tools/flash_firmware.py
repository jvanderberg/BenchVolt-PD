#!/usr/bin/env python3
"""Flash a BenchVolt application through the hardened CDC bootloader."""

import argparse
import os
import struct
import time
from pathlib import Path

CMD_START = 0x01
CMD_DATA = 0x02
CMD_END = 0x03
ACK = b"\x06"

APP_ORIGIN = 0x0800_8000
SETTINGS_ORIGIN = 0x0801_F000
if os.environ.get("BENCHVOLT_LAYOUT") == "v2":
    # v2 boot chain: app partition base, capacity ending at the in-partition
    # descriptor (validation only — v2 devices are flashed with the
    # sectioned protocol, not this uploader).
    APP_ORIGIN = 0x0800_5000
    SETTINGS_ORIGIN = 0x0801_EFC0

# The stock bootloader receives one 64-byte USB CDC packet at a time. The
# three-byte DATA header leaves 61 bytes; 60 also keeps every write aligned.
CHUNK_SIZE = 60


def stm32_crc32(data: bytes) -> int:
    crc = 0xFFFF_FFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            if crc & 0x8000_0000:
                crc = ((crc << 1) ^ 0x04C1_1DB7) & 0xFFFF_FFFF
            else:
                crc = (crc << 1) & 0xFFFF_FFFF
    return crc


def require_ack(port, operation: str) -> None:
    response = port.read(1)
    if response != ACK:
        received = "timeout" if not response else f"0x{response[0]:02x}"
        raise RuntimeError(f"{operation}: expected ACK, received {received}; aborted")


def validate_image(image: bytes) -> None:
    capacity = SETTINGS_ORIGIN - APP_ORIGIN
    if len(image) < 192:
        raise ValueError("image is smaller than the required 192-byte vector table")
    if len(image) > capacity:
        raise ValueError(
            f"image ends at 0x{APP_ORIGIN + len(image):08x}, overlapping settings"
        )

    initial_sp, reset_vector = struct.unpack_from("<II", image)
    if not 0x2000_0000 <= initial_sp <= 0x2000_4000:
        raise ValueError(f"initial stack pointer 0x{initial_sp:08x} is outside SRAM")
    if reset_vector & 1 == 0:
        raise ValueError(f"reset vector 0x{reset_vector:08x} is not a Thumb address")
    reset_address = reset_vector & ~1
    if not APP_ORIGIN <= reset_address < APP_ORIGIN + len(image):
        raise ValueError(f"reset vector 0x{reset_vector:08x} is outside the image")


def flash(port_name: str, image: bytes) -> None:
    import serial

    crc = stm32_crc32(image)
    print(f"image: {len(image)} bytes, end=0x{APP_ORIGIN + len(image):08x}, crc=0x{crc:08x}")

    with serial.Serial(port_name, 115200, timeout=15, write_timeout=15, exclusive=True) as port:
        time.sleep(0.5)
        port.reset_input_buffer()

        port.write(struct.pack("<BI", CMD_START, len(image)))
        port.flush()
        require_ack(port, "START/erase")
        print("START: ACK")

        for offset in range(0, len(image), CHUNK_SIZE):
            chunk = image[offset : offset + CHUNK_SIZE]
            port.write(struct.pack("<BH", CMD_DATA, len(chunk)) + chunk)
            port.flush()
            require_ack(port, f"DATA at 0x{APP_ORIGIN + offset:08x}")
            if offset == 0 or offset + len(chunk) == len(image) or offset % 3000 == 0:
                print(f"DATA: {offset + len(chunk)}/{len(image)} bytes ACKed")

        port.write(struct.pack("<BI", CMD_END, crc))
        port.flush()
        require_ack(port, "END/CRC")
        print("END: ACK; CRC sealed, bootloader is jumping to the application")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("port", help="stock bootloader CDC port, e.g. /dev/cu.usbmodem...")
    parser.add_argument("image", type=Path, help="application .bin linked at 0x08008000")
    args = parser.parse_args()

    image = args.image.read_bytes()
    validate_image(image)
    flash(args.port, image)


if __name__ == "__main__":
    main()
