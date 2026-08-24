use crate::app::AppState;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AwgDamage {
    pub rows: u8,
    pub values: u8,
    pub load_heading: bool,
    pub load_current: bool,
    pub load_power: bool,
}

fn load_channel_projection(state: &AppState) -> u8 {
    if state.awg_status == crate::app::AwgStatus::Running {
        state.active_awg_channel()
    } else {
        state.awg.channel
    }
}

pub fn awg_damage(old: &AppState, new: &AppState) -> AwgDamage {
    let mut damage = AwgDamage::default();

    if old.menu_selection != new.menu_selection {
        damage.rows |= 1 << usize::from(old.menu_selection);
        damage.rows |= 1 << usize::from(new.menu_selection);
    }
    if old.awg_editing != new.awg_editing {
        damage.rows |= 1 << usize::from(new.menu_selection);
    }
    if old.awg.channel != new.awg.channel {
        damage.values |= 1 << 0;
    }
    if old.awg.waveform != new.awg.waveform {
        damage.values |= 1 << 1;
        damage.values |= 1 << 3;
    }
    if old.awg.frequency_millihz != new.awg.frequency_millihz {
        damage.values |= 1 << 2;
    }
    if old.awg.duty_percent != new.awg.duty_percent {
        damage.values |= 1 << 3;
    }
    if old.awg.low_mv != new.awg.low_mv {
        damage.values |= 1 << 4;
    }
    if old.awg.high_mv != new.awg.high_mv {
        damage.values |= 1 << 5;
    }
    if old.awg_status != new.awg_status {
        damage.values |= 1 << 6;
    }
    damage.load_heading = load_channel_projection(old) != load_channel_projection(new);
    damage.load_current = (old.awg_load.valid, old.awg_load.milliamps_rms)
        != (new.awg_load.valid, new.awg_load.milliamps_rms);
    damage.load_power = (old.awg_load.valid, old.awg_load.milliwatts_average / 10)
        != (new.awg_load.valid, new.awg_load.milliwatts_average / 10);

    // A row repaint already includes its value. Keep each damaged pixel region single-owner.
    damage.values &= !damage.rows;
    damage
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_edit_invalidates_only_the_changed_value() {
        let old = AppState::new(true, None);
        let mut new = old;
        new.awg.frequency_millihz = 2_000;

        assert_eq!(
            awg_damage(&old, &new),
            AwgDamage {
                rows: 0,
                values: 1 << 2,
                ..AwgDamage::default()
            }
        );
    }

    #[test]
    fn selection_invalidates_only_old_and_new_rows() {
        let mut old = AppState::new(true, None);
        old.menu_selection = 2;
        let mut new = old;
        new.menu_selection = 3;

        assert_eq!(
            awg_damage(&old, &new),
            AwgDamage {
                rows: (1 << 2) | (1 << 3),
                values: 0,
                ..AwgDamage::default()
            }
        );
    }

    #[test]
    fn entering_edit_mode_invalidates_only_the_selected_row() {
        let mut old = AppState::new(true, None);
        old.menu_selection = 4;
        let mut new = old;
        new.awg_editing = true;

        assert_eq!(
            awg_damage(&old, &new),
            AwgDamage {
                rows: 1 << 4,
                values: 0,
                ..AwgDamage::default()
            }
        );
    }

    #[test]
    fn scheduler_drive_updates_do_not_damage_awg_controls() {
        let old = AppState::new(true, None);
        let mut new = old;
        new.channels[3].drive_mv = 2_345;

        assert_eq!(awg_damage(&old, &new), AwgDamage::default());
    }

    #[test]
    fn duty_edit_invalidates_only_duty_value() {
        let old = AppState::new(true, None);
        let mut new = old;
        new.awg.duty_percent = 67;
        assert_eq!(
            awg_damage(&old, &new),
            AwgDamage {
                rows: 0,
                values: 1 << 3,
                ..AwgDamage::default()
            }
        );
    }

    #[test]
    fn waveform_change_also_invalidates_duty_availability() {
        let old = AppState::new(true, None);
        let mut new = old;
        new.awg.waveform = crate::app::AwgWaveform::Triangle;
        assert_eq!(
            awg_damage(&old, &new),
            AwgDamage {
                rows: 0,
                values: (1 << 1) | (1 << 3),
                ..AwgDamage::default()
            }
        );
    }

    #[test]
    fn load_damage_tracks_only_text_visible_precision() {
        let mut old = AppState::new(true, None);
        old.awg_load = crate::app::LoadMeasurement {
            milliamps_rms: 420,
            milliwatts_average: 1_234,
            valid: true,
        };
        let mut hidden_change = old;
        hidden_change.awg_load.milliwatts_average = 1_239;
        assert_eq!(awg_damage(&old, &hidden_change), AwgDamage::default());

        let mut visible_change = old;
        visible_change.awg_load.milliwatts_average = 1_240;
        assert_eq!(
            awg_damage(&old, &visible_change),
            AwgDamage {
                load_power: true,
                ..AwgDamage::default()
            }
        );
    }

    #[test]
    fn current_and_power_are_independent_damage_regions() {
        let mut old = AppState::new(true, None);
        old.awg_load = crate::app::LoadMeasurement {
            milliamps_rms: 420,
            milliwatts_average: 1_230,
            valid: true,
        };
        let mut new = old;
        new.awg_load.milliamps_rms = 421;
        assert_eq!(
            awg_damage(&old, &new),
            AwgDamage {
                load_current: true,
                ..AwgDamage::default()
            }
        );
    }
}
