use crate::app::{AppState, AwgConfig, AwgSource, AwgWaveform, RegulationMode, TemperatureUnit};
use crate::limits::{CH5_MAX_VOLTAGE_MV, CH5_MIN_VOLTAGE_MV};

pub const RECORD_SIZE: usize = 48;
const MAGIC: u32 = 0x4256_5333;
const COMMIT: u32 = 0x434f_4d54;

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct PersistentSettings {
    pub current_limits_ma: [u16; 5],
    pub ch4_voltage_mv: u16,
    pub ch5_voltage_mv: u16,
    pub ch4_regulation_mode: RegulationMode,
    pub ch5_regulation_mode: RegulationMode,
    pub sink_current_limit_ma: u16,
    pub temperature_unit: TemperatureUnit,
    pub awg: AwgConfig,
}

impl PersistentSettings {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            current_limits_ma: core::array::from_fn(|index| state.channels[index].current_limit_ma),
            ch4_voltage_mv: state.channels[3].setpoint_mv,
            ch5_voltage_mv: state.channels[4].setpoint_mv,
            ch4_regulation_mode: state.channels[3].regulation_mode,
            ch5_regulation_mode: state.channels[4].regulation_mode,
            sink_current_limit_ma: state.sink_current_limit_ma,
            temperature_unit: state.temperature_unit,
            awg: state.awg,
        }
    }

    pub fn apply_to(self, state: &mut AppState) {
        for (channel, limit) in state.channels.iter_mut().zip(self.current_limits_ma) {
            channel.current_limit_ma = limit.min(3_000);
        }
        state.channels[3].setpoint_mv = self.ch4_voltage_mv.clamp(500, 5_000);
        state.channels[4].setpoint_mv = self
            .ch5_voltage_mv
            .clamp(CH5_MIN_VOLTAGE_MV, CH5_MAX_VOLTAGE_MV);
        state.channels[3].drive_mv = state.channels[3].setpoint_mv;
        state.channels[4].drive_mv = state.channels[4].setpoint_mv;
        state.channels[3].regulation_mode = self.ch4_regulation_mode;
        state.channels[4].regulation_mode = self.ch5_regulation_mode;
        state.sink_current_limit_ma = self.sink_current_limit_ma.min(5_000);
        state.temperature_unit = self.temperature_unit;
        state.awg = self.awg;
        state.awg_source = AwgSource::Builtin;
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum RecordKind {
    Autosave,
    Profile(u8),
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct SettingsRecord {
    pub sequence: u32,
    pub kind: RecordKind,
    pub settings: PersistentSettings,
}

pub struct SettingsDebouncer {
    observed: PersistentSettings,
    saved: PersistentSettings,
    quiet_ms: u16,
}

impl SettingsDebouncer {
    pub fn new(initial: PersistentSettings) -> Self {
        Self {
            observed: initial,
            saved: initial,
            quiet_ms: 0,
        }
    }

    pub fn tick(
        &mut self,
        current: PersistentSettings,
        transitions_stable: bool,
        outputs_physically_off: bool,
        elapsed_ms: u16,
    ) -> Option<PersistentSettings> {
        if current != self.observed {
            self.observed = current;
            self.quiet_ms = 0;
            return None;
        }
        if current == self.saved {
            return None;
        }
        self.quiet_ms = self.quiet_ms.saturating_add(elapsed_ms);
        if self.quiet_ms >= 1_000 && transitions_stable && outputs_physically_off {
            self.quiet_ms = 0;
            Some(current)
        } else {
            None
        }
    }

    pub fn mark_saved(&mut self, settings: PersistentSettings) {
        self.saved = settings;
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

pub fn encode(record: SettingsRecord) -> [u8; RECORD_SIZE] {
    let mut bytes = [0xff; RECORD_SIZE];
    bytes[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    let kind = match record.kind {
        RecordKind::Autosave => 0,
        RecordKind::Profile(slot @ 0..=2) => u32::from(slot) + 1,
        RecordKind::Profile(_) => 0,
    };
    let tagged_sequence = (record.sequence & 0x3fff_ffff) | (kind << 30);
    bytes[4..8].copy_from_slice(&tagged_sequence.to_le_bytes());
    for (index, limit) in record.settings.current_limits_ma.iter().enumerate() {
        let offset = 8 + index * 2;
        bytes[offset..offset + 2].copy_from_slice(&limit.to_le_bytes());
    }
    let ch4_voltage_and_mode = record.settings.ch4_voltage_mv
        | if record.settings.ch4_regulation_mode == RegulationMode::Cc {
            0x8000
        } else {
            0
        };
    bytes[18..20].copy_from_slice(&ch4_voltage_and_mode.to_le_bytes());
    let ch5_voltage_and_mode = record.settings.ch5_voltage_mv
        | if record.settings.ch5_regulation_mode == RegulationMode::Cc {
            0x8000
        } else {
            0
        };
    bytes[20..22].copy_from_slice(&ch5_voltage_and_mode.to_le_bytes());
    let sink_limit_and_unit = record.settings.sink_current_limit_ma
        | if record.settings.temperature_unit == TemperatureUnit::Fahrenheit {
            0x8000
        } else {
            0
        };
    bytes[22..24].copy_from_slice(&sink_limit_and_unit.to_le_bytes());
    bytes[24] = record.settings.awg.channel;
    bytes[25] = match record.settings.awg.waveform {
        AwgWaveform::Square => 0,
        AwgWaveform::Triangle => 1,
        AwgWaveform::Ramp => 2,
        AwgWaveform::Sine => 3,
    };
    bytes[26..30].copy_from_slice(&record.settings.awg.frequency_millihz.to_le_bytes());
    bytes[30..32].copy_from_slice(&record.settings.awg.low_mv.to_le_bytes());
    bytes[32..34].copy_from_slice(&record.settings.awg.high_mv.to_le_bytes());
    bytes[34] = record.settings.awg.duty_percent;
    let crc = crc32(&bytes[..40]);
    bytes[40..44].copy_from_slice(&crc.to_le_bytes());
    bytes[44..48].copy_from_slice(&COMMIT.to_le_bytes());
    bytes
}

pub fn decode(bytes: &[u8; RECORD_SIZE]) -> Option<SettingsRecord> {
    let word = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    if word(0) != MAGIC || word(44) != COMMIT || word(40) != crc32(&bytes[..40]) {
        return None;
    }
    let half = |offset| u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
    let ch4_voltage_and_mode = half(18);
    let ch5_voltage_and_mode = half(20);
    let tagged_sequence = word(4);
    let kind = match tagged_sequence >> 30 {
        0 => RecordKind::Autosave,
        value => RecordKind::Profile((value - 1) as u8),
    };
    let sink_limit_and_unit = half(22);
    let waveform = match bytes[25] {
        0 => AwgWaveform::Square,
        1 => AwgWaveform::Triangle,
        2 => AwgWaveform::Ramp,
        3 => AwgWaveform::Sine,
        _ => return None,
    };
    let record = SettingsRecord {
        sequence: tagged_sequence & 0x3fff_ffff,
        kind,
        settings: PersistentSettings {
            current_limits_ma: core::array::from_fn(|index| half(8 + index * 2)),
            ch4_voltage_mv: ch4_voltage_and_mode & 0x7fff,
            ch5_voltage_mv: ch5_voltage_and_mode & 0x7fff,
            ch4_regulation_mode: if ch4_voltage_and_mode & 0x8000 != 0 {
                RegulationMode::Cc
            } else {
                RegulationMode::Cv
            },
            ch5_regulation_mode: if ch5_voltage_and_mode & 0x8000 != 0 {
                RegulationMode::Cc
            } else {
                RegulationMode::Cv
            },
            sink_current_limit_ma: sink_limit_and_unit & 0x7fff,
            temperature_unit: if sink_limit_and_unit & 0x8000 != 0 {
                TemperatureUnit::Fahrenheit
            } else {
                TemperatureUnit::Celsius
            },
            awg: AwgConfig {
                channel: bytes[24],
                waveform,
                frequency_millihz: word(26),
                low_mv: half(30),
                high_mv: half(32),
                duty_percent: bytes[34],
            },
        },
    };
    let valid = record
        .settings
        .current_limits_ma
        .iter()
        .all(|value| *value <= 3_000)
        && (500..=5_000).contains(&record.settings.ch4_voltage_mv)
        && (CH5_MIN_VOLTAGE_MV..=CH5_MAX_VOLTAGE_MV).contains(&record.settings.ch5_voltage_mv)
        && record.settings.sink_current_limit_ma <= 5_000;
    let awg_minimum = if record.settings.awg.channel == 3 {
        500
    } else {
        CH5_MIN_VOLTAGE_MV
    };
    let awg_maximum = if record.settings.awg.channel == 3 {
        5_000
    } else {
        CH5_MAX_VOLTAGE_MV
    };
    let awg_max_frequency = record.settings.awg.waveform.max_frequency_millihz();
    (valid
        && matches!(record.settings.awg.channel, 3 | 4)
        && (awg_minimum..=awg_maximum).contains(&record.settings.awg.low_mv)
        && (record.settings.awg.low_mv..=awg_maximum).contains(&record.settings.awg.high_mv)
        && (1..=99).contains(&record.settings.awg.duty_percent)
        && (100..=awg_max_frequency).contains(&record.settings.awg.frequency_millihz))
    .then_some(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_and_rejects_torn_or_corrupt_data() {
        let record = SettingsRecord {
            sequence: 42,
            kind: RecordKind::Profile(1),
            settings: PersistentSettings {
                current_limits_ma: [100, 200, 300, 400, 500],
                ch4_voltage_mv: 2_500,
                ch5_voltage_mv: 12_000,
                ch4_regulation_mode: RegulationMode::Cc,
                ch5_regulation_mode: RegulationMode::Cc,
                sink_current_limit_ma: 4_250,
                temperature_unit: TemperatureUnit::Fahrenheit,
                awg: AwgConfig::default(),
            },
        };
        let encoded = encode(record);
        assert!(decode(&encoded) == Some(record));

        let mut torn = encoded;
        torn[44..48].fill(0xff);
        assert!(decode(&torn).is_none());

        let mut corrupt = encoded;
        corrupt[12] ^= 1;
        assert!(decode(&corrupt).is_none());

        let mut invalid_duty_record = record;
        invalid_duty_record.settings.awg.duty_percent = 0;
        assert!(decode(&encode(invalid_duty_record)).is_none());
    }

    #[test]
    fn record_kinds_and_temperature_units_are_independent() {
        for kind in [
            RecordKind::Autosave,
            RecordKind::Profile(0),
            RecordKind::Profile(1),
            RecordKind::Profile(2),
        ] {
            for temperature_unit in [TemperatureUnit::Celsius, TemperatureUnit::Fahrenheit] {
                let record = SettingsRecord {
                    sequence: 0x1234,
                    kind,
                    settings: PersistentSettings {
                        current_limits_ma: [10, 20, 30, 40, 50],
                        ch4_voltage_mv: 3_300,
                        ch5_voltage_mv: 9_000,
                        ch4_regulation_mode: RegulationMode::Cv,
                        ch5_regulation_mode: RegulationMode::Cc,
                        sink_current_limit_ma: 350,
                        temperature_unit,
                        awg: AwgConfig::default(),
                    },
                };
                assert!(decode(&encode(record)) == Some(record));
            }
        }
    }

    #[test]
    fn persistence_effect_debounces_edits_and_waits_for_stable_power() {
        let mut state = AppState::new(true, Some(25 * 16));
        let initial = PersistentSettings::from_state(&state);
        let mut effect = SettingsDebouncer::new(initial);
        state.channels[4].current_limit_ma = 400;
        let edited = PersistentSettings::from_state(&state);

        assert!(effect.tick(edited, true, true, 400).is_none());
        assert!(effect.tick(edited, true, true, 999).is_none());
        assert!(effect.tick(edited, false, true, 1).is_none());
        assert!(effect.tick(edited, true, false, 0).is_none());
        assert!(effect.tick(edited, true, true, 0) == Some(edited));
        effect.mark_saved(edited);
        assert!(effect.tick(edited, true, true, 5_000).is_none());

        state.channels[4].current_limit_ma = 410;
        let first = PersistentSettings::from_state(&state);
        assert!(effect.tick(first, true, true, 500).is_none());
        assert!(effect.tick(first, true, true, 500).is_none());
        state.channels[4].current_limit_ma = 420;
        let second = PersistentSettings::from_state(&state);
        assert!(effect.tick(second, true, true, 500).is_none());
        assert!(effect.tick(second, true, true, 999).is_none());
        assert!(effect.tick(second, true, true, 1) == Some(second));
    }
}
