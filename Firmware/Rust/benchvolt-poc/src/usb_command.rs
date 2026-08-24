use crate::{
    app::{AppState, AwgSource, AwgStatus, Fault, Measurement, RegulationMode},
    arb::{DataChunk as ArbDataChunk, Start as ArbStart},
    limits::{CH5_MAX_VOLTAGE_MV, CH5_MIN_VOLTAGE_MV},
    protocol::parse_milliunits,
};

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

    fn push_bytes(&mut self, value: &[u8]) -> Result<(), CommandError> {
        let start = usize::from(self.len);
        let end = start.checked_add(value.len()).ok_or(CommandError::Range)?;
        let target = self.bytes.get_mut(start..end).ok_or(CommandError::Range)?;
        target.copy_from_slice(value);
        self.len = end as u16;
        Ok(())
    }

    fn push_byte(&mut self, value: u8) -> Result<(), CommandError> {
        self.push_bytes(&[value])
    }

    fn push_unsigned(&mut self, mut value: u32) -> Result<(), CommandError> {
        let mut digits = [0u8; 10];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                return self.push_bytes(&digits[start..]);
            }
        }
    }

    fn push_fixed(&mut self, scaled: u32, decimal_digits: u8) -> Result<(), CommandError> {
        let divisor = match decimal_digits {
            1 => 10,
            2 => 100,
            _ => 1_000,
        };
        self.push_unsigned(scaled / divisor)?;
        self.push_byte(b'.')?;
        let fraction = scaled % divisor;
        if decimal_digits >= 2 && fraction < divisor / 10 {
            self.push_byte(b'0')?;
        }
        if decimal_digits == 3 && fraction < 10 {
            self.push_byte(b'0')?;
        }
        self.push_unsigned(fraction)
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

pub fn output_completion_response(result: Result<(), Fault>) -> &'static [u8] {
    match result {
        Ok(()) => b"OK\r\n",
        Err(Fault::OverCurrent) => b"ERR:OVERCURRENT\r\n",
        Err(Fault::OverTemperature) => b"ERR:OVERTEMP\r\n",
        Err(Fault::Sensor) => b"ERR:SENSOR\r\n",
        Err(_) => b"ERR:HARDWARE\r\n",
    }
}

pub fn temperature_response(state: &AppState) -> Response {
    let mut response = Response::new();
    if state.temp_valid {
        let hundredths = i32::from(state.temp_sixteenths_c) * 100 / 16;
        if hundredths < 0 {
            response.push_byte(b'-').ok();
        }
        response.push_fixed(hundredths.unsigned_abs(), 2).ok();
        response.push_bytes(b"\r\n").ok();
    } else {
        response.push_bytes(b"ERR:SENSOR\r\n").ok();
    }
    response
}

fn write_voltage(response: &mut Response, measurement: Measurement) -> Result<(), CommandError> {
    if measurement.valid {
        let centivolts = (u32::from(measurement.millivolts) + 5) / 10;
        response.push_fixed(centivolts, 2)
    } else {
        response.push_bytes(b"nan")
    }
}

fn write_current(response: &mut Response, measurement: Measurement) -> Result<(), CommandError> {
    if measurement.valid {
        response.push_fixed(u32::from(measurement.milliamps), 3)
    } else {
        response.push_bytes(b"nan")
    }
}

fn write_temperature(response: &mut Response, state: &AppState) -> Result<(), CommandError> {
    if state.temp_valid {
        let tenths = i32::from(state.temp_sixteenths_c) * 10 / 16;
        if tenths < 0 {
            response.push_byte(b'-')?;
        }
        response.push_fixed(tenths.unsigned_abs(), 1)
    } else {
        response.push_bytes(b"nan")
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
        )?;
        response.push_bytes(b"\r\n")?;
        return Ok(Some(response));
    }

    if command != b"MEAS:ALL?" {
        return Ok(None);
    }

    let mut response = Response::new();
    for channel in &state.channels {
        write_voltage(&mut response, channel.measurement)?;
        response.push_byte(b',')?;
        write_current(&mut response, channel.measurement)?;
        response.push_byte(b',')?;
    }
    write_voltage(&mut response, state.sink)?;
    response.push_byte(b',')?;
    write_current(&mut response, state.sink)?;
    response.push_byte(b',')?;
    write_temperature(&mut response, state)?;

    for channel in &state.channels {
        response.push_byte(b',')?;
        response.push_byte(b'0' + u8::from(channel.physical_enabled))?;
    }
    response.push_byte(b',')?;
    response.push_byte(b'0' + u8::from(arbitrary_channel_active(state, 3)))?;
    response.push_byte(b',')?;
    response.push_byte(b'0' + u8::from(arbitrary_channel_active(state, 4)))?;
    for channel in &state.channels {
        let centiamps = (u32::from(channel.current_limit_ma) + 5) / 10;
        response.push_byte(b',')?;
        response.push_fixed(centiamps, 2)?;
    }
    let ch4_centivolts = (u32::from(state.channels[3].setpoint_mv) + 5) / 10;
    let ch5_centivolts = (u32::from(state.channels[4].setpoint_mv) + 5) / 10;
    response.push_byte(b',')?;
    response.push_fixed(ch4_centivolts, 2)?;
    response.push_byte(b',')?;
    response.push_fixed(ch5_centivolts, 2)?;
    response.push_bytes(b"\r\n")?;
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

    #[test]
    fn output_completion_responses_preserve_fault_specific_cli_errors() {
        assert_eq!(output_completion_response(Ok(())), b"OK\r\n");
        assert_eq!(
            output_completion_response(Err(Fault::OverCurrent)),
            b"ERR:OVERCURRENT\r\n"
        );
        assert_eq!(
            output_completion_response(Err(Fault::OverTemperature)),
            b"ERR:OVERTEMP\r\n"
        );
        assert_eq!(
            output_completion_response(Err(Fault::Sensor)),
            b"ERR:SENSOR\r\n"
        );
        assert_eq!(
            output_completion_response(Err(Fault::Hardware)),
            b"ERR:HARDWARE\r\n"
        );
        assert_eq!(
            output_completion_response(Err(Fault::None)),
            b"ERR:HARDWARE\r\n"
        );
    }

    #[test]
    fn native_temperature_response_preserves_a_negative_sub_degree_sign() {
        let negative = AppState::new(true, Some(-1));
        let invalid = AppState::new(true, None);

        assert_eq!(temperature_response(&negative).as_bytes(), b"-0.06\r\n");
        assert_eq!(temperature_response(&invalid).as_bytes(), b"ERR:SENSOR\r\n");
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
