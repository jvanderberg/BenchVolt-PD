"""v2 sectioned-protocol uploader (bench tool).

Speaks the v1-framing ACK/DATA/CRC protocol with the v2 section byte:
  START <size:u32 LE> <section:u8>   (0 = application, 1 = core slot B)
  DATA  <len:u16 LE> <bytes>         (<= 60 bytes per chunk, lockstep ACK)
  END   <crc32:u32 LE>               (commit: descriptor last)

Usage: python3 flash_v2.py /dev/cu.usbmodemXXXX image.bin [section]
"""

import sys
import time

import serial

ACK = 0x06
CMD_START = 0x01
CMD_DATA = 0x02
CMD_END = 0x03
CMD_INFO = 0x10
CHUNK = 60


def crc32(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = ((crc << 1) ^ 0x04C11DB7) & 0xFFFFFFFF if crc & 0x80000000 else (crc << 1) & 0xFFFFFFFF
    return crc


def expect_ack(s: serial.Serial, what: str) -> None:
    got = s.read(1)
    if got != bytes([ACK]):
        sys.exit(f"{what}: expected ACK, got {got.hex() or '(timeout)'}")


def main() -> None:
    port, path = sys.argv[1], sys.argv[2]
    section = int(sys.argv[3]) if len(sys.argv) > 3 else 0
    data = open(path, "rb").read()

    s = serial.Serial(port, 115200, timeout=5)
    s.write(bytes([CMD_INFO]))
    info = s.read(12)
    if info[:4] != b"BV2C":
        sys.exit(f"not a v2 boot core: {info.hex() or '(no reply)'}")
    print(f"core: layout=0x{int.from_bytes(info[4:8],'little'):08x} "
          f"app_max={int.from_bytes(info[8:12],'little')}")

    # START triggers the full-section erase (~1.3 s for the app section).
    s.timeout = 10
    s.write(bytes([CMD_START]) + len(data).to_bytes(4, "little") + bytes([section]))
    expect_ack(s, "START")
    s.timeout = 5

    started = time.time()
    sent = 0
    while sent < len(data):
        chunk = data[sent:sent + CHUNK]
        s.write(bytes([CMD_DATA]) + len(chunk).to_bytes(2, "little") + chunk)
        expect_ack(s, f"DATA @{sent}")
        sent += len(chunk)
    print(f"sent {sent} bytes in {time.time() - started:.1f}s")

    s.write(bytes([CMD_END]) + crc32(data).to_bytes(4, "little"))
    expect_ack(s, "END")
    print("committed (CRC verified, descriptor programmed)")


if __name__ == "__main__":
    main()
