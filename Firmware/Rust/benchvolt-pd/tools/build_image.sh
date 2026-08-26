#!/usr/bin/env bash
set -euo pipefail

tool_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
project_dir=${1:-$(CDPATH= cd -- "$tool_dir/.." && pwd -P)}
project_dir=$(CDPATH= cd -- "$project_dir" && pwd -P)
thumb_target=thumbv6m-none-eabi
elf="$project_dir/target/$thumb_target/release/benchvolt-pd"
image="$elf.bin"

for command in cargo arm-none-eabi-objcopy python3; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "required command is unavailable: $command" >&2
        exit 1
    fi
done

cd "$project_dir"
cargo build --locked --release --target "$thumb_target"
arm-none-eabi-objcopy -O binary "$elf" "$image"

validation_tools="$project_dir/tools"
PYTHONPATH="$validation_tools" python3 -c '
import sys
from pathlib import Path
import flash_firmware

path = Path(sys.argv[1])
firmware = path.read_bytes()
flash_firmware.validate_image(firmware)
capacity = flash_firmware.SETTINGS_ORIGIN - flash_firmware.APP_ORIGIN
end = flash_firmware.APP_ORIGIN + len(firmware)
free = capacity - len(firmware)
minimum_free = 1024
print(f"image: {len(firmware)} bytes; end=0x{end:08x}; free={free} bytes")
if free < minimum_free:
    raise SystemExit(
        f"release image leaves {free} bytes; require at least {minimum_free} bytes free"
    )
' "$image"
