#![no_std]

/// Firmware identity shown on the System screen and in `SYST:BUILD?`:
/// the crate version plus the short git revision baked in by build.rs.
pub const FIRMWARE_BUILD: &str = concat!(
    "v",
    env!("CARGO_PKG_VERSION"),
    " ",
    env!("BENCHVOLT_GIT_REV")
);

pub mod app;
pub mod arb;
pub mod awg;
pub mod cadence;
pub mod dispatch;
pub mod early_shutdown;
pub mod input_policy;
pub mod limits;
pub mod math;
pub mod load;
pub mod measurement;
pub mod monitoring;
pub mod paint_queue;
pub mod pd;
pub mod power;
pub mod protocol;
pub mod reset_cause;
pub mod settings;
pub mod ui_content;
pub mod usb_command;
pub mod usb_output;
pub mod usb_query;
pub mod view_projection;
pub mod waveform;
