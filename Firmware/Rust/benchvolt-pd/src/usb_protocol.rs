//! Thin I/O shim over the pure dispatcher in `benchvolt_pd::usb_query`:
//! gathers the hardware diagnostic snapshot, dispatches, and queues the
//! reply. All protocol logic lives (and is tested) in the lib crate.

use crate::input::{encoder_counts, monotonic_ms};
use crate::usb_transport::queue_usb_response;
use crate::{arb_runtime, diagnostics, display_dma};
use benchvolt_pd::{
    app::AppState,
    power::ProtectionMonitor,
    usb_command::Response,
    usb_command::UsbIntent,
    usb_query::{dispatch_command, DiagnosticsSnapshot},
};

fn diagnostics_snapshot() -> DiagnosticsSnapshot {
    let (arb_index, arb_cycles, arb_late_updates, arb_skipped_cycles) = arb_runtime::status();
    let (display_queued, display_high_water, display_active, display_overflowed, display_failed) =
        display_dma::diagnostics();
    let (encoder_edges, encoder_drops) = encoder_counts();
    DiagnosticsSnapshot {
        arb_index,
        arb_cycles,
        arb_late_updates,
        arb_skipped_cycles,
        hw_last_operation: diagnostics::last_hw_operation(),
        hw_last_error: diagnostics::last_hw_error(),
        hw_retry_count: diagnostics::hw_retry_count(),
        display_label: display_dma::lifecycle_label(),
        display_queued,
        display_high_water,
        display_active,
        display_overflowed,
        display_failed,
        display_ready_for_seal: display_dma::ready_for_seal(),
        reset_causes: diagnostics::reset_causes(),
        reset_reason: diagnostics::reset_reason(),
        tps_ch5_status: diagnostics::ch5_tps_status(),
        tick_ms: monotonic_ms(),
        loop_gap_ticks: diagnostics::take_loop_gap(),
        encoder_edges,
        encoder_drops,
    }
}

pub(crate) fn handle_usb_command(
    command: &[u8],
    state: &AppState,
    protection_monitors: &[ProtectionMonitor; 5],
) -> UsbIntent {
    let mut response = Response::new_empty();
    match dispatch_command(
        command,
        state,
        protection_monitors,
        &diagnostics_snapshot(),
        &mut response,
    ) {
        Some(intent) => intent,
        None => {
            queue_usb_response(response.as_bytes());
            UsbIntent::None
        }
    }
}
