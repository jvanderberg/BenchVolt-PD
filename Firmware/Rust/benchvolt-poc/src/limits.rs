//! Electrical limits imposed by the r3 hardware.

/// CH5 is labeled 0.8–22 V in the r3 schematic. Its 6.8 kΩ/1 kΩ ADC
/// divider would expose the MCU analog input to more than 3.3 V at 32 V.
pub const CH5_MIN_VOLTAGE_MV: u16 = 800;
pub const CH5_MAX_VOLTAGE_MV: u16 = 22_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ch5_limit_keeps_the_r3_measurement_input_below_vdda() {
        // R35/R36 form a 6.8 kΩ/1 kΩ divider: ADC = Vout / 7.8.
        let adc_mv = u32::from(CH5_MAX_VOLTAGE_MV) * 10 / 78;
        let stale_32v_adc_mv = 32_000u32 * 10 / 78;

        assert!(adc_mv < 3_300);
        assert!(stale_32v_adc_mv > 3_300);
    }
}
