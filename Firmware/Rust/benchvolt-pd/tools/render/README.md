# benchvolt-render

Host-side renderer for the BenchVolt firmware UI. It compiles the real
firmware view (`src/view.rs`, included via `#[path]`) and the real
`benchvolt-pd` app/reducer/input-policy on the host, renders into an
in-memory 320x170 RGB565 framebuffer (matching the ST7789 panel), and writes
2x-scaled simulated screenshots plus an animated menu-flow GIF to
`Images/firmware/` at the repository root.

## Regenerate

```sh
cd tools/render
cargo run --release
```

This crate is deliberately NOT a member of the firmware package's workspace
(`[workspace]` table in its Cargo.toml) and carries its own
`.cargo/config.toml` pinning the host target, so the firmware's default
`thumbv6m-none-eabi` build is unaffected.

The stills set app-state fields directly (simulated telemetry) but always
render through the real view. The GIF is driven exclusively through
`input_policy` (encoder detents and button clicks), with power-executor
completions (`OutputApplied`) and monitoring telemetry dispatched the way the
firmware runtime would.
