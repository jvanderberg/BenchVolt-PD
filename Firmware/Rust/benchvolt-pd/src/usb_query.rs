//! Pure USB command dispatcher: maps one framed command line plus a
//! snapshot of application/diagnostic state to either a formatted reply or
//! a typed [`UsbIntent`]. No I/O happens here — the binary crate gathers
//! [`DiagnosticsSnapshot`] from its hardware singletons and queues whatever
//! reply this module returns — so every protocol branch is host-testable.

use core::fmt::Write as _;

use crate::usb_command::Response;

use crate::app::{
    AppState, AwgConfig, AwgSource, AwgStatus, AwgWaveform, ControlFocus, Fault, RegulationMode,
    Screen,
};
use crate::arb::{
    parse_data as parse_arb_data, parse_start as parse_arb_start, ParseError as ArbParseError,
};
use crate::power::ProtectionMonitor;
use crate::protocol::parse_milliunits;
use crate::usb_command::{
    parse_compat_mutation, project_compat_query, temperature_response, CommandError, UsbIntent,
};

/// Values sampled from the binary crate's hardware-coupled singletons just
/// before dispatch. Plain data so the dispatcher stays pure.
#[derive(Clone, Copy, Default)]
pub struct DiagnosticsSnapshot {
    pub arb_index: u32,
    pub arb_cycles: u32,
    pub arb_late_updates: u32,
    pub arb_skipped_cycles: u32,
    pub hw_last_operation: u8,
    pub hw_last_error: u8,
    pub hw_retry_count: u32,
    pub display_label: &'static str,
    pub display_queued: usize,
    pub display_high_water: u16,
    pub display_active: bool,
    pub display_overflowed: bool,
    pub display_failed: bool,
    pub display_ready_for_seal: bool,
    pub reset_causes: u8,
    pub reset_reason: u8,
    pub tps_ch5_status: u8,
    pub tick_ms: u16,
    pub encoder_edges: u32,
    pub encoder_drops: u32,
}

fn reply(out: &mut Response, text: &str) -> Option<UsbIntent> {
    let _ = out.push_bytes(text.as_bytes());
    None
}

/// Parse `SQU|TRI|RAMP|SIN,<freq_millihz>,<duty_pct>,<low_mv>,<high_mv>`.
/// Only syntax lives here; range checks belong to the reducer.
fn parse_awg_func(channel: u8, args: &[u8]) -> Option<AwgConfig> {
    let mut fields = args.split(|byte| *byte == b',');
    let waveform = match fields.next()? {
        b"SQU" => AwgWaveform::Square,
        b"TRI" => AwgWaveform::Triangle,
        b"RAMP" => AwgWaveform::Ramp,
        b"SIN" => AwgWaveform::Sine,
        _ => return None,
    };
    let mut number = || {
        let field = fields.next()?;
        if field.is_empty() || field.iter().any(|byte| !byte.is_ascii_digit()) {
            return None;
        }
        field.iter().try_fold(0u32, |value, byte| {
            value.checked_mul(10)?.checked_add(u32::from(*byte - b'0'))
        })
    };
    let frequency_millihz = number()?;
    let duty_percent = u8::try_from(number()?).ok()?;
    let low_mv = u16::try_from(number()?).ok()?;
    let high_mv = u16::try_from(number()?).ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some(AwgConfig {
        channel,
        waveform,
        frequency_millihz,
        duty_percent,
        low_mv,
        high_mv,
    })
}

fn finish(out: &mut Response, response: &Response) -> Option<UsbIntent> {
    let _ = out.push_bytes(response.as_bytes());
    None
}

