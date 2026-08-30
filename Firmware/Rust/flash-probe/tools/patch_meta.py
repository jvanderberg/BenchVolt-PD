"""Patches the stock bootloader's metadata page content for flash-probe.

Computes the CRC-32 the stock bootloader expects (poly 0x04C11DB7, init
0xFFFFFFFF, MSB-first, no reflection, no final XOR — the bitwise algorithm
in Bootloader/Core/Src/main.c) over the probe binary and emits an 8-byte
image for 0x0801F800: word 0 = CRC, word 1 = byte length.

Usage: python3 patch_meta.py flash-probe.bin
Writes flash-probe.meta.bin next to the input.
"""

import struct
import sys
from pathlib import Path

POLY = 0x04C11DB7


def crc32_mpeg2(data: bytes) -> int:
    # Matches flash_firmware.py::stm32_crc32 and the stock bootloader's
    # calculate_crc32: the byte is XORed into the low bits, MSB-first shifts.
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            if crc & 0x80000000:
                crc = ((crc << 1) ^ POLY) & 0xFFFFFFFF
            else:
                crc = (crc << 1) & 0xFFFFFFFF
    return crc


def main() -> None:
    image = Path(sys.argv[1])
    data = image.read_bytes()
    if len(data) < 192:
        sys.exit("image smaller than the 192-byte vector table")
    meta = struct.pack("<II", crc32_mpeg2(data), len(data))
    out = image.with_suffix(".meta.bin")
    out.write_bytes(meta)
    print(f"{out} (crc=0x{crc32_mpeg2(data):08x}, size={len(data)})")


if __name__ == "__main__":
    main()
