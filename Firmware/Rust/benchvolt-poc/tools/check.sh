#!/usr/bin/env bash
set -euo pipefail

tool_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
project_dir=$(CDPATH= cd -- "$tool_dir/.." && pwd -P)
repo_dir=$(CDPATH= cd -- "$project_dir/../../.." && pwd -P)
host_target=$(rustc -vV | sed -n 's/^host: //p')
thumb_target=thumbv6m-none-eabi
elf="$project_dir/target/$thumb_target/release/benchvolt-poc"
image="$elf.bin"

if [[ -z "$host_target" ]]; then
    echo "could not determine the Rust host target" >&2
    exit 1
fi

for command in cargo python3 clang arm-none-eabi-objcopy; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "required command is unavailable: $command" >&2
        exit 1
    fi
done

cd "$project_dir"

echo "Rust host tests ($host_target)"
cargo test --locked --lib --target "$host_target"

echo "Rust host lints"
cargo clippy --locked --lib --target "$host_target" -- -D warnings

echo "Uploader tests"
PYTHONPATH="$tool_dir" python3 -m unittest tools/test_flash_poc.py

echo "Thumb release lints and build"
cargo clippy --locked --release --target "$thumb_target" -- -D warnings
cargo build --locked --release --target "$thumb_target"
arm-none-eabi-objcopy -O binary "$elf" "$image"

PYTHONPATH="$tool_dir" python3 -c '
import sys
from pathlib import Path
import flash_poc

path = Path(sys.argv[1])
firmware = path.read_bytes()
flash_poc.validate_image(firmware)
capacity = flash_poc.SETTINGS_ORIGIN - flash_poc.APP_ORIGIN
end = flash_poc.APP_ORIGIN + len(firmware)
print(f"image: {len(firmware)} bytes; end=0x{end:08x}; free={capacity - len(firmware)} bytes")
' "$image"

common_c_flags=(
    -std=gnu11
    -include stddef.h
    -fsyntax-only
    -Wformat=2
    -Werror=return-type
    -Werror=implicit-function-declaration
    -Werror=format
    -Wno-unknown-attributes
    -Wno-language-extension-token
    -Wno-pointer-to-int-cast
    -Wno-int-to-pointer-cast
    -DUSE_HAL_DRIVER
    -DSTM32F070xB
)

check_c_project() {
    local project=$1
    local name=${project##*/}
    local includes=(
        -I"$project/Core/Inc"
        -I"$project/Drivers/STM32F0xx_HAL_Driver/Inc"
        -I"$project/Drivers/STM32F0xx_HAL_Driver/Inc/Legacy"
        -I"$project/Drivers/CMSIS/Device/ST/STM32F0xx/Include"
        -I"$project/Drivers/CMSIS/Include"
        -I"$project/USB_DEVICE/App"
        -I"$project/USB_DEVICE/Target"
        -I"$project/Middlewares/ST/STM32_USB_Device_Library/Core/Inc"
        -I"$project/Middlewares/ST/STM32_USB_Device_Library/Class/CDC/Inc"
    )
    local sources=(
        "$project"/Core/Src/*.c
        "$project"/USB_DEVICE/App/*.c
        "$project"/USB_DEVICE/Target/*.c
    )

    echo "C syntax and fatal diagnostics ($name)"
    clang "${common_c_flags[@]}" "${includes[@]}" "${sources[@]}"
}

check_c_project "$repo_dir/Firmware/USB_Power_Source"
check_c_project "$repo_dir/Firmware/Bootloader"

echo "All headless regression checks passed"
