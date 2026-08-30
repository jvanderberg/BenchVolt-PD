"""Non-destructive smoke test for the v2 sectioned upload protocol.

Exercises INFO and the NACK guard rails only — never sends a valid START, so
no flash is erased. Safe to run against a fakeapp/golden core at any time.

Usage: python3 protocol_smoke.py /dev/cu.usbmodemXXXX
"""

import sys

import serial

ACK = 0x06
NACK = 0x15
CMD_START = 0x01
CMD_DATA = 0x02
CMD_END = 0x03
CMD_INFO = 0x10


def expect(name: str, got: bytes, want: bytes) -> bool:
    ok = got == want
    print(f"{'PASS' if ok else 'FAIL'} {name}: got {got.hex() or '(nothing)'}"
          + ("" if ok else f", want {want.hex()}"))
    return ok


def main() -> None:
    port = sys.argv[1]
    s = serial.Serial(port, 115200, timeout=2)
    failures = 0

    # INFO: 12-byte identity block.
    s.write(bytes([CMD_INFO]))
    info = s.read(12)
    ok = len(info) == 12 and info[:4] == b"BV2C"
    print(f"{'PASS' if ok else 'FAIL'} INFO: {info.hex() or '(nothing)'}")
    failures += 0 if ok else 1
    if ok:
        layout = int.from_bytes(info[4:8], "little")
        app_max = int.from_bytes(info[8:12], "little")
        print(f"     layout=0x{layout:08x} app_max={app_max}")

    # Unknown command -> NACK.
    s.write(bytes([0x7F]))
    failures += 0 if expect("unknown cmd NACKed", s.read(1), bytes([NACK])) else 1

    # START with a bad section id -> NACK, nothing erased.
    s.write(bytes([CMD_START]) + (1024).to_bytes(4, "little") + bytes([9]))
    failures += 0 if expect("bad section NACKed", s.read(1), bytes([NACK])) else 1

    # START with an undersized image -> NACK (minimum 192 bytes).
    s.write(bytes([CMD_START]) + (16).to_bytes(4, "little") + bytes([0]))
    failures += 0 if expect("undersized image NACKed", s.read(1), bytes([NACK])) else 1

    # START with a malformed body -> NACK.
    s.write(bytes([CMD_START, 1, 2]))
    failures += 0 if expect("short START NACKed", s.read(1), bytes([NACK])) else 1

    # DATA with no active transfer -> NACK.
    s.write(bytes([CMD_DATA, 4, 0]) + b"abcd")
    failures += 0 if expect("orphan DATA NACKed", s.read(1), bytes([NACK])) else 1

    # END with no active transfer -> NACK.
    s.write(bytes([CMD_END]) + (0).to_bytes(4, "little"))
    failures += 0 if expect("orphan END NACKed", s.read(1), bytes([NACK])) else 1

    s.close()
    print("all passed" if failures == 0 else f"{failures} FAILURES")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
