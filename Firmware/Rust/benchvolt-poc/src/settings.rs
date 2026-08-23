use crate::app::{AppState, RegulationMode};

pub const RECORD_SIZE: usize = 32;
const MAGIC: u32 = 0x4256_5331;
const COMMIT: u32 = 0x434f_4d54;

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct PersistentSettings {
    pub current_limits_ma: [u16; 5],
    pub ch4_voltage_mv: u16,
    pub ch5_voltage_mv: u16,
    pub ch4_regulation_mode: RegulationMode,
    pub ch5_regulation_mode: RegulationMode,
    pub sink_current_limit_ma: u16,
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
        }
    }

    pub fn apply_to(self, state: &mut AppState) {
        for (channel, limit) in state.channels.iter_mut().zip(self.current_limits_ma) {
            channel.current_limit_ma = limit.min(3_000);
        }
        state.channels[3].setpoint_mv = self.ch4_voltage_mv.clamp(500, 5_000);
        state.channels[4].setpoint_mv = self.ch5_voltage_mv.clamp(800, 22_000);
        state.channels[3].drive_mv = state.channels[3].setpoint_mv;
        state.channels[4].drive_mv = state.channels[4].setpoint_mv;
        state.channels[3].regulation_mode = self.ch4_regulation_mode;
        state.channels[4].regulation_mode = self.ch5_regulation_mode;
        state.sink_current_limit_ma = self.sink_current_limit_ma.min(5_000);
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct SettingsRecord {
    pub sequence: u32,
    pub settings: PersistentSettings,
}

pub struct SettingsDebouncer {
    observed: PersistentSettings,
    saved: PersistentSettings,
    quiet_ticks: u16,
}

impl SettingsDebouncer {
    pub fn new(initial: PersistentSettings) -> Self {
        Self {
            observed: initial,
            saved: initial,
            quiet_ticks: 0,
        }
    }

    pub fn tick(
        &mut self,
        current: PersistentSettings,
        transitions_stable: bool,
    ) -> Option<PersistentSettings> {
        if current != self.observed {
            self.observed = current;
            self.quiet_ticks = 0;
            return None;
        }
        if current == self.saved {
            return None;
        }
        self.quiet_ticks = self.quiet_ticks.saturating_add(1);
        if self.quiet_ticks >= 1_000 && transitions_stable {
            self.quiet_ticks = 0;
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
    bytes[4..8].copy_from_slice(&record.sequence.to_le_bytes());
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
    bytes[22..24].copy_from_slice(&record.settings.sink_current_limit_ma.to_le_bytes());
    let crc = crc32(&bytes[..24]);
    bytes[24..28].copy_from_slice(&crc.to_le_bytes());
    bytes[28..32].copy_from_slice(&COMMIT.to_le_bytes());
    bytes
}

pub fn decode(bytes: &[u8; RECORD_SIZE]) -> Option<SettingsRecord> {
    let word = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    if word(0) != MAGIC || word(28) != COMMIT || word(24) != crc32(&bytes[..24]) {
        return None;
    }
    let half = |offset| u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap());
    let ch4_voltage_and_mode = half(18);
    let ch5_voltage_and_mode = half(20);
    let record = SettingsRecord {
        sequence: word(4),
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
            sink_current_limit_ma: half(22),
        },
    };
    let valid = record
        .settings
        .current_limits_ma
        .iter()
        .all(|value| *value <= 3_000)
        && (500..=5_000).contains(&record.settings.ch4_voltage_mv)
        && (800..=22_000).contains(&record.settings.ch5_voltage_mv)
        && record.settings.sink_current_limit_ma <= 5_000;
    valid.then_some(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_and_rejects_torn_or_corrupt_data() {
        let record = SettingsRecord {
            sequence: 42,
            settings: PersistentSettings {
                current_limits_ma: [100, 200, 300, 400, 500],
                ch4_voltage_mv: 2_500,
                ch5_voltage_mv: 12_000,
                ch4_regulation_mode: RegulationMode::Cc,
                ch5_regulation_mode: RegulationMode::Cc,
                sink_current_limit_ma: 4_250,
            },
        };
        let encoded = encode(record);
        assert!(decode(&encoded) == Some(record));

        let mut torn = encoded;
        torn[28..32].fill(0xff);
        assert!(decode(&torn).is_none());

        let mut corrupt = encoded;
        corrupt[12] ^= 1;
        assert!(decode(&corrupt).is_none());
    }

    #[test]
    fn persistence_effect_debounces_edits_and_waits_for_stable_power() {
        let mut state = AppState::new(true, Some(25 * 16));
        let initial = PersistentSettings::from_state(&state);
        let mut effect = SettingsDebouncer::new(initial);
        state.channels[4].current_limit_ma = 400;
        let edited = PersistentSettings::from_state(&state);

        assert!(effect.tick(edited, true).is_none());
        for _ in 0..999 {
            assert!(effect.tick(edited, true).is_none());
        }
        assert!(effect.tick(edited, false).is_none());
        assert!(effect.tick(edited, true) == Some(edited));
        effect.mark_saved(edited);
        assert!(effect.tick(edited, true).is_none());

        state.channels[4].current_limit_ma = 410;
        let first = PersistentSettings::from_state(&state);
        assert!(effect.tick(first, true).is_none());
        for _ in 0..500 {
            assert!(effect.tick(first, true).is_none());
        }
        state.channels[4].current_limit_ma = 420;
        let second = PersistentSettings::from_state(&state);
        assert!(effect.tick(second, true).is_none());
        for _ in 0..999 {
            assert!(effect.tick(second, true).is_none());
        }
        assert!(effect.tick(second, true) == Some(second));
    }
}
