//! Services one framed USB command per loop pass: parse via the pure
//! dispatcher, then execute the typed intent against the runtime. Every
//! reply is queued here; `main` only calls [`service_usb_command`].

use core::fmt::Write as _;

use benchvolt_pd::app::{Action, AwgSource, AwgStatus};
use benchvolt_pd::arb::UploadSession as ArbUploadSession;
use benchvolt_pd::monitoring::ProtectionService;
use benchvolt_pd::pd::Service as PdService;
use benchvolt_pd::reset_cause::ResetReason;
use benchvolt_pd::usb_command::{
    output_completion_response, pd_completion_response, pd_diagnostics_response, UsbIntent,
};
use benchvolt_pd::usb_output::{Admission, OutputTransaction, RequestResult};
use benchvolt_pd::waveform::Service as WaveformService;
use benchvolt_pd::early_shutdown::raw_emergency_shutdown;
use heapless::String;

use crate::arb_runtime;
use crate::board::i2c::SoftPdBus;
use crate::boot::{erase_flash_page, BOOT_METADATA_ADDR};
use crate::display_dma;
use crate::runtime::{
    confirmed_global_shutdown, dispatch_app, set_current_limit, set_regulation_mode, set_voltage,
    stop_awg_confirmed,
};
use crate::types::{FirmwareApp, FirmwarePower, PdI2c};
use crate::usb_protocol::handle_usb_command;
use crate::usb_transport::{queue_usb_response, take_usb_command};

pub(crate) struct UsbCtx<'a> {
    pub app: &'a mut FirmwareApp,
    pub power: &'a mut FirmwarePower,
    pub pd_bus: &'a mut PdI2c,
    pub pd_service: &'a mut PdService,
    pub waveform: &'a mut WaveformService,
    pub arb_upload: &'a mut ArbUploadSession,
    pub usb_output: &'a mut OutputTransaction,
    pub pending_awg_ack: &'a mut Option<u8>,
}

fn awg_engine_busy(ctx: &UsbCtx) -> bool {
    !matches!(
        ctx.app.state().awg_status,
        AwgStatus::Stopped | AwgStatus::Fault
    )
}

