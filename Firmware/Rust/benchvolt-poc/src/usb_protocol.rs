use crate::{
    encoder_counts, monotonic_ms, ARB_CYCLES, ARB_INDEX, ARB_LATE_UPDATES,
    ARB_SKIPPED_CYCLES, CH5_TPS_STATUS, HW_RETRY_COUNT, LAST_HW_ERROR, LAST_HW_OPERATION,
};
use crate::usb_transport::queue_usb_response;
use benchvolt_poc::{
    app::{AppState, AwgSource, AwgStatus, RegulationMode},
    arb::{
        parse_data as parse_arb_data, parse_start as parse_arb_start, DataChunk as ArbDataChunk,
        ParseError as ArbParseError, Start as ArbStart,
    },
    power::ProtectionMonitor,
    protocol::parse_milliunits,
};
use core::{fmt::Write as _, sync::atomic::Ordering};
use heapless::String;

pub(crate) enum UsbIntent {
    None,
    JumpToBootloader,
    Reboot,
    SetOutput { channel: u8, enabled: bool },
    SetCurrentLimit { channel: u8, milliamps: u16 },
    SetRegulationMode { channel: u8, mode: RegulationMode },
    SetSinkCurrentLimit(u16),
    ArbData(ArbDataChunk),
    ArbStart(ArbStart),
    ArbStop(u8),
}

