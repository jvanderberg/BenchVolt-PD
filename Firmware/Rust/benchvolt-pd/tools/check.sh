#!/usr/bin/env bash
set -euo pipefail

tool_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
project_dir=$(CDPATH= cd -- "$tool_dir/.." && pwd -P)
repo_dir=$(CDPATH= cd -- "$project_dir/../../.." && pwd -P)
host_target=$(rustc -vV | sed -n 's/^host: //p')
thumb_target=thumbv6m-none-eabi

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
# --tests matches CI: the integration and fuzz crates (and their shared mock
# driver) must compile and pass too, not just the lib. --no-default-features
# excludes the ARM-only firmware bin (and its test harness) from the host
# build, exactly as CI does.
cargo test --locked --lib --tests --no-default-features --target "$host_target"

echo "Rust host lints"
cargo clippy --locked --lib --target "$host_target" -- -D warnings

echo "Uploader tests"
PYTHONPATH="$tool_dir" python3 -m unittest discover -s tools -p 'test_*.py'

echo "Thumb release lints and build"
cargo clippy --locked --release --target "$thumb_target" -- -D warnings
"$tool_dir/build_image.sh"

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