/// Dispatch one framed command. Returns `Some(intent)` for typed mutations;
/// otherwise writes the reply into `out` and returns `None`.
pub fn dispatch_command(
    command: &[u8],
    state: &AppState,
    protection_monitors: &[ProtectionMonitor; 5],
    diagnostics: &DiagnosticsSnapshot,
    out: &mut Response,
) -> Option<UsbIntent> {
    let command = command.strip_suffix(b"\r").unwrap_or(command);
    if let Some(rest) = command.strip_prefix(b"SOUR:WAVE:CH") {
        if let Some((&digit, tail)) = rest.split_first() {
            if matches!(digit, b'4' | b'5') {
                // 0-based output index (3 or 4).
                let channel = digit - b'1';
                let owner_status = |owned: bool| {
                    if owned {
                        match state.awg_status {
                            AwgStatus::Running => "RUNNING",
                            AwgStatus::StartRequested | AwgStatus::Starting => "STARTING",
                            AwgStatus::StopRequested => "STOPPING",
                            AwgStatus::Fault => "FAULT",
                            AwgStatus::Stopped => "STOPPED",
                        }
                    } else {
                        "STOPPED"
                    }
                };
                match tail {
                    b":ARB:STAT?" => {
                        let owner = state.awg_source == AwgSource::Arbitrary
                            && state.active_awg_channel() == channel;
                        let mut response = Response::new_empty();
                        write!(
                            &mut response,
                            "{},INDEX:{},CYCLES:{},LATE:{},SKIP:{}\r\n",
                            owner_status(owner),
                            diagnostics.arb_index,
                            diagnostics.arb_cycles,
                            diagnostics.arb_late_updates,
                            diagnostics.arb_skipped_cycles,
                        )
                        .ok();
                        return finish(out, &response);
                    }
                    b":ARB:STOP" => return Some(UsbIntent::ArbStop(channel)),
                    b":STAT?" => {
                        let owner =
                            state.awg_source == AwgSource::Builtin && state.awg.channel == channel;
                        let mut response = Response::new_empty();
                        write!(&mut response, "{}\r\n", owner_status(owner)).ok();
                        return finish(out, &response);
                    }
                    b":RUN" => return Some(UsbIntent::AwgRun(channel)),
                    b":STOP" => return Some(UsbIntent::AwgStop(channel)),
                    _ => {
                        if let Some(args) = tail.strip_prefix(b":FUNC ") {
                            return match parse_awg_func(channel, args) {
                                Some(config) => Some(UsbIntent::AwgConfigure(config)),
                                None => reply(out, "ERR:SYNTAX\r\n"),
                            };
                        }
                    }
                }
            }
        }
    }
    if command == b"SOUR:WAVE:FUNC?" {
        // The on-device AWG configuration is the single source of truth the
        // desktop GUI syncs its waveform panel from.
        let mut response = Response::new_empty();
        write!(
            &mut response,
            "CH{},{},{},{},{},{}\r\n",
            state.awg.channel + 1,
            match state.awg.waveform {
                AwgWaveform::Square => "SQU",
                AwgWaveform::Triangle => "TRI",
                AwgWaveform::Ramp => "RAMP",
                AwgWaveform::Sine => "SIN",
            },
            state.awg.frequency_millihz,
            state.awg.duty_percent,
            state.awg.low_mv,
            state.awg.high_mv,
        )
        .ok();
        return finish(out, &response);
    }
    if command == b"SYST:PD:CONTRACT?" {
        let mut response = Response::new_empty();
        match state.pd_contract {
            Some(contract) => write!(
                &mut response,
                "{},{},{}\r\n",
                contract.source_position, contract.millivolts, contract.operating_milliamps,
            )
            .ok(),
            None => response.write_str("NONE\r\n").ok(),
        };
        return finish(out, &response);
    }
    match parse_arb_data(command) {
        Ok(Some(chunk)) => return Some(UsbIntent::ArbData(chunk)),
        Err(ArbParseError::Syntax) => return reply(out, "ERR:SYNTAX\r\n"),
        Err(ArbParseError::Range) => return reply(out, "ERR:RANGE\r\n"),
        Ok(None) => {}
    }
    match parse_arb_start(command) {
        Ok(Some(start)) => return Some(UsbIntent::ArbStart(start)),
        Err(ArbParseError::Syntax) => return reply(out, "ERR:SYNTAX\r\n"),
        Err(ArbParseError::Range) => return reply(out, "ERR:RANGE\r\n"),
        Ok(None) => {}
    }
    if let Some(rest) = command.strip_prefix(b"SYST:PROT:CH") {
        let Some(channel) = rest
            .first()
            .and_then(|byte| byte.checked_sub(b'1'))
            .filter(|channel| *channel < 5 && rest.get(1..) == Some(b"?"))
        else {
            return reply(out, "ERR:RANGE\r\n");
        };
        let snapshot = protection_monitors[usize::from(channel)].snapshot();
        let mut response = Response::new_empty();
        write!(
            &mut response,
            "A{} R{},{} P{} G{} O{} V{} T{},{} N{}\r\n",
            u8::from(snapshot.active),
            snapshot.last.millivolts,
            snapshot.last.milliamps,
            snapshot.peak_milliamps,
            snapshot.grace_remaining,
            snapshot.overcurrent_samples,
            snapshot.voltage_samples,
            snapshot.trip.millivolts,
            snapshot.trip.milliamps,
            snapshot.samples_since_enable,
        )
        .ok();
        return finish(out, &response);
    }
    if let Some(rest) = command.strip_prefix(b"OUTP:CH") {
        if rest.len() == 2 && rest[1] == b'?' {
            let Some(channel) = rest[0].checked_sub(b'1').filter(|channel| *channel < 5) else {
                return reply(out, "ERR:RANGE\r\n");
            };
            let output = &state.channels[usize::from(channel)];
            let status = match output.fault {
                Fault::OverCurrent => "FAULT:OVERCURRENT",
                Fault::OverTemperature => "FAULT:OVERTEMP",
                Fault::Sensor => "FAULT:SENSOR",
                Fault::Hardware => "FAULT:HARDWARE",
                Fault::None if output.physical_enabled => "ON",
                Fault::None => "OFF",
            };
            let mut response = Response::new_empty();
            write!(&mut response, "{}\r\n", status).ok();
            return finish(out, &response);
        }
    }
    match project_compat_query(command, state) {
        Ok(Some(response)) => return finish(out, &response),
        Err(CommandError::Syntax) => return reply(out, "ERR:SYNTAX\r\n"),
        Err(CommandError::Range) => return reply(out, "ERR:RANGE\r\n"),
        Ok(None) => {}
    }
    match parse_compat_mutation(command) {
        Ok(Some(intent)) => return Some(intent),
        Err(CommandError::Syntax) => return reply(out, "ERR:SYNTAX\r\n"),
        Err(CommandError::Range) => return reply(out, "ERR:RANGE\r\n"),
        Ok(None) => {}
    }
    if let Some(rest) = command.strip_prefix(b"SOUR:CURR:CH") {
        let Some(channel) = rest.first().and_then(|byte| byte.checked_sub(b'1')) else {
            return reply(out, "ERR:RANGE\r\n");
        };
        if channel >= 5 {
            return reply(out, "ERR:RANGE\r\n");
        }
        if rest.get(1..) == Some(b"?") {
            let limit = state.channels[usize::from(channel)].current_limit_ma;
            let mut response = Response::new_empty();
            write!(&mut response, "{}.{:03}A\r\n", limit / 1_000, limit % 1_000).ok();
            return finish(out, &response);
        }
        let Some(value) = rest.get(2..).filter(|_| rest.get(1) == Some(&b' ')) else {
            return reply(out, "ERR:SYNTAX\r\n");
        };
        let Some(milliamps) = parse_milliunits(value).filter(|value| *value <= 3_000) else {
            return reply(out, "ERR:RANGE\r\n");
        };
        return Some(UsbIntent::SetCurrentLimit { channel, milliamps });
    }
    if let Some(value) = command.strip_prefix(b"SINK:LIMIT ") {
        let Some(milliamps) = parse_milliunits(value).filter(|value| *value <= 5_000) else {
            return reply(out, "ERR:RANGE\r\n");
        };
        return Some(UsbIntent::SetSinkCurrentLimit(milliamps));
    }
    match command {
        b"*IDN?" => reply(out, "BenchVolt-PD,RUST,S/N:2026-01\r\n"),
        b"SYST:BUILD?" => reply(
            out,
            concat!(
                "BenchVolt-PD v",
                env!("CARGO_PKG_VERSION"),
                " ",
                env!("BENCHVOLT_GIT_REV"),
                "\r\n"
            ),
        ),
        b"SYST:HWERR?" => {
            let mut response = Response::new_empty();
            write!(
                &mut response,
                "OP{} ERR{} RETRIES{}\r\n",
                diagnostics.hw_last_operation,
                diagnostics.hw_last_error,
                diagnostics.hw_retry_count,
            )
            .ok();
            finish(out, &response)
        }
        b"SYST:DISPLAY?" => {
            let mut response = Response::new_empty();
            write!(
                &mut response,
                "{} Q{} H{} A{} O{} F{} SEAL{}\r\n",
                diagnostics.display_label,
                diagnostics.display_queued,
                diagnostics.display_high_water,
                u8::from(diagnostics.display_active),
                u8::from(diagnostics.display_overflowed),
                u8::from(diagnostics.display_failed),
                u8::from(diagnostics.display_ready_for_seal),
            )
            .ok();
            finish(out, &response)
        }
        b"SYST:RESET?" => {
            let causes = diagnostics.reset_causes;
            let mut response = Response::new_empty();
            write!(&mut response, "0x{causes:02X}").ok();
            for (mask, label) in [
                (crate::reset_cause::OPTION_BYTE, "OPTION"),
                (crate::reset_cause::PIN, "PIN"),
                (crate::reset_cause::POWER_ON, "POR"),
                (crate::reset_cause::SOFTWARE, "SOFTWARE"),
                (crate::reset_cause::INDEPENDENT_WATCHDOG, "IWDG"),
                (crate::reset_cause::WINDOW_WATCHDOG, "WWDG"),
                (crate::reset_cause::LOW_POWER, "LOWPOWER"),
                (crate::reset_cause::V18_DOMAIN, "V18"),
            ] {
                if causes & mask != 0 {
                    write!(&mut response, ",{label}").ok();
                }
            }
            if let Some(reason) =
                crate::reset_cause::ResetReason::from_raw(u32::from(diagnostics.reset_reason))
            {
                write!(&mut response, ",CAUSE:{}", reason.label()).ok();
            }
            response.write_str("\r\n").ok();
            finish(out, &response)
        }
        b"SYST:TPS:CH5?" => {
            let mut response = Response::new_empty();
            write!(&mut response, "0x{:02X}\r\n", diagnostics.tps_ch5_status).ok();
            finish(out, &response)
        }
        b"SYST:PD?" => {
            let mut response = Response::new_empty();
            if let Some(contract) = state.pd_contract {
                write!(
                    &mut response,
                    "READY,PDO{},{}mV,{}mA,MAX{}mA\r\n",
                    contract.source_position,
                    contract.millivolts,
                    contract.operating_milliamps,
                    contract.maximum_milliamps,
                )
                .ok();
            } else if let Some(error) = state.pd_error {
                let code = match error {
                    crate::pd::PdError::Bus => "BUS",
                    crate::pd::PdError::WrongDevice => "DEVICE",
                    crate::pd::PdError::Detached => "DETACHED",
                    crate::pd::PdError::Timeout => "TIMEOUT",
                    crate::pd::PdError::MalformedCapabilities => "CAPS",
                    crate::pd::PdError::NoSuitablePdo => "NO_PDO",
                    crate::pd::PdError::ContractMismatch => "CONTRACT",
                };
                write!(&mut response, "ERROR,{code}\r\n").ok();
            } else if state.pd_negotiating {
                response.write_str("NEGOTIATING\r\n").ok();
            } else {
                response.write_str("IDLE\r\n").ok();
            }
            finish(out, &response)
        }
        b"SYST:TICK?" => {
            let mut response = Response::new_empty();
            write!(&mut response, "{}\r\n", diagnostics.tick_ms).ok();
            finish(out, &response)
        }
        b"MEAS:TEMP?" => finish(out, &temperature_response(state)),
        b"MEAS:CH1?" | b"MEAS:CH2?" | b"MEAS:CH3?" | b"MEAS:CH4?" | b"MEAS:CH5?" => {
            let channel = usize::from(command[7] - b'1');
            let measurement = state.channels[channel].measurement;
            let mut response = Response::new_empty();
            if measurement.valid {
                write!(
                    &mut response,
                    "{}.{:03}V,{}.{:03}A\r\n",
                    measurement.millivolts / 1_000,
                    measurement.millivolts % 1_000,
                    measurement.milliamps / 1_000,
                    measurement.milliamps % 1_000
                )
                .ok();
            } else {
                response.write_str("ERR:SENSOR\r\n").ok();
            }
            finish(out, &response)
        }
        b"MEAS:SINK?" => {
            let measurement = state.sink;
            let mut response = Response::new_empty();
            if measurement.valid {
                let milliwatts = u32::from(measurement.millivolts)
                    .saturating_mul(u32::from(measurement.milliamps))
                    / 1_000;
                write!(
                    &mut response,
                    "{}.{:03}V,{}.{:03}A,{}.{:03}W\r\n",
                    measurement.millivolts / 1_000,
                    measurement.millivolts % 1_000,
                    measurement.milliamps / 1_000,
                    measurement.milliamps % 1_000,
                    milliwatts / 1_000,
                    milliwatts % 1_000
                )
                .ok();
            } else {
                response.write_str("ERR:SENSOR\r\n").ok();
            }
            finish(out, &response)
        }
        b"SINK:LIMIT?" => {
            let mut response = Response::new_empty();
            write!(
                &mut response,
                "{}.{:03}A\r\n",
                state.sink_current_limit_ma / 1_000,
                state.sink_current_limit_ma % 1_000
            )
            .ok();
            finish(out, &response)
        }
        b"SOUR:MODE:CH4?" => reply(
            out,
            if state.channels[3].regulation_mode == RegulationMode::Cc {
                "CC\r\n"
            } else {
                "CV\r\n"
            },
        ),
        b"SOUR:MODE:CH5?" => reply(
            out,
            if state.channels[4].regulation_mode == RegulationMode::Cc {
                "CC\r\n"
            } else {
                "CV\r\n"
            },
        ),
        b"SOUR:MODE:CH4 CV" => Some(UsbIntent::SetRegulationMode {
            channel: 3,
            mode: RegulationMode::Cv,
        }),
        b"SOUR:MODE:CH4 CC" => Some(UsbIntent::SetRegulationMode {
            channel: 3,
            mode: RegulationMode::Cc,
        }),
        b"SOUR:MODE:CH5 CV" => Some(UsbIntent::SetRegulationMode {
            channel: 4,
            mode: RegulationMode::Cv,
        }),
        b"SOUR:MODE:CH5 CC" => Some(UsbIntent::SetRegulationMode {
            channel: 4,
            mode: RegulationMode::Cc,
        }),
        b"SYST:UI?" => {
            let mut response = Response::new_empty();
            let focus = match state.focus {
                ControlFocus::None => "NONE",
                ControlFocus::OverviewOutput(_) => "OVOUT",
                ControlFocus::Output => "OUT",
                ControlFocus::Voltage => "VOLT",
                ControlFocus::CurrentLimit => "CURR",
                ControlFocus::RegulationMode => "MODE",
            };
            match state.screen {
                Screen::Channel(channel) => {
                    let output = &state.channels[usize::from(channel)];
                    write!(
                        &mut response,
                        "CH{},{} V:{} I:{} E:{} D:{}\r\n",
                        channel + 1,
                        focus,
                        output.setpoint_mv,
                        output.current_limit_ma,
                        diagnostics.encoder_edges,
                        diagnostics.encoder_drops
                    )
                    .ok();
                }
                Screen::Overview => {
                    write!(
                        &mut response,
                        "OVERVIEW E:{} D:{}\r\n",
                        diagnostics.encoder_edges, diagnostics.encoder_drops
                    )
                    .ok();
                }
                Screen::UsbPdInput => {
                    write!(
                        &mut response,
                        "USBPD,{} I:{} E:{} D:{}\r\n",
                        focus,
                        state.sink_current_limit_ma,
                        diagnostics.encoder_edges,
                        diagnostics.encoder_drops
                    )
                    .ok();
                }
                Screen::MainMenu => {
                    response.write_str("MENU\r\n").ok();
                }
                Screen::Awg => {
                    response.write_str("AWG\r\n").ok();
                }
                Screen::Settings => {
                    response.write_str("SETTINGS\r\n").ok();
                }
                Screen::ProfileSave => {
                    response.write_str("PROFILE:SAVE\r\n").ok();
                }
                Screen::ProfileLoad => {
                    response.write_str("PROFILE:LOAD\r\n").ok();
                }
                Screen::System => {
                    response.write_str("SYSTEM\r\n").ok();
                }
                Screen::Help => {
                    response.write_str("HELP\r\n").ok();
                }
            }
            finish(out, &response)
        }
        b"JUMP:BOOTLOADER" => Some(UsbIntent::JumpToBootloader),
        b"SYST:REBOOT" => Some(UsbIntent::Reboot),
        _ => reply(out, "ERR:UNKNOWN_COMMAND\r\n"),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::app::Measurement;

    fn dispatch(
        command: &[u8],
        state: &AppState,
        diagnostics: &DiagnosticsSnapshot,
    ) -> (Option<UsbIntent>, std::string::String) {
        let monitors = [ProtectionMonitor::default(); 5];
        let mut out = Response::new_empty();
        let intent = dispatch_command(command, state, &monitors, diagnostics, &mut out);
        (
            intent,
            std::string::String::from_utf8(out.as_bytes().to_vec()).unwrap(),
        )
    }

    fn state() -> AppState {
        AppState::new(true, Some(25 * 16))
    }

    #[test]
    fn identification_build_and_unknown_commands_reply_directly() {
        let diagnostics = DiagnosticsSnapshot::default();
        let (intent, reply) = dispatch(b"*IDN?", &state(), &diagnostics);
        assert!(intent.is_none());
        assert_eq!(reply, "BenchVolt-PD,RUST,S/N:2026-01\r\n");

        let (intent, reply) = dispatch(b"BOGUS", &state(), &diagnostics);
        assert!(intent.is_none());
        assert_eq!(reply, "ERR:UNKNOWN_COMMAND\r\n");

        // Trailing carriage returns from CRLF hosts are tolerated.
        let (_, reply) = dispatch(b"SYST:BUILD?\r", &state(), &diagnostics);
        let expected: heapless::String<64> = {
            let mut expected = heapless::String::new();
            expected.push_str("BenchVolt-PD ").unwrap();
            expected.push_str(crate::FIRMWARE_BUILD).unwrap();
            expected.push_str("\r\n").unwrap();
            expected
        };
        assert_eq!(reply, expected.as_str());
    }

    #[test]
    fn output_status_query_reports_fault_kinds_over_enable_state() {
        let diagnostics = DiagnosticsSnapshot::default();
        let mut state = state();
        state.channels[1].physical_enabled = true;
        let (_, reply) = dispatch(b"OUTP:CH2?", &state, &diagnostics);
        assert_eq!(reply, "ON\r\n");

        state.channels[1].fault = crate::app::Fault::OverCurrent;
        let (_, reply) = dispatch(b"OUTP:CH2?", &state, &diagnostics);
        assert_eq!(reply, "FAULT:OVERCURRENT\r\n");

        let (_, reply) = dispatch(b"OUTP:CH9?", &state, &diagnostics);
        assert_eq!(reply, "ERR:RANGE\r\n");
    }

    #[test]
    fn current_limit_get_set_and_range_rejection() {
        let diagnostics = DiagnosticsSnapshot::default();
        let (intent, _) = dispatch(b"SOUR:CURR:CH1 1.5", &state(), &diagnostics);
        assert_eq!(
            intent,
            Some(UsbIntent::SetCurrentLimit {
                channel: 0,
                milliamps: 1_500,
            })
        );

        let (intent, reply) = dispatch(b"SOUR:CURR:CH1 3.5", &state(), &diagnostics);
        assert!(intent.is_none());
        assert_eq!(reply, "ERR:RANGE\r\n");

        let mut state = state();
        state.channels[0].current_limit_ma = 2_050;
        let (_, reply) = dispatch(b"SOUR:CURR:CH1?", &state, &diagnostics);
        assert_eq!(reply, "2.050A\r\n");
    }

    #[test]
    fn measurement_queries_format_values_and_fail_closed_on_invalid() {
        let diagnostics = DiagnosticsSnapshot::default();
        let mut state = state();
        state.channels[2].measurement = Measurement {
            millivolts: 3_300,
            milliamps: 1_250,
            valid: true,
        };
        let (_, reply) = dispatch(b"MEAS:CH3?", &state, &diagnostics);
        assert_eq!(reply, "3.300V,1.250A\r\n");

        state.channels[2].measurement.valid = false;
        let (_, reply) = dispatch(b"MEAS:CH3?", &state, &diagnostics);
        assert_eq!(reply, "ERR:SENSOR\r\n");

        state.sink = Measurement {
            millivolts: 20_000,
            milliamps: 2_500,
            valid: true,
        };
        let (_, reply) = dispatch(b"MEAS:SINK?", &state, &diagnostics);
        assert_eq!(reply, "20.000V,2.500A,50.000W\r\n");
    }

    #[test]
    fn bulk_measurement_query_is_not_truncated() {
        // MEAS:ALL? produces the longest reply in the protocol; it must
        // round-trip through the dispatcher's response buffer intact.
        let diagnostics = DiagnosticsSnapshot::default();
        let mut state = state();
        for channel in &mut state.channels {
            channel.measurement = Measurement {
                millivolts: 12_345,
                milliamps: 2_345,
                valid: true,
            };
        }
        state.sink = state.channels[0].measurement;
        let (intent, reply) = dispatch(b"MEAS:ALL?", &state, &diagnostics);
        assert!(intent.is_none());
        assert!(reply.ends_with("\r\n"));
        // 5 channel pairs + sink pair + temperature + 5 enable flags +
        // 2 ARB flags + 5 limits + 2 setpoints.
        assert_eq!(reply.trim_end().split(',').count(), 27);
    }

    #[test]
    fn regulation_mode_commands_produce_typed_intents_and_queries_reply() {
        let diagnostics = DiagnosticsSnapshot::default();
        let (intent, _) = dispatch(b"SOUR:MODE:CH5 CC", &state(), &diagnostics);
        assert_eq!(
            intent,
            Some(UsbIntent::SetRegulationMode {
                channel: 4,
                mode: RegulationMode::Cc,
            })
        );

        let mut state = state();
        state.channels[3].regulation_mode = RegulationMode::Cc;
        let (_, reply) = dispatch(b"SOUR:MODE:CH4?", &state, &diagnostics);
        assert_eq!(reply, "CC\r\n");
    }

    #[test]
    fn arb_status_reports_ownership_and_scheduler_counters() {
        let mut diagnostics = DiagnosticsSnapshot::default();
        diagnostics.arb_index = 7;
        diagnostics.arb_cycles = 3;
        let mut state = state();
        state.awg_source = AwgSource::Arbitrary;
        state.arb_run.channel = 4;
        state.awg_status = AwgStatus::Running;
        let (_, reply) = dispatch(b"SOUR:WAVE:CH5:ARB:STAT?", &state, &diagnostics);
        assert_eq!(reply, "RUNNING,INDEX:7,CYCLES:3,LATE:0,SKIP:0\r\n");

        // The other channel does not own the run.
        let (_, reply) = dispatch(b"SOUR:WAVE:CH4:ARB:STAT?", &state, &diagnostics);
        assert!(reply.starts_with("STOPPED,"));

        let (intent, _) = dispatch(b"SOUR:WAVE:CH5:ARB:STOP", &state, &diagnostics);
        assert_eq!(intent, Some(UsbIntent::ArbStop(4)));
    }

    #[test]
    fn builtin_awg_commands_produce_typed_intents() {
        let diagnostics = DiagnosticsSnapshot::default();
        let (intent, _) = dispatch(
            b"SOUR:WAVE:CH4:FUNC SIN,60000,50,1000,5000",
            &state(),
            &diagnostics,
        );
        assert_eq!(
            intent,
            Some(UsbIntent::AwgConfigure(AwgConfig {
                channel: 3,
                waveform: AwgWaveform::Sine,
                frequency_millihz: 60_000,
                duty_percent: 50,
                low_mv: 1_000,
                high_mv: 5_000,
            }))
        );

        let (intent, _) = dispatch(b"SOUR:WAVE:CH5:RUN", &state(), &diagnostics);
        assert_eq!(intent, Some(UsbIntent::AwgRun(4)));
        let (intent, _) = dispatch(b"SOUR:WAVE:CH4:STOP", &state(), &diagnostics);
        assert_eq!(intent, Some(UsbIntent::AwgStop(3)));

        // Malformed FUNC arguments are syntax errors, not intents.
        let (intent, reply) = dispatch(
            b"SOUR:WAVE:CH4:FUNC SAW,60000,50,1000,5000",
            &state(),
            &diagnostics,
        );
        assert!(intent.is_none());
        assert_eq!(reply, "ERR:SYNTAX\r\n");
        let (intent, reply) = dispatch(
            b"SOUR:WAVE:CH4:FUNC SIN,60000,50,1000",
            &state(),
            &diagnostics,
        );
        assert!(intent.is_none());
        assert_eq!(reply, "ERR:SYNTAX\r\n");
    }

    #[test]
    fn builtin_awg_status_reports_only_the_owning_channel() {
        let diagnostics = DiagnosticsSnapshot::default();
        let mut state = state();
        state.awg_source = AwgSource::Builtin;
        state.awg.channel = 4;
        state.awg_status = AwgStatus::Running;
        let (_, reply) = dispatch(b"SOUR:WAVE:CH5:STAT?", &state, &diagnostics);
        assert_eq!(reply, "RUNNING\r\n");
        let (_, reply) = dispatch(b"SOUR:WAVE:CH4:STAT?", &state, &diagnostics);
        assert_eq!(reply, "STOPPED\r\n");

        // An arbitrary run does not leak into the builtin status view.
        state.awg_source = AwgSource::Arbitrary;
        state.arb_run.channel = 4;
        let (_, reply) = dispatch(b"SOUR:WAVE:CH5:STAT?", &state, &diagnostics);
        assert_eq!(reply, "STOPPED\r\n");
    }

    #[test]
    fn wave_func_query_reports_the_on_device_configuration() {
        let diagnostics = DiagnosticsSnapshot::default();
        let mut state = state();
        state.awg = AwgConfig {
            channel: 4,
            waveform: AwgWaveform::Triangle,
            frequency_millihz: 2_500,
            duty_percent: 50,
            low_mv: 1_000,
            high_mv: 12_000,
        };
        let (intent, reply) = dispatch(b"SOUR:WAVE:FUNC?", &state, &diagnostics);
        assert!(intent.is_none());
        assert_eq!(reply, "CH5,TRI,2500,50,1000,12000\r\n");
    }

    #[test]
    fn pd_contract_query_reports_negotiated_position_or_none() {
        let diagnostics = DiagnosticsSnapshot::default();
        let mut state = state();
        let (_, reply) = dispatch(b"SYST:PD:CONTRACT?", &state, &diagnostics);
        assert_eq!(reply, "NONE\r\n");

        state.pd_contract = Some(crate::pd::Contract {
            source_position: 4,
            millivolts: 20_000,
            operating_milliamps: 5_000,
            maximum_milliamps: 5_000,
        });
        let (_, reply) = dispatch(b"SYST:PD:CONTRACT?", &state, &diagnostics);
        assert_eq!(reply, "4,20000,5000\r\n");
    }

    #[test]
    fn diagnostic_queries_render_the_injected_snapshot() {
        let mut diagnostics = DiagnosticsSnapshot::default();
        diagnostics.hw_last_operation = 3;
        diagnostics.hw_last_error = 2;
        diagnostics.hw_retry_count = 9;
        diagnostics.tick_ms = 1_234;
        diagnostics.reset_causes = crate::reset_cause::POWER_ON;
        let state = state();

        let (_, reply) = dispatch(b"SYST:HWERR?", &state, &diagnostics);
        assert_eq!(reply, "OP3 ERR2 RETRIES9\r\n");

        let (_, reply) = dispatch(b"SYST:TICK?", &state, &diagnostics);
        assert_eq!(reply, "1234\r\n");

        let (_, reply) = dispatch(b"SYST:RESET?", &state, &diagnostics);
        assert!(reply.starts_with("0x"));
        assert!(reply.contains("POR"));
    }

    #[test]
    fn ui_query_describes_screen_focus_and_encoder_health() {
        let mut diagnostics = DiagnosticsSnapshot::default();
        diagnostics.encoder_edges = 42;
        diagnostics.encoder_drops = 1;
        let mut state = state();
        state.screen = Screen::Channel(3);
        state.focus = ControlFocus::Voltage;
        let (_, reply) = dispatch(b"SYST:UI?", &state, &diagnostics);
        assert!(reply.starts_with("CH4,VOLT "));
        assert!(reply.contains("E:42"));
        assert!(reply.contains("D:1"));

        state.screen = Screen::Help;
        let (_, reply) = dispatch(b"SYST:UI?", &state, &diagnostics);
        assert_eq!(reply, "HELP\r\n");
    }
}