pub(crate) fn service_usb_command(ctx: &mut UsbCtx, protection: &ProtectionService) {
    let Some(command) = take_usb_command() else {
        return;
    };
    match handle_usb_command(
        command.as_slice(),
        ctx.app.state(),
        protection.channel_monitors(),
    ) {
        UsbIntent::None => {}
        UsbIntent::JumpToBootloader => {
            // A bootloader transition is also a global safety transition.
            // Attempt every independent off control before resetting, even
            // if one driver operation reports a failure.
            if !confirmed_global_shutdown(ctx.app, ctx.power) {
                queue_usb_response(b"ERR:HARDWARE\r\n");
                return;
            }
            if !erase_flash_page(BOOT_METADATA_ADDR) {
                unsafe { raw_emergency_shutdown() };
                queue_usb_response(b"ERR:FLASH\r\n");
                return;
            }
            queue_usb_response(b"OK:JUMPING_TO_BOOTLOADER\r\n");
            unsafe { raw_emergency_shutdown() };
            cortex_m::asm::delay(4_800_000);
            crate::emergency_reset(ResetReason::BootloaderRequest);
        }
        UsbIntent::Reboot => {
            dispatch_app(ctx.app, ctx.power, Action::RequestReboot);
            queue_usb_response(b"OK:REBOOTING\r\n");
        }
        UsbIntent::SetOutput { channel, enabled } => {
            if enabled && display_dma::has_failed() {
                queue_usb_response(b"ERR:DISPLAY\r\n");
                return;
            }
            match ctx.usb_output.begin_request(
                channel,
                enabled,
                ctx.power.is_busy(),
                ctx.pd_service.command_pending(),
            ) {
                Admission::Proceed => {}
                Admission::ProceedAfterCancellation => {
                    queue_usb_response(b"ERR:CANCELLED\r\n");
                }
                Admission::Busy => {
                    queue_usb_response(b"ERR:BUSY\r\n");
                    return;
                }
            }
            if enabled && awg_engine_busy(ctx) {
                queue_usb_response(b"ERR:BUSY\r\n");
                return;
            }
            if !enabled
                && channel == ctx.app.state().active_awg_channel()
                && matches!(
                    ctx.app.state().awg_status,
                    AwgStatus::StartRequested | AwgStatus::Starting | AwgStatus::StopRequested
                )
            {
                // The AWG start/stop sequence owns the channel for a
                // bounded window; report busy instead of letting the
                // reducer's guard surface a bogus ERR:HARDWARE.
                queue_usb_response(b"ERR:BUSY\r\n");
                return;
            }
            if !enabled
                && channel == ctx.app.state().active_awg_channel()
                && ctx.app.state().awg_status == AwgStatus::Running
            {
                if ctx.app.state().awg_source == AwgSource::Arbitrary {
                    ctx.waveform.cancel_arbitrary(channel);
                }
                queue_usb_response(stop_awg_confirmed(ctx.app, ctx.power));
                return;
            }
            dispatch_app(
                ctx.app,
                ctx.power,
                Action::SetOutputRequested { channel, enabled },
            );
            let output = &ctx.app.state().channels[usize::from(channel)];
            if let RequestResult::Complete(result) =
                ctx.usb_output.record_request(channel, enabled, output)
            {
                queue_usb_response(output_completion_response(result));
            }
        }
        UsbIntent::SetCurrentLimit { channel, milliamps } => {
            queue_usb_response(set_current_limit(ctx.app, ctx.power, channel, milliamps));
        }
        UsbIntent::SetVoltage {
            channel,
            millivolts,
        } => {
            queue_usb_response(set_voltage(ctx.app, ctx.power, channel, millivolts));
        }
        UsbIntent::SetRegulationMode { channel, mode } => {
            queue_usb_response(set_regulation_mode(ctx.app, ctx.power, channel, mode));
        }
        UsbIntent::SetSinkCurrentLimit(milliamps) => {
            if ctx.pd_service.command_pending() {
                queue_usb_response(b"ERR:BUSY\r\n");
                return;
            }
            dispatch_app(ctx.app, ctx.power, Action::SetSinkCurrentLimit(milliamps));
            if ctx.app.state().sink_current_limit_ma == milliamps {
                queue_usb_response(b"OK\r\n");
            } else {
                queue_usb_response(b"ERR:RANGE\r\n");
            }
        }
        UsbIntent::PdDiagnostics => {
            let result = benchvolt_pd::pd::read_diagnostics(&mut SoftPdBus::new(
                ctx.pd_bus,
                ctx.power.delay_mut(),
            ));
            match result {
                Ok(snapshot) => {
                    let response = pd_diagnostics_response(snapshot);
                    queue_usb_response(response.as_bytes());
                }
                Err(_) => queue_usb_response(b"ERR:PD:BUS\r\n"),
            }
        }
        UsbIntent::PdNegotiate => {
            let outputs_off = ctx.app.state().outputs_inactive();
            if ctx.pd_service.command_pending() || ctx.power.is_busy() || !outputs_off {
                queue_usb_response(b"ERR:BUSY\r\n");
                return;
            }
            let result = benchvolt_pd::pd::configure_request_source_current(&mut SoftPdBus::new(
                ctx.pd_bus,
                ctx.power.delay_mut(),
            ));
            match result {
                Ok(benchvolt_pd::pd::NvmUpdate::Updated) => {
                    queue_usb_response(b"OK:PD:NVM_UPDATED:POWER_CYCLE\r\n")
                }
                Ok(benchvolt_pd::pd::NvmUpdate::AlreadyConfigured) => {
                    let result = benchvolt_pd::pd::request_legacy_boot_contract(
                        &mut SoftPdBus::new(ctx.pd_bus, ctx.power.delay_mut()),
                    );
                    queue_usb_response(pd_completion_response(result));
                }
                Err(_) => queue_usb_response(b"ERR:PD:NVM\r\n"),
            }
        }
        UsbIntent::PdList => {
            if ctx.pd_service.command_pending() {
                queue_usb_response(b"ERR:BUSY\r\n");
                return;
            }
            let result = benchvolt_pd::pd::read_source_capabilities(&mut SoftPdBus::new(
                ctx.pd_bus,
                ctx.power.delay_mut(),
            ));
            // The desktop GUI collects lines between these markers and
            // ignores any line that is not "index,mv,ma,mw".
            let mut listing: String<176> = String::new();
            listing.push_str("UI_PDO_LIST_START\r\n").ok();
            match result {
                Ok((raw_pdos, count)) => {
                    // Unlike the original C firmware, list only valid
                    // fixed-supply objects: some sources lead with a
                    // malformed or augmented object whose blind field
                    // extraction produced an unselectable phantom row.
                    for (index, raw) in raw_pdos[..count].iter().enumerate() {
                        let Some(pdo) = benchvolt_pd::pd::decode_fixed_pdo(*raw, index as u8 + 1)
                        else {
                            continue;
                        };
                        let millivolts = u32::from(pdo.millivolts);
                        let milliamps = u32::from(pdo.milliamps);
                        write!(
                            &mut listing,
                            "{},{},{},{}\r\n",
                            index as u32,
                            millivolts,
                            milliamps,
                            millivolts * milliamps / 1_000
                        )
                        .ok();
                    }
                }
                Err(_) => {
                    listing.push_str("ERR:PD:BUS\r\n").ok();
                }
            }
            queue_usb_response(listing.as_bytes());
            queue_usb_response(b"UI_PDO_LIST_END\r\n");
        }
        UsbIntent::PdoSet {
            slot,
            millivolts,
            milliamps,
        } => {
            let outputs_off = ctx.app.state().outputs_inactive();
            if ctx.pd_service.command_pending() || ctx.power.is_busy() || !outputs_off {
                queue_usb_response(b"ERR:BUSY\r\n");
                return;
            }
            let result = benchvolt_pd::pd::set_sink_pdo(
                &mut SoftPdBus::new(ctx.pd_bus, ctx.power.delay_mut()),
                slot,
                millivolts,
                milliamps,
            );
            match result {
                Ok(()) => queue_usb_response(b"OK:PD_PROFILED\r\n"),
                Err(benchvolt_pd::pd::PdError::NoSuitablePdo) => {
                    queue_usb_response(b"ERR:PARAM_FORMAT\r\n")
                }
                Err(_) => queue_usb_response(b"ERR:PD_WRITE_FAILED\r\n"),
            }
        }
        UsbIntent::ArbData(chunk) => {
            if awg_engine_busy(ctx) {
                queue_usb_response(b"ERR:BUSY\r\n");
                return;
            }
            if ctx.arb_upload.accept(chunk).is_err() {
                queue_usb_response(b"ERR:SEQUENCE\r\n");
                return;
            }
            arb_runtime::write(chunk);
            let mut response: String<32> = String::new();
            write!(&mut response, "OK:ACK:CH{}\r\n", u32::from(chunk.channel) + 1).ok();
            queue_usb_response(response.as_bytes());
        }
        UsbIntent::ArbStart(start) => {
            if awg_engine_busy(ctx) {
                queue_usb_response(b"ERR:BUSY\r\n");
                return;
            }
            if !ctx.arb_upload.is_complete_for(start) {
                queue_usb_response(b"ERR:INCOMPLETE\r\n");
                return;
            }
            let bounds = arb_runtime::validate(start);
            let Some((initial_mv, low_mv, high_mv)) = bounds else {
                queue_usb_response(b"ERR:RANGE\r\n");
                return;
            };
            dispatch_app(
                ctx.app,
                ctx.power,
                Action::RequestArbStart {
                    channel: start.channel,
                    initial_mv,
                    low_mv,
                    high_mv,
                },
            );
            if ctx.app.state().awg_status == AwgStatus::StartRequested
                && ctx.app.state().awg_source == AwgSource::Arbitrary
            {
                arb_runtime::reset_status();
                // The buffer now belongs to this run; require a fresh
                // contiguous upload before it can be started again.
                ctx.arb_upload.invalidate();
                ctx.waveform.arm_arbitrary(start);
            } else {
                queue_usb_response(b"ERR:BUSY\r\n");
            }
        }
        UsbIntent::ArbStop(channel) => {
            ctx.waveform.cancel_arbitrary(channel);
            if ctx.app.state().awg_source == AwgSource::Arbitrary
                && ctx.app.state().active_awg_channel() == channel
                && !matches!(ctx.app.state().awg_status, AwgStatus::Stopped)
            {
                let reply = stop_awg_confirmed(ctx.app, ctx.power);
                if reply == b"OK\r\n" {
                    ctx.waveform.stop_arbitrary();
                }
                queue_usb_response(reply);
            } else {
                queue_usb_response(b"OK\r\n");
            }
        }
        UsbIntent::AwgConfigure(config) => {
            if awg_engine_busy(ctx) {
                queue_usb_response(b"ERR:BUSY\r\n");
                return;
            }
            dispatch_app(ctx.app, ctx.power, Action::ConfigureAwg(config));
            if ctx.app.state().awg == config {
                queue_usb_response(b"OK\r\n");
            } else {
                queue_usb_response(b"ERR:RANGE\r\n");
            }
        }
        UsbIntent::AwgRun(channel) => {
            if ctx.app.state().awg.channel != channel {
                queue_usb_response(b"ERR:RANGE\r\n");
                return;
            }
            dispatch_app(ctx.app, ctx.power, Action::RequestAwgStart);
            if ctx.app.state().awg_status == AwgStatus::StartRequested
                && ctx.app.state().awg_source == AwgSource::Builtin
            {
                *ctx.pending_awg_ack = Some(channel);
            } else {
                queue_usb_response(b"ERR:BUSY\r\n");
            }
        }
        UsbIntent::AwgStop(channel) => {
            if ctx.app.state().awg_source == AwgSource::Builtin
                && ctx.app.state().awg.channel == channel
                && !matches!(ctx.app.state().awg_status, AwgStatus::Stopped)
            {
                *ctx.pending_awg_ack = None;
                queue_usb_response(stop_awg_confirmed(ctx.app, ctx.power));
            } else {
                queue_usb_response(b"OK\r\n");
            }
        }
    }
}
