use crate::app::AppState;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FramedValueDamage {
    None,
    Value,
    Frame,
}

pub const fn framed_value_damage(
    old_value: u16,
    new_value: u16,
    old_focused: bool,
    new_focused: bool,
) -> FramedValueDamage {
    if old_focused != new_focused {
        FramedValueDamage::Frame
    } else if old_value != new_value {
        FramedValueDamage::Value
    } else {
        FramedValueDamage::None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SinkPdStatus {
    Negotiating,
    Ready(crate::pd::Contract),
    Error(crate::pd::PdError),
    Fault(crate::app::Fault),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SinkProjection {
    pub voltage_centivolts: Option<u16>,
    pub current_centiamps: Option<u16>,
    pub power_centiwatts: Option<u32>,
    pub limit_centiamps: u16,
    pub focused: bool,
    pub over_limit: bool,
    pub pd_status: SinkPdStatus,
}

pub fn sink_projection(state: &AppState) -> SinkProjection {
    SinkProjection {
        voltage_centivolts: state.sink.valid.then_some(state.sink.millivolts / 10),
        current_centiamps: state.sink.valid.then_some(state.sink.milliamps / 10),
        power_centiwatts: state
            .sink
            .valid
            .then_some(u32::from(state.sink.millivolts) * u32::from(state.sink.milliamps) / 10_000),
        limit_centiamps: state.sink_current_limit_ma / 10,
        focused: state.focus == crate::app::ControlFocus::CurrentLimit,
        over_limit: state.sink_fault != crate::app::Fault::None
            || (state.sink.valid && state.sink.milliamps > state.sink_current_limit_ma),
        pd_status: if state.sink_fault != crate::app::Fault::None {
            SinkPdStatus::Fault(state.sink_fault)
        } else if let Some(contract) = state.pd_contract {
            SinkPdStatus::Ready(contract)
        } else if let Some(error) = state.pd_error {
            SinkPdStatus::Error(error)
        } else {
            SinkPdStatus::Negotiating
        },
    }
}

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
    fn value_edits_preserve_the_existing_focus_frame() {
        assert_eq!(
            framed_value_damage(500, 501, true, true),
            FramedValueDamage::Value
        );
        assert_eq!(
            framed_value_damage(500, 501, false, false),
            FramedValueDamage::Value
        );
        assert_eq!(
            framed_value_damage(500, 501, false, true),
            FramedValueDamage::Frame
        );
        assert_eq!(
            framed_value_damage(500, 500, true, true),
            FramedValueDamage::None
        );
    }

    #[test]
    fn sink_projection_keeps_latched_fault_visibly_asserted() {
        let mut state = AppState::new(true, None);
        state.sink = crate::app::Measurement {
            millivolts: 5_000,
            milliamps: 100,
            valid: true,
        };
        state.sink_current_limit_ma = 1_000;
        state.sink_fault = crate::app::Fault::OverCurrent;

        let projection = sink_projection(&state);
        assert!(projection.over_limit);
        assert_eq!(projection.current_centiamps, Some(10));
        assert_eq!(
            projection.pd_status,
            SinkPdStatus::Fault(crate::app::Fault::OverCurrent)
        );
    }

    #[test]
    fn sink_projection_exposes_negotiated_contract_status() {
        let mut state = AppState::new(true, None);
        let contract = crate::pd::Contract {
            source_position: 3,
            millivolts: 20_000,
            operating_milliamps: 1_500,
            maximum_milliamps: 2_000,
        };
        state.pd_contract = Some(contract);

        assert_eq!(
            sink_projection(&state).pd_status,
            SinkPdStatus::Ready(contract)
        );
    }

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
