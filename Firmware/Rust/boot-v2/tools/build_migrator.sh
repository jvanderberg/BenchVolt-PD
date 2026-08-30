#!/bin/bash
# Build the one-time v1->v2 migrator: compiles the trampoline and both boot
# cores, embeds their binaries as the migrator's payload, and emits
# migrator.bin (a v1 application image for the stock bootloader).
set -euo pipefail
cd "$(dirname "$0")/.."

# Objcopy resolution: explicit override, cargo-binutils, PATH, dev machine.
OBJCOPY=${OBJCOPY:-$(command -v rust-objcopy || command -v arm-none-eabi-objcopy || echo "${PICO_TOOLCHAIN_PATH:-/Users/joshv/git/toolchains/arm-gcc}/bin/arm-none-eabi-objcopy")}

cargo build --release -p trampoline -p golden -p worker
for b in trampoline golden worker; do
    "$OBJCOPY" -O binary "target/thumbv6m-none-eabi/release/$b" "crates/migrator/payload/$b.bin"
done

cargo build --release -p migrator
"$OBJCOPY" -O binary target/thumbv6m-none-eabi/release/migrator migrator.bin
ls -l migrator.bin crates/migrator/payload/*.bin
