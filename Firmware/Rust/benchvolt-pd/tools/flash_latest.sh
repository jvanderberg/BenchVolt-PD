#!/usr/bin/env bash
set -euo pipefail

tool_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
project_dir=$(CDPATH= cd -- "$tool_dir/.." && pwd -P)
image="$project_dir/target/thumbv6m-none-eabi/release/benchvolt-pd.bin"

find_python() {
    local candidate
    for candidate in \
        "$project_dir/.venv/bin/python" \
        /tmp/benchvolt-flash/bin/python \
        "$(command -v python3 2>/dev/null || true)"
    do
        if [[ -n "$candidate" && -x "$candidate" ]] \
            && "$candidate" -c 'import serial' >/dev/null 2>&1
        then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

python_bin=$(find_python || true)
if [[ -z "$python_bin" ]]; then
    echo "pyserial is unavailable; install it once with:" >&2
    echo "  python3 -m venv $project_dir/.venv" >&2
    echo "  $project_dir/.venv/bin/pip install pyserial" >&2
    exit 1
fi

if [[ ${1:-} == "--list" ]]; then
    exec "$python_bin" -m serial.tools.list_ports -v
fi

if [[ $# -ne 1 && !( $# -eq 2 && $1 == "--from-app" ) ]]; then
    echo "usage: tools/flash_latest.sh BOOTLOADER_PORT" >&2
    echo "       tools/flash_latest.sh --from-app APPLICATION_PORT" >&2
    echo "       tools/flash_latest.sh --list" >&2
    exit 2
fi

cd "$project_dir"
"$tool_dir/check.sh"

if [[ $1 == "--from-app" ]]; then
    echo "Verifying outputs are off and entering the stock bootloader"
    port=$("$python_bin" "$tool_dir/enter_bootloader.py" "$2")
    echo "bootloader port: $port"
else
    port=$1
fi

exec "$python_bin" "$tool_dir/flash_firmware.py" "$port" "$image"