pub(crate) fn handle_usb_command(
    command: &[u8],
    state: &AppState,
    protection_monitors: &[ProtectionMonitor; 5],
) -> UsbIntent {
    let command = command.strip_suffix(b"\r").unwrap_or(command);
    for channel in 4..=5u8 {
        let mut status_command: String<40> = String::new();
        write!(&mut status_command, "SOUR:WAVE:CH{channel}:ARB:STAT?").ok();
        if command == status_command.as_bytes() {
            let owner = state.awg_source == AwgSource::Arbitrary
                && state.active_awg_channel() + 1 == channel;
            let status = if owner {
                match state.awg_status {
                    AwgStatus::Running => "RUNNING",
                    AwgStatus::StartRequested | AwgStatus::Starting => "STARTING",
                    AwgStatus::StopRequested => "STOPPING",
                    AwgStatus::Fault => "FAULT",
                    AwgStatus::Stopped => "STOPPED",
                }
            } else {
                "STOPPED"
            };
            let mut response: String<96> = String::new();
            write!(
                &mut response,
                "{},INDEX:{},CYCLES:{},LATE:{},SKIP:{}\r\n",
                status,
                ARB_INDEX.load(Ordering::Relaxed),
                ARB_CYCLES.load(Ordering::Relaxed),
                ARB_LATE_UPDATES.load(Ordering::Relaxed),
                ARB_SKIPPED_CYCLES.load(Ordering::Relaxed),
            )
            .ok();
            queue_usb_response(response.as_bytes());
            return UsbIntent::None;
        }
        let mut stop_command: String<40> = String::new();
        write!(&mut stop_command, "SOUR:WAVE:CH{channel}:ARB:STOP").ok();
        if command == stop_command.as_bytes() {
            return UsbIntent::ArbStop(channel - 1);
        }
    }
    match parse_arb_data(command) {
        Ok(Some(chunk)) => return UsbIntent::ArbData(chunk),
        Err(ArbParseError::Syntax) => {
            queue_usb_response(b"ERR:SYNTAX\r\n");
            return UsbIntent::None;
        }
        Err(ArbParseError::Range) => {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        }
        Ok(None) => {}
    }
    match parse_arb_start(command) {
        Ok(Some(start)) => return UsbIntent::ArbStart(start),
        Err(ArbParseError::Syntax) => {
            queue_usb_response(b"ERR:SYNTAX\r\n");
            return UsbIntent::None;
        }
        Err(ArbParseError::Range) => {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        }
        Ok(None) => {}
    }
    if let Some(rest) = command.strip_prefix(b"SYST:PROT:CH") {
        let Some(channel) = rest
            .first()
            .and_then(|byte| byte.checked_sub(b'1'))
            .filter(|channel| *channel < 5 && rest.get(1..) == Some(b"?"))
        else {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        };
        let snapshot = protection_monitors[usize::from(channel)].snapshot();
        let mut response: String<64> = String::new();
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
        queue_usb_response(response.as_bytes());
        return UsbIntent::None;
    }
    if let Some(rest) = command.strip_prefix(b"OUTP:CH") {
        if rest.len() == 2 && rest[1] == b'?' {
            let Some(channel) = rest[0].checked_sub(b'1').filter(|channel| *channel < 5) else {
                queue_usb_response(b"ERR:RANGE\r\n");
                return UsbIntent::None;
            };
            let output = &state.channels[usize::from(channel)];
            let status = match output.fault {
                benchvolt_poc::app::Fault::OverCurrent => "FAULT:OVERCURRENT",
                benchvolt_poc::app::Fault::OverTemperature => "FAULT:OVERTEMP",
                benchvolt_poc::app::Fault::Sensor => "FAULT:SENSOR",
                benchvolt_poc::app::Fault::Hardware => "FAULT:HARDWARE",
                benchvolt_poc::app::Fault::None if output.physical_enabled => "ON",
                benchvolt_poc::app::Fault::None => "OFF",
            };
            let mut response: String<32> = String::new();
            write!(&mut response, "{}\r\n", status).ok();
            queue_usb_response(response.as_bytes());
            return UsbIntent::None;
        }
    }
    if let Some(rest) = command.strip_prefix(b"SOUR:CURR:CH") {
        let Some(channel) = rest.first().and_then(|byte| byte.checked_sub(b'1')) else {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        };
        if channel >= 5 {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        }
        if rest.get(1..) == Some(b"?") {
            let limit = state.channels[usize::from(channel)].current_limit_ma;
            let mut response: String<20> = String::new();
            write!(&mut response, "{}.{:03}A\r\n", limit / 1_000, limit % 1_000).ok();
            queue_usb_response(response.as_bytes());
            return UsbIntent::None;
        }
        let Some(value) = rest.get(2..).filter(|_| rest.get(1) == Some(&b' ')) else {
            queue_usb_response(b"ERR:SYNTAX\r\n");
            return UsbIntent::None;
        };
        let Some(milliamps) = parse_milliunits(value).filter(|value| *value <= 3_000) else {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        };
        return UsbIntent::SetCurrentLimit { channel, milliamps };
    }
    if let Some(value) = command.strip_prefix(b"SINK:LIMIT ") {
        let Some(milliamps) = parse_milliunits(value).filter(|value| *value <= 5_000) else {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        };
        return UsbIntent::SetSinkCurrentLimit(milliamps);
    }
    match command {
        b"*IDN?" => queue_usb_response(b"BenchVolt-PD,RUST-POC,S/N:2026-01\r\n"),
        b"SYST:BUILD?" => queue_usb_response(b"Rust POC 0.1.0\r\n"),
        b"SYST:HWERR?" => {
            let mut response: String<48> = String::new();
            write!(
                &mut response,
                "OP{} ERR{} RETRIES{}\r\n",
                LAST_HW_OPERATION.load(Ordering::Relaxed),
                LAST_HW_ERROR.load(Ordering::Relaxed),
                HW_RETRY_COUNT.load(Ordering::Relaxed),
            )
            .ok();
            queue_usb_response(response.as_bytes());
        }
        b"SYST:TPS:CH5?" => {
            let mut response: String<16> = String::new();
            write!(
                &mut response,
                "0x{:02X}\r\n",
                CH5_TPS_STATUS.load(Ordering::Relaxed)
            )
            .ok();
            queue_usb_response(response.as_bytes());
        }
        b"SYST:PD?" => {
            let mut response: String<64> = String::new();
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
                    benchvolt_poc::pd::PdError::Bus => "BUS",
                    benchvolt_poc::pd::PdError::WrongDevice => "DEVICE",
                    benchvolt_poc::pd::PdError::Detached => "DETACHED",
                    benchvolt_poc::pd::PdError::Timeout => "TIMEOUT",
                    benchvolt_poc::pd::PdError::MalformedCapabilities => "CAPS",
                    benchvolt_poc::pd::PdError::NoSuitablePdo => "NO_PDO",
                    benchvolt_poc::pd::PdError::ContractMismatch => "CONTRACT",
                };
                write!(&mut response, "ERROR,{code}\r\n").ok();
            } else {
                response.push_str("NEGOTIATING\r\n").ok();
            }
            queue_usb_response(response.as_bytes());
        }
        b"SYST:TICK?" => {
            let mut response: String<16> = String::new();
            write!(&mut response, "{}\r\n", monotonic_ms()).ok();
            queue_usb_response(response.as_bytes());
        }
        b"MEAS:TEMP?" => {
            let mut response: String<32> = String::new();
            if state.temp_valid {
                let raw = i32::from(state.temp_sixteenths_c);
                let hundredths = raw * 100 / 16;
                write!(
                    &mut response,
                    "{}.{:02}\r\n",
                    hundredths / 100,
                    hundredths.abs() % 100
                )
                .ok();
            } else {
                response.push_str("ERR:SENSOR\r\n").ok();
            }
            queue_usb_response(response.as_bytes());
        }
        b"MEAS:CH1?" | b"MEAS:CH2?" | b"MEAS:CH3?" | b"MEAS:CH4?" | b"MEAS:CH5?" => {
            let channel = usize::from(command[7] - b'1');
            let measurement = state.channels[channel].measurement;
            let mut response: String<40> = String::new();
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
                response.push_str("ERR:SENSOR\r\n").ok();
            }
            queue_usb_response(response.as_bytes());
        }
        b"MEAS:SINK?" => {
            let measurement = state.sink;
            let mut response: String<48> = String::new();
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
                response.push_str("ERR:SENSOR\r\n").ok();
            }
            queue_usb_response(response.as_bytes());
        }
        b"SINK:LIMIT?" => {
            let mut response: String<20> = String::new();
            write!(
                &mut response,
                "{}.{:03}A\r\n",
                state.sink_current_limit_ma / 1_000,
                state.sink_current_limit_ma % 1_000
            )
            .ok();
            queue_usb_response(response.as_bytes());
        }
        b"SOUR:MODE:CH4?" => {
            queue_usb_response(if state.channels[3].regulation_mode == RegulationMode::Cc {
                b"CC\r\n"
            } else {
                b"CV\r\n"
            })
        }
        b"SOUR:MODE:CH5?" => {
            queue_usb_response(if state.channels[4].regulation_mode == RegulationMode::Cc {
                b"CC\r\n"
            } else {
                b"CV\r\n"
            })
        }
        b"SOUR:MODE:CH4 CV" => {
            return UsbIntent::SetRegulationMode {
                channel: 3,
                mode: RegulationMode::Cv,
            }
        }
        b"SOUR:MODE:CH4 CC" => {
            return UsbIntent::SetRegulationMode {
                channel: 3,
                mode: RegulationMode::Cc,
            }
        }
        b"SOUR:MODE:CH5 CV" => {
            return UsbIntent::SetRegulationMode {
                channel: 4,
                mode: RegulationMode::Cv,
            }
        }
        b"SOUR:MODE:CH5 CC" => {
            return UsbIntent::SetRegulationMode {
                channel: 4,
                mode: RegulationMode::Cc,
            }
        }
        b"SYST:UI?" => {
            let (edges, drops) = encoder_counts();
            let mut response: String<64> = String::new();
            let focus = match state.focus {
                benchvolt_poc::app::ControlFocus::None => "NONE",
                benchvolt_poc::app::ControlFocus::OverviewOutput(_) => "OVOUT",
                benchvolt_poc::app::ControlFocus::Output => "OUT",
                benchvolt_poc::app::ControlFocus::Voltage => "VOLT",
                benchvolt_poc::app::ControlFocus::CurrentLimit => "CURR",
                benchvolt_poc::app::ControlFocus::RegulationMode => "MODE",
            };
            match state.screen {
                benchvolt_poc::app::Screen::Channel(channel) => {
                    let output = &state.channels[usize::from(channel)];
                    write!(
                        &mut response,
                        "CH{},{} V:{} I:{} E:{} D:{}\r\n",
                        channel + 1,
                        focus,
                        output.setpoint_mv,
                        output.current_limit_ma,
                        edges,
                        drops
                    )
                    .ok();
                }
                benchvolt_poc::app::Screen::Overview => {
                    write!(&mut response, "OVERVIEW E:{} D:{}\r\n", edges, drops).ok();
                }
                benchvolt_poc::app::Screen::UsbPdInput => {
                    write!(
                        &mut response,
                        "USBPD,{} I:{} E:{} D:{}\r\n",
                        focus, state.sink_current_limit_ma, edges, drops
                    )
                    .ok();
                }
                benchvolt_poc::app::Screen::MainMenu => {
                    response.push_str("MENU\r\n").ok();
                }
                benchvolt_poc::app::Screen::Awg => {
                    response.push_str("AWG\r\n").ok();
                }
                benchvolt_poc::app::Screen::Settings => {
                    response.push_str("SETTINGS\r\n").ok();
                }
                benchvolt_poc::app::Screen::ProfileSave => {
                    response.push_str("PROFILE:SAVE\r\n").ok();
                }
                benchvolt_poc::app::Screen::ProfileLoad => {
                    response.push_str("PROFILE:LOAD\r\n").ok();
                }
                benchvolt_poc::app::Screen::System => {
                    response.push_str("SYSTEM\r\n").ok();
                }
                benchvolt_poc::app::Screen::Help => {
                    response.push_str("HELP\r\n").ok();
                }
            }
            queue_usb_response(response.as_bytes());
        }
        b"OUTP:CH1 ON" => {
            return UsbIntent::SetOutput {
                channel: 0,
                enabled: true,
            }
        }
        b"OUTP:CH1 OFF" => {
            return UsbIntent::SetOutput {
                channel: 0,
                enabled: false,
            }
        }
        b"OUTP:CH2 ON" => {
            return UsbIntent::SetOutput {
                channel: 1,
                enabled: true,
            }
        }
        b"OUTP:CH2 OFF" => {
            return UsbIntent::SetOutput {
                channel: 1,
                enabled: false,
            }
        }
        b"OUTP:CH3 ON" => {
            return UsbIntent::SetOutput {
                channel: 2,
                enabled: true,
            }
        }
        b"OUTP:CH3 OFF" => {
            return UsbIntent::SetOutput {
                channel: 2,
                enabled: false,
            }
        }
        b"OUTP:CH4 ON" => {
            return UsbIntent::SetOutput {
                channel: 3,
                enabled: true,
            }
        }
        b"OUTP:CH4 OFF" => {
            return UsbIntent::SetOutput {
                channel: 3,
                enabled: false,
            }
        }
        b"OUTP:CH5 ON" => {
            return UsbIntent::SetOutput {
                channel: 4,
                enabled: true,
            }
        }
        b"OUTP:CH5 OFF" => {
            return UsbIntent::SetOutput {
                channel: 4,
                enabled: false,
            }
        }
        b"JUMP:BOOTLOADER" => return UsbIntent::JumpToBootloader,
        b"SYST:REBOOT" => return UsbIntent::Reboot,
        _ => queue_usb_response(b"ERR:UNKNOWN_COMMAND\r\n"),
    }
    UsbIntent::None
}
