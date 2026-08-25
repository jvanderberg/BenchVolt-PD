use core::fmt::Write as _;

use heapless::String;

use crate::app::{
    AppState, ChannelSnapshot, ControlFocus, Fault, OutputTransition, RegulationMode,
    TemperatureUnit,
};

pub const fn centered_origin(container_origin: i32, container_size: u32, item_size: u32) -> i32 {
    container_origin + ((container_size - item_size) / 2) as i32
}

pub const fn seven_segment_mask(character: char) -> Option<u8> {
    const DIGITS: [u8; 10] = [
        0b111_0111, 0b010_0100, 0b101_1101, 0b110_1101, 0b010_1110, 0b110_1011, 0b111_1011,
        0b010_0101, 0b111_1111, 0b110_1111,
    ];
    match character {
        '0'..='9' => Some(DIGITS[character as usize - '0' as usize]),
        '-' => Some(1 << 3),
        _ => None,
    }
}

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
    Idle,
    Negotiating,
    Ready(crate::pd::Contract),
    Error(crate::pd::PdError),
    Fault(crate::app::Fault),
}

pub fn pd_contract_label(contract: crate::pd::Contract) -> String<16> {
    let mut label = String::new();
    write!(
        &mut label,
        "P{} {}V {}.{}A",
        contract.source_position,
        contract.millivolts / 1_000,
        contract.operating_milliamps / 1_000,
        contract.operating_milliamps % 1_000 / 100,
    )
    .ok();
    label
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
        } else if state.pd_negotiating {
            SinkPdStatus::Negotiating
        } else {
            SinkPdStatus::Idle
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

/// One-based channel number shown in the AWG "CHn LOAD" heading. Shared by
/// the damage computation and the painter so they cannot drift.
pub fn load_channel_number(state: &AppState) -> u8 {
    load_channel_projection(state) + 1
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

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum TemperatureProjection {
    Invalid,
    Tenths(i32, TemperatureUnit),
}

pub fn temperature_projection(state: &AppState) -> TemperatureProjection {
    if !state.temp_valid {
        return TemperatureProjection::Invalid;
    }
    let tenths_c = i32::from(state.temp_sixteenths_c) * 10 / 16;
    match state.temperature_unit {
        TemperatureUnit::Celsius => {
            TemperatureProjection::Tenths(tenths_c, TemperatureUnit::Celsius)
        }
        TemperatureUnit::Fahrenheit => {
            TemperatureProjection::Tenths(tenths_c * 9 / 5 + 320, TemperatureUnit::Fahrenheit)
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum StatusProjection {
    Fault,
    On,
    Wait,
    Off,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ChannelProjection {
    pub setpoint_centivolts: u16,
    pub limit_centiamps: u16,
    pub measured_centivolts: Option<u16>,
    pub measured_centiamps: Option<u16>,
    pub status: StatusProjection,
    pub regulation_mode: RegulationMode,
    pub regulating_current: bool,
}

pub fn channel_projection(channel: &ChannelSnapshot) -> ChannelProjection {
    let status = match channel.fault {
        Fault::None if channel.transition != OutputTransition::Stable => StatusProjection::Wait,
        Fault::None if channel.physical_enabled => StatusProjection::On,
        Fault::None if channel.requested_enabled => StatusProjection::Wait,
        Fault::None => StatusProjection::Off,
        _ => StatusProjection::Fault,
    };
    ChannelProjection {
        setpoint_centivolts: channel.setpoint_mv / 10,
        limit_centiamps: channel.current_limit_ma / 10,
        measured_centivolts: channel
            .measurement
            .valid
            .then_some(channel.measurement.millivolts / 10),
        measured_centiamps: channel
            .measurement
            .valid
            .then_some(channel.measurement.milliamps / 10),
        status,
        regulation_mode: channel.regulation_mode,
        regulating_current: channel.regulation_mode == RegulationMode::Cc
            && channel.physical_enabled
            && channel.drive_mv < channel.setpoint_mv,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct DetailProjection {
    pub voltage_centivolts: Option<u16>,
    pub current_centiamps: Option<u16>,
    pub power_centiwatts: Option<u32>,
    pub setpoint_centivolts: u16,
    pub limit_centiamps: u16,
    pub status: StatusProjection,
    pub focus: ControlFocus,
    pub regulation_mode: RegulationMode,
    pub regulating_current: bool,
}

pub fn detail_projection(channel: &ChannelSnapshot, focus: ControlFocus) -> DetailProjection {
    let row = channel_projection(channel);
    DetailProjection {
        voltage_centivolts: row.measured_centivolts,
        current_centiamps: row.measured_centiamps,
        power_centiwatts: channel.measurement.valid.then_some(
            u32::from(channel.measurement.millivolts) * u32::from(channel.measurement.milliamps)
                / 10_000,
        ),
        setpoint_centivolts: row.setpoint_centivolts,
        limit_centiamps: row.limit_centiamps,
        status: row.status,
        focus,
        regulation_mode: channel.regulation_mode,
        regulating_current: row.regulating_current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_status_precedence_is_fault_then_wait_then_on_then_off() {
        let mut state = AppState::new(true, Some(25 * 16));
        let output = &mut state.channels[0];

        assert!(channel_projection(output).status == StatusProjection::Off);

        output.requested_enabled = true;
        assert!(channel_projection(output).status == StatusProjection::Wait);

        output.physical_enabled = true;
        output.transition = OutputTransition::Enabling(1);
        assert!(channel_projection(output).status == StatusProjection::Wait);

        output.transition = OutputTransition::Stable;
        assert!(channel_projection(output).status == StatusProjection::On);

        // Any latched fault dominates every other status.
        output.fault = Fault::OverCurrent;
        assert!(channel_projection(output).status == StatusProjection::Fault);
    }

    #[test]
    fn cc_indicator_asserts_only_while_the_drive_is_backed_off() {
        let mut state = AppState::new(true, Some(25 * 16));
        let output = &mut state.channels[4];
        output.regulation_mode = RegulationMode::Cc;
        output.physical_enabled = true;
        output.setpoint_mv = 12_000;
        output.drive_mv = 12_000;
        assert!(!channel_projection(output).regulating_current);

        output.drive_mv = 9_000;
        assert!(channel_projection(output).regulating_current);

        output.physical_enabled = false;
        assert!(!channel_projection(output).regulating_current);
    }

    #[test]
    fn temperature_projects_tenths_in_both_units_and_flags_invalid() {
        let mut state = AppState::new(true, Some(25 * 16));
        state.temp_valid = true;
        state.temp_sixteenths_c = 25 * 16;
        assert!(
            temperature_projection(&state)
                == TemperatureProjection::Tenths(250, TemperatureUnit::Celsius)
        );

        state.temperature_unit = TemperatureUnit::Fahrenheit;
        assert!(
            temperature_projection(&state)
                == TemperatureProjection::Tenths(770, TemperatureUnit::Fahrenheit)
        );

        state.temp_valid = false;
        assert!(temperature_projection(&state) == TemperatureProjection::Invalid);
    }

    #[test]
    fn detail_projection_derives_power_from_the_shared_row_projection() {
        let mut state = AppState::new(true, Some(25 * 16));
        let output = &mut state.channels[3];
        output.measurement = crate::app::Measurement {
            millivolts: 5_000,
            milliamps: 2_000,
            valid: true,
        };
        let detail = detail_projection(output, ControlFocus::Voltage);
        assert_eq!(detail.power_centiwatts, Some(1_000));
        assert_eq!(detail.voltage_centivolts, Some(500));
        assert_eq!(detail.current_centiamps, Some(200));

        output.measurement.valid = false;
        let detail = detail_projection(output, ControlFocus::Voltage);
        assert_eq!(detail.power_centiwatts, None);
        assert_eq!(detail.voltage_centivolts, None);
    }

    #[test]
    fn toggle_knobs_share_the_track_center_at_both_sizes() {
        assert_eq!(centered_origin(5, 14, 10), 7);
        assert_eq!(centered_origin(130, 28, 22), 133);

        // Compare doubled centers to avoid fractional-pixel arithmetic.
        assert_eq!(2 * 5 + 14, 2 * 7 + 10);
        assert_eq!(2 * 130 + 28, 2 * 133 + 22);
    }

    #[test]
    fn seven_segment_masks_cover_digits_and_minus_without_aliases() {
        assert_eq!(seven_segment_mask('0'), Some(0b111_0111));
        assert_eq!(seven_segment_mask('1'), Some(0b010_0100));
        assert_eq!(seven_segment_mask('8'), Some(0b111_1111));
        assert_eq!(seven_segment_mask('-'), Some(0b000_1000));
        assert_eq!(seven_segment_mask('.'), None);
    }

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
    fn compact_pd_label_preserves_fractional_contract_current() {
        let contract = crate::pd::Contract {
            source_position: 3,
            millivolts: 20_000,
            operating_milliamps: 1_500,
            maximum_milliamps: 3_000,
        };
        assert_eq!(pd_contract_label(contract).as_str(), "P3 20V 1.5A");

        let low_current = crate::pd::Contract {
            source_position: 2,
            millivolts: 9_000,
            operating_milliamps: 500,
            maximum_milliamps: 500,
        };
        assert_eq!(pd_contract_label(low_current).as_str(), "P2 9V 0.5A");
        assert!(pd_contract_label(contract).len() * 8 <= 104);
    }

    #[test]
    fn sink_projection_distinguishes_idle_from_active_negotiation() {
        let mut state = AppState::new(false, Some(400));
        assert_eq!(sink_projection(&state).pd_status, SinkPdStatus::Idle);
        state.pd_negotiating = true;
        assert_eq!(sink_projection(&state).pd_status, SinkPdStatus::Negotiating);
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
