use super::i2c::SoftI2c;
use crate::diagnostics::{record_hw_error, record_hw_retries};
use benchvolt_pd::power::{
    mcp4725_code_for_millivolts, tps55289_configuration_registers, tps55289_current_code,
    tps55289_output_acknowledged, tps55289_output_mode, tps55289_voltage_code, DriverOperation,
    PowerDriver, Rail, SHARED_RAIL_LIMIT_MA,
};
use embedded_hal::{
    blocking::delay::{DelayMs, DelayUs},
    digital::v2::{InputPin, OutputPin},
};
use stm32f0xx_hal::{delay::Delay, pac};

#[derive(Clone, Copy)]
pub(crate) enum HardwareError {
    Bus,
    Verify,
}

pub(crate) struct HardwarePowerDriver<B2SCL, B2SDA, B3SCL, B3SDA> {
    shared_bus: SoftI2c<B2SCL, B2SDA>,
    adjustable_bus: SoftI2c<B3SCL, B3SDA>,
    delay: Delay,
}

impl<B2SCL, B2SDA, B3SCL, B3SDA> HardwarePowerDriver<B2SCL, B2SDA, B3SCL, B3SDA>
where
    B2SCL: OutputPin,
    B2SDA: OutputPin + InputPin,
    B3SCL: OutputPin,
    B3SDA: OutputPin + InputPin,
{
    const TPS_DC1: u8 = 0x75;
    const TPS_DC2: u8 = 0x74;
    const TPS_CH5: u8 = 0x75;

    pub(crate) fn new(
        shared_bus: SoftI2c<B2SCL, B2SDA>,
        adjustable_bus: SoftI2c<B3SCL, B3SDA>,
        delay: Delay,
    ) -> Self {
        Self {
            shared_bus,
            adjustable_bus,
            delay,
        }
    }

    pub(crate) fn delay_ms(&mut self, milliseconds: u8) {
        self.delay.delay_ms(milliseconds);
    }

    pub(crate) fn delay_mut(&mut self) -> &mut Delay {
        &mut self.delay
    }

    pub(crate) fn read_temperature(&mut self) -> Option<i16> {
        self.shared_bus.read_tmp1075(&mut self.delay)
    }

    pub(crate) fn read_ch5_status(&mut self) -> Result<u8, HardwareError> {
        self.adjustable_bus
            .read_register(Self::TPS_CH5, 0x07, &mut self.delay)
            .map_err(|_| HardwareError::Bus)
    }

    pub(crate) fn read_rail_status(&mut self, rail: Rail) -> Result<u8, HardwareError> {
        self.shared_bus
            .read_register(
                match rail {
                    Rail::Dc1 => Self::TPS_DC1,
                    Rail::Dc2 => Self::TPS_DC2,
                },
                0x07,
                &mut self.delay,
            )
            .map_err(|_| HardwareError::Bus)
    }

    fn write_gpio(port: char, pin: u8, enabled: bool) {
        let bits = if enabled {
            1u32 << pin
        } else {
            1u32 << (pin + 16)
        };
        unsafe {
            match port {
                'A' => (*pac::GPIOA::ptr()).bsrr.write(|w| w.bits(bits)),
                'B' => (*pac::GPIOB::ptr()).bsrr.write(|w| w.bits(bits)),
                _ => (*pac::GPIOC::ptr()).bsrr.write(|w| w.bits(bits)),
            }
        }
    }

    fn gpio_is_set(port: char, pin: u8) -> bool {
        // Read IDR, not ODR: the output latch always mirrors the last BSRR
        // write, so an ODR read can never observe a stuck or shorted pin.
        // These EN pins are push-pull outputs, so IDR reflects the real
        // electrical level being driven.
        unsafe {
            let bits = match port {
                'A' => (*pac::GPIOA::ptr()).idr.read().bits(),
                'B' => (*pac::GPIOB::ptr()).idr.read().bits(),
                _ => (*pac::GPIOC::ptr()).idr.read().bits(),
            };
            bits & (1u32 << pin) != 0
        }
    }

    fn channel_gate(channel: u8, enabled: bool) -> Result<(), HardwareError> {
        let (port, pin) = match channel {
            0 => ('C', 12),
            1 => ('A', 15),
            2 => ('B', 15),
            3 => ('B', 6),
            _ => return Err(HardwareError::Verify),
        };
        Self::write_gpio(port, pin, enabled);
        if Self::gpio_is_set(port, pin) == enabled {
            Ok(())
        } else {
            Err(HardwareError::Verify)
        }
    }

    fn rail_enable(rail: Rail, enabled: bool) -> Result<(), HardwareError> {
        let (port, pin) = match rail {
            Rail::Dc1 => ('C', 13),
            Rail::Dc2 => ('B', 2),
        };
        Self::write_gpio(port, pin, enabled);
        if Self::gpio_is_set(port, pin) == enabled {
            Ok(())
        } else {
            Err(HardwareError::Verify)
        }
    }

    fn tps_configure<SCL, SDA>(
        bus: &mut SoftI2c<SCL, SDA>,
        delay: &mut Delay,
        address: u8,
        millivolts: u16,
        current_limit_ma: u16,
        enable_output: bool,
        forced_pwm: bool,
    ) -> Result<(), HardwareError>
    where
        SCL: OutputPin,
        SDA: OutputPin + InputPin,
    {
        const REF_LSB: u8 = 0x00;
        const REF_MSB: u8 = 0x01;
        const IOUT_LIMIT: u8 = 0x02;
        const VOUT_SR: u8 = 0x03;
        const VOUT_FS: u8 = 0x04;
        const MODE: u8 = 0x06;

        let vout_fs = bus
            .read_register(address, VOUT_FS, delay)
            .map_err(|_| HardwareError::Bus)?;
        let code = tps55289_voltage_code(millivolts);
        let current_code = tps55289_current_code(current_limit_ma);
        let mode = bus
            .read_register(address, MODE, delay)
            .map_err(|_| HardwareError::Bus)?;
        let slew = bus
            .read_register(address, VOUT_SR, delay)
            .map_err(|_| HardwareError::Bus)?;
        let [vout_fs, mode, slew] =
            tps55289_configuration_registers(vout_fs, mode, slew, enable_output, forced_pwm)
                .ok_or(HardwareError::Verify)?;
        for (register, value) in [
            (VOUT_FS, vout_fs),
            (REF_LSB, code as u8),
            (REF_MSB, ((code >> 8) & 0x07) as u8),
            (IOUT_LIMIT, current_code),
            (VOUT_SR, slew),
            (MODE, mode),
        ] {
            bus.write_register(address, register, value, delay)
                .map_err(|_| HardwareError::Bus)?;
            if bus
                .read_register(address, register, delay)
                .map_err(|_| HardwareError::Bus)?
                != value
            {
                return Err(HardwareError::Verify);
            }
        }
        Ok(())
    }

    fn tps_set_output<SCL, SDA>(
        bus: &mut SoftI2c<SCL, SDA>,
        delay: &mut Delay,
        address: u8,
        enabled: bool,
    ) -> Result<(), HardwareError>
    where
        SCL: OutputPin,
        SDA: OutputPin + InputPin,
    {
        const MODE: u8 = 0x06;
        let old = bus
            .read_register(address, MODE, delay)
            .map_err(|_| HardwareError::Bus)?;
        let next = tps55289_output_mode(old, enabled).ok_or(HardwareError::Verify)?;
        bus.write_register(address, MODE, next, delay)
            .map_err(|_| HardwareError::Bus)?;
        if bus
            .read_register(address, MODE, delay)
            .map_err(|_| HardwareError::Bus)?
            == next
        {
            Ok(())
        } else {
            Err(HardwareError::Verify)
        }
    }

    fn tps_set_voltage<SCL, SDA>(
        bus: &mut SoftI2c<SCL, SDA>,
        delay: &mut Delay,
        address: u8,
        millivolts: u16,
    ) -> Result<(), HardwareError>
    where
        SCL: OutputPin,
        SDA: OutputPin + InputPin,
    {
        let code = tps55289_voltage_code(millivolts);
        let reference = [code as u8, ((code >> 8) & 0x07) as u8];
        let mut last_error = HardwareError::Bus;
        for attempt in 0..3 {
            let result = bus
                .write_bytes(address, &[0x00, reference[0], reference[1]], delay)
                .map_err(|_| HardwareError::Bus)
                .and_then(|()| {
                    let mut verified = [0; 2];
                    bus.read_registers(address, 0x00, &mut verified, delay)
                        .map_err(|_| HardwareError::Bus)?;
                    (verified == reference)
                        .then_some(())
                        .ok_or(HardwareError::Verify)
                });
            match result {
                Ok(()) => {
                    if attempt != 0 {
                        record_hw_retries(attempt as u32);
                    }
                    return Ok(());
                }
                Err(error) => {
                    last_error = error;
                    delay.delay_us(100u32);
                }
            }
        }
        Err(last_error)
    }

    fn tps_verify<SCL, SDA>(
        bus: &mut SoftI2c<SCL, SDA>,
        delay: &mut Delay,
        address: u8,
    ) -> Result<(), HardwareError>
    where
        SCL: OutputPin,
        SDA: OutputPin + InputPin,
    {
        let mode = bus
            .read_register(address, 0x06, delay)
            .map_err(|_| HardwareError::Bus)?;
        let status = bus
            .read_register(address, 0x07, delay)
            .map_err(|_| HardwareError::Bus)?;
        if mode & 0x80 != 0 && status & 0xe0 == 0 && status & 0x03 != 0x03 {
            Ok(())
        } else {
            Err(HardwareError::Verify)
        }
    }

    fn tps_verify_output_enabled<SCL, SDA>(
        bus: &mut SoftI2c<SCL, SDA>,
        delay: &mut Delay,
        address: u8,
    ) -> Result<(), HardwareError>
    where
        SCL: OutputPin,
        SDA: OutputPin + InputPin,
    {
        let mode = bus
            .read_register(address, 0x06, delay)
            .map_err(|_| HardwareError::Bus)?;
        let status = bus
            .read_register(address, 0x07, delay)
            .map_err(|_| HardwareError::Bus)?;
        if tps55289_output_acknowledged(mode, status) {
            Ok(())
        } else {
            Err(HardwareError::Verify)
        }
    }
}

