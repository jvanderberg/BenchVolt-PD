use crate::{
    app::RegulationMode,
    arb::{DataChunk as ArbDataChunk, Start as ArbStart},
    limits::{CH5_MAX_VOLTAGE_MV, CH5_MIN_VOLTAGE_MV},
    protocol::parse_milliunits,
};

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
}
