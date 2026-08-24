use crate::{
    app::{AppState, AwgSource, AwgStatus, Measurement, RegulationMode},
    arb::{DataChunk as ArbDataChunk, Start as ArbStart},
    limits::{CH5_MAX_VOLTAGE_MV, CH5_MIN_VOLTAGE_MV},
    protocol::parse_milliunits,
};
use core::fmt::{self, Write as _};

pub const RESPONSE_CAPACITY: usize = 192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Response {
    bytes: [u8; RESPONSE_CAPACITY],
    len: u16,
}

impl Response {
    const fn new() -> Self {
        Self {
            bytes: [0; RESPONSE_CAPACITY],
            len: 0,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl fmt::Write for Response {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let start = usize::from(self.len);
        let end = start.checked_add(value.len()).ok_or(fmt::Error)?;
        let target = self.bytes.get_mut(start..end).ok_or(fmt::Error)?;
        target.copy_from_slice(value.as_bytes());
        self.len = end as u16;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsbIntent {
    None,
    JumpToBootloader,
    Reboot,
    SetOutput { channel: u8, enabled: bool },
    SetVoltage { channel: u8, millivolts: u16 },
    SetCurrentLimit { channel: u8, milliamps: u16 },
    SetRegulationMode { channel: u8, mode: RegulationMode },
    SetSinkCurrentLimit(u16),
    ArbData(ArbDataChunk),
    ArbStart(ArbStart),
    ArbStop(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandError {
    Syntax,
    Range,
}

fn write_voltage(response: &mut Response, measurement: Measurement) -> fmt::Result {
    if measurement.valid {
        let centivolts = (u32::from(measurement.millivolts) + 5) / 10;
        write!(response, "{}.{:02}", centivolts / 100, centivolts % 100)
    } else {
        response.write_str("nan")
    }
}

fn write_current(response: &mut Response, measurement: Measurement) -> fmt::Result {
    if measurement.valid {
        write!(
            response,
            "{}.{:03}",
            measurement.milliamps / 1_000,
            measurement.milliamps % 1_000
        )
    } else {
        response.write_str("nan")
    }
}

fn write_temperature(response: &mut Response, state: &AppState) -> fmt::Result {
    if state.temp_valid {
        let tenths = i32::from(state.temp_sixteenths_c) * 10 / 16;
        if tenths < 0 {
            write!(response, "-{}.{:01}", tenths.abs() / 10, tenths.abs() % 10)
        } else {
            write!(response, "{}.{:01}", tenths / 10, tenths % 10)
        }
    } else {
        response.write_str("nan")
    }
}

fn arbitrary_channel_active(state: &AppState, channel: u8) -> bool {
    state.awg_source == AwgSource::Arbitrary
        && state.active_awg_channel() == channel
        && !matches!(state.awg_status, AwgStatus::Stopped | AwgStatus::Fault)
}

pub fn project_compat_query(
    command: &[u8],
    state: &AppState,
) -> Result<Option<Response>, CommandError> {
    if let Some(rest) = command.strip_prefix(b"MEAS:VOLT:CH") {
        let channel = rest
            .first()
            .and_then(|byte| byte.checked_sub(b'1'))
            .filter(|channel| *channel < 5 && rest.get(1..) == Some(b"?"))
            .ok_or(CommandError::Range)?;
        let mut response = Response::new();
        write_voltage(
            &mut response,
            state.channels[usize::from(channel)].measurement,
        )
        .map_err(|_| CommandError::Range)?;
        response
            .write_str("\r\n")
            .map_err(|_| CommandError::Range)?;
        return Ok(Some(response));
    }

    if command != b"MEAS:ALL?" {
        return Ok(None);
    }

    let mut response = Response::new();
    for channel in &state.channels {
        write_voltage(&mut response, channel.measurement).map_err(|_| CommandError::Range)?;
        response.write_char(',').map_err(|_| CommandError::Range)?;
        write_current(&mut response, channel.measurement).map_err(|_| CommandError::Range)?;
        response.write_char(',').map_err(|_| CommandError::Range)?;
    }
    write_voltage(&mut response, state.sink).map_err(|_| CommandError::Range)?;
    response.write_char(',').map_err(|_| CommandError::Range)?;
    write_current(&mut response, state.sink).map_err(|_| CommandError::Range)?;
    response.write_char(',').map_err(|_| CommandError::Range)?;
    write_temperature(&mut response, state).map_err(|_| CommandError::Range)?;

    for channel in &state.channels {
        write!(response, ",{}", u8::from(channel.physical_enabled))
            .map_err(|_| CommandError::Range)?;
    }
    write!(
        response,
        ",{},{}",
        u8::from(arbitrary_channel_active(state, 3)),
        u8::from(arbitrary_channel_active(state, 4))
    )
    .map_err(|_| CommandError::Range)?;
    for channel in &state.channels {
        let centiamps = (u32::from(channel.current_limit_ma) + 5) / 10;
        write!(response, ",{}.{:02}", centiamps / 100, centiamps % 100)
            .map_err(|_| CommandError::Range)?;
    }
    let ch4_centivolts = (u32::from(state.channels[3].setpoint_mv) + 5) / 10;
    let ch5_centivolts = (u32::from(state.channels[4].setpoint_mv) + 5) / 10;
    write!(
        response,
        ",{}.{:02},{}.{:02}\r\n",
        ch4_centivolts / 100,
        ch4_centivolts % 100,
        ch5_centivolts / 100,
        ch5_centivolts % 100,
    )
    .map_err(|_| CommandError::Range)?;
    Ok(Some(response))
}

pub fn parse_compat_mutation(command: &[u8]) -> Result<Option<UsbIntent>, CommandError> {
    if let Some(rest) = command.strip_prefix(b"OUTP:CH") {
        if rest.len() == 2 && rest[1] == b'?' {
            return Ok(None);
        }
        let channel = rest
            .first()
            .and_then(|byte| byte.checked_sub(b'1'))
            .filter(|channel| *channel < 5)
            .ok_or(CommandError::Range)?;
        let operation = rest.get(1..).ok_or(CommandError::Syntax)?;
        let enabled = match operation {
            b" ON" | b":STAT 1" | b" STAT 1" => true,
            b" OFF" | b":STAT 0" | b" STAT 0" => false,
            _ => return Err(CommandError::Syntax),
        };
        return Ok(Some(UsbIntent::SetOutput { channel, enabled }));
    }

    if let Some(rest) = command.strip_prefix(b"SOUR:VOLT:CH") {
        let channel = rest
            .first()
            .and_then(|byte| byte.checked_sub(b'1'))
            .filter(|channel| matches!(*channel, 3 | 4))
            .ok_or(CommandError::Range)?;
        let value = rest
            .get(2..)
            .filter(|_| rest.get(1) == Some(&b' '))
            .ok_or(CommandError::Syntax)?;
        let millivolts = parse_milliunits(value).ok_or(CommandError::Syntax)?;
        let range = if channel == 3 {
            500..=5_000
        } else {
            CH5_MIN_VOLTAGE_MV..=CH5_MAX_VOLTAGE_MV
        };
        if !range.contains(&millivolts) {
            return Err(CommandError::Range);
        }
        return Ok(Some(UsbIntent::SetVoltage {
            channel,
            millivolts,
        }));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_parser_accepts_current_gui_and_documented_legacy_forms() {
        for (command, channel, enabled) in [
            (b"OUTP:CH1 ON" as &[u8], 0, true),
            (b"OUTP:CH5 OFF", 4, false),
            (b"OUTP:CH2:STAT 1", 1, true),
            (b"OUTP:CH3:STAT 0", 2, false),
            (b"OUTP:CH4 STAT 1", 3, true),
        ] {
            assert_eq!(
                parse_compat_mutation(command),
                Ok(Some(UsbIntent::SetOutput { channel, enabled }))
            );
        }
    }

    #[test]
    fn voltage_parser_uses_exact_millivolts_and_board_ranges() {
        assert_eq!(
            parse_compat_mutation(b"SOUR:VOLT:CH4 1.25"),
            Ok(Some(UsbIntent::SetVoltage {
                channel: 3,
                millivolts: 1_250,
            }))
        );
        assert_eq!(
            parse_compat_mutation(b"SOUR:VOLT:CH5 22.00"),
            Ok(Some(UsbIntent::SetVoltage {
                channel: 4,
                millivolts: 22_000,
            }))
        );
        assert_eq!(
            parse_compat_mutation(b"SOUR:VOLT:CH5 22.01"),
            Err(CommandError::Range)
        );
        assert_eq!(
            parse_compat_mutation(b"SOUR:VOLT:CH4 garbage"),
            Err(CommandError::Syntax)
        );
    }

    #[test]
    fn compatibility_parser_leaves_queries_and_unrelated_commands_untouched() {
        assert_eq!(parse_compat_mutation(b"OUTP:CH1?"), Ok(None));
        assert_eq!(parse_compat_mutation(b"MEAS:ALL?"), Ok(None));
    }

    const fn measurement(millivolts: u16, milliamps: u16) -> Measurement {
        Measurement {
            millivolts,
            milliamps,
            valid: true,
        }
    }

    #[test]
    fn legacy_voltage_query_reports_two_decimal_volts() {
        let mut state = AppState::new(true, Some(0));
        state.channels[4].measurement = measurement(12_349, 500);

        let response = project_compat_query(b"MEAS:VOLT:CH5?", &state)
            .unwrap()
            .unwrap();

        assert_eq!(response.as_bytes(), b"12.35\r\n");
        assert_eq!(
            project_compat_query(b"MEAS:VOLT:CH6?", &state),
            Err(CommandError::Range)
        );
    }

    #[test]
    fn legacy_bulk_query_matches_the_gui_27_field_contract() {
        let mut state = AppState::new(true, Some(25 * 16));
        for (channel, sample) in state.channels.iter_mut().zip([
            measurement(1_800, 101),
            measurement(2_500, 202),
            measurement(3_300, 303),
            measurement(4_400, 404),
            measurement(12_340, 505),
        ]) {
            channel.measurement = sample;
        }
        state.sink = measurement(19_990, 1_234);
        for (channel, enabled) in state
            .channels
            .iter_mut()
            .zip([true, false, true, false, true])
        {
            channel.physical_enabled = enabled;
        }
        for (channel, limit) in state
            .channels
            .iter_mut()
            .zip([3_000, 2_500, 2_000, 1_500, 1_000])
        {
            channel.current_limit_ma = limit;
        }
        state.channels[3].setpoint_mv = 4_500;
        state.channels[4].setpoint_mv = 12_000;
        state.awg_source = AwgSource::Arbitrary;
        state.arb_run.channel = 4;
        state.awg_status = AwgStatus::Running;

        let response = project_compat_query(b"MEAS:ALL?", &state).unwrap().unwrap();

        assert_eq!(
            response.as_bytes(),
            b"1.80,0.101,2.50,0.202,3.30,0.303,4.40,0.404,12.34,0.505,19.99,1.234,25.0,1,0,1,0,1,0,1,3.00,2.50,2.00,1.50,1.00,4.50,12.00\r\n"
        );
        assert_eq!(response.as_bytes().split(|byte| *byte == b',').count(), 27);
        assert!(response.as_bytes().len() <= RESPONSE_CAPACITY);
    }

    #[test]
    fn compatibility_queries_never_present_invalid_measurements_as_zero() {
        let state = AppState::new(true, None);
        let voltage = project_compat_query(b"MEAS:VOLT:CH1?", &state)
            .unwrap()
            .unwrap();
        let bulk = project_compat_query(b"MEAS:ALL?", &state).unwrap().unwrap();

        assert_eq!(voltage.as_bytes(), b"nan\r\n");
        assert!(bulk.as_bytes().starts_with(b"nan,nan"));
        assert!(bulk.as_bytes().windows(5).any(|value| value == b",nan,"));
    }

    #[test]
    fn compatibility_projection_handles_negative_temperature_and_maximum_state() {
        let mut state = AppState::new(true, Some(-8));
        state.sink = measurement(u16::MAX, u16::MAX);
        for channel in &mut state.channels {
            channel.measurement = measurement(u16::MAX, u16::MAX);
            channel.current_limit_ma = u16::MAX;
            channel.setpoint_mv = u16::MAX;
        }

        let response = project_compat_query(b"MEAS:ALL?", &state).unwrap().unwrap();

        assert!(response
            .as_bytes()
            .windows(5)
            .any(|value| value == b",-0.5"));
        assert!(response.as_bytes().len() <= RESPONSE_CAPACITY);
        assert!(response.as_bytes().ends_with(b"65.54,65.54\r\n"));
    }
}