impl<B2SCL, B2SDA, B3SCL, B3SDA> PowerDriver for HardwarePowerDriver<B2SCL, B2SDA, B3SCL, B3SDA>
where
    B2SCL: OutputPin,
    B2SDA: OutputPin + InputPin,
    B3SCL: OutputPin,
    B3SDA: OutputPin + InputPin,
{
    type Error = HardwareError;

    fn apply(&mut self, operation: DriverOperation) -> Result<(), Self::Error> {
        let operation_code = match operation {
            DriverOperation::SetAdjustableDac { .. } => 1,
            DriverOperation::Ch5Voltage(_) => 2,
            DriverOperation::ConfigureCh5 { .. } => 3,
            DriverOperation::ClearCh5Status => 3,
            DriverOperation::Ch5OutputEnable(_) => 4,
            DriverOperation::ConfigureRail { .. } => 5,
            DriverOperation::VerifyRail { .. } | DriverOperation::VerifyOutput { .. } => 6,
            DriverOperation::Ch5Enable(_) => 8,
            _ => 7,
        };
        let result = match operation {
            DriverOperation::ChannelGate { channel, enabled } => {
                Self::channel_gate(channel, enabled)
            }
            DriverOperation::RailEnable { rail, enabled } => Self::rail_enable(rail, enabled),
            DriverOperation::ConfigureRail { rail, millivolts } => {
                let address = match rail {
                    Rail::Dc1 => Self::TPS_DC1,
                    Rail::Dc2 => Self::TPS_DC2,
                };
                Self::tps_configure(
                    &mut self.shared_bus,
                    &mut self.delay,
                    address,
                    millivolts,
                    SHARED_RAIL_LIMIT_MA,
                    true,
                    false,
                )
            }
            DriverOperation::VerifyRail { rail } => Self::tps_verify(
                &mut self.shared_bus,
                &mut self.delay,
                match rail {
                    Rail::Dc1 => Self::TPS_DC1,
                    Rail::Dc2 => Self::TPS_DC2,
                },
            ),
            DriverOperation::SetAdjustableDac { millivolts } => {
                let code = mcp4725_code_for_millivolts(millivolts);
                let bytes = [((code >> 8) & 0x0f) as u8, code as u8];
                let mut result = Err(HardwareError::Bus);
                for attempt in 0..3 {
                    result = self
                        .adjustable_bus
                        .write_bytes(0x60, &bytes, &mut self.delay)
                        .map_err(|_| HardwareError::Bus);
                    if result.is_ok() {
                        if attempt != 0 {
                            record_hw_retries(attempt as u32);
                        }
                        break;
                    }
                    self.delay.delay_us(100u32);
                }
                result
            }
            DriverOperation::Ch5Enable(enabled) => {
                Self::write_gpio('B', 7, enabled);
                // Unlike the bare load-switch gates, the TPS55289 EN net rises
                // slower than a next-instruction IDR read. Allow a bounded
                // settle before declaring the pin stuck at the wrong level.
                let mut result = Err(HardwareError::Verify);
                for _ in 0..10 {
                    if Self::gpio_is_set('B', 7) == enabled {
                        result = Ok(());
                        break;
                    }
                    self.delay.delay_us(100u32);
                }
                result
            }
            DriverOperation::ConfigureCh5 {
                millivolts,
                current_limit_ma,
                forced_pwm,
            } => Self::tps_configure(
                &mut self.adjustable_bus,
                &mut self.delay,
                Self::TPS_CH5,
                millivolts,
                current_limit_ma,
                false,
                forced_pwm,
            ),
            DriverOperation::ClearCh5Status => self.read_ch5_status().map(|_| ()),
            DriverOperation::Ch5OutputEnable(enabled) => Self::tps_set_output(
                &mut self.adjustable_bus,
                &mut self.delay,
                Self::TPS_CH5,
                enabled,
            ),
            DriverOperation::Ch5Voltage(millivolts) => Self::tps_set_voltage(
                &mut self.adjustable_bus,
                &mut self.delay,
                Self::TPS_CH5,
                millivolts,
            ),
            DriverOperation::VerifyOutput { channel: 4, .. } => Self::tps_verify_output_enabled(
                &mut self.adjustable_bus,
                &mut self.delay,
                Self::TPS_CH5,
            ),
            DriverOperation::VerifyOutput { channel, .. } => {
                let (port, pin) = match channel {
                    0 => ('C', 12),
                    1 => ('A', 15),
                    2 => ('B', 15),
                    3 => ('B', 6),
                    _ => return Err(HardwareError::Verify),
                };
                if Self::gpio_is_set(port, pin) {
                    Ok(())
                } else {
                    Err(HardwareError::Verify)
                }
            }
        };
        if let Err(error) = result {
            record_hw_error(
                operation_code,
                match error {
                    HardwareError::Bus => 1,
                    HardwareError::Verify => 2,
                },
            );
        }
        result
    }
}
