use benchvolt_poc::pd::{
    BusError as PdBusError, PdBus, STUSB4500_ADDRESS, STUSB4500_I2C_HALF_CYCLE_US,
};
use embedded_hal::{
    blocking::delay::DelayUs,
    digital::v2::{InputPin, OutputPin},
};
use stm32f0xx_hal::delay::Delay;

pub(crate) struct SoftI2c<SCL, SDA, const HALF_CYCLE_US: u32 = 2> {
    scl: SCL,
    sda: SDA,
}

impl<SCL, SDA, const HALF_CYCLE_US: u32> SoftI2c<SCL, SDA, HALF_CYCLE_US>
where
    SCL: OutputPin,
    SDA: OutputPin + InputPin,
{
    pub(crate) fn new(mut scl: SCL, mut sda: SDA) -> Self {
        scl.set_high().ok();
        sda.set_high().ok();
        Self { scl, sda }
    }

    fn half_cycle(delay: &mut impl DelayUs<u32>) {
        delay.delay_us(HALF_CYCLE_US);
    }

    fn start(&mut self, delay: &mut impl DelayUs<u32>) {
        self.sda.set_high().ok();
        self.scl.set_high().ok();
        Self::half_cycle(delay);
        self.sda.set_low().ok();
        Self::half_cycle(delay);
        self.scl.set_low().ok();
    }

    fn stop(&mut self, delay: &mut impl DelayUs<u32>) {
        self.sda.set_low().ok();
        Self::half_cycle(delay);
        self.scl.set_high().ok();
        Self::half_cycle(delay);
        self.sda.set_high().ok();
        Self::half_cycle(delay);
    }

    fn write_byte(&mut self, mut value: u8, delay: &mut impl DelayUs<u32>) -> bool {
        for _ in 0..8 {
            if value & 0x80 == 0 {
                self.sda.set_low().ok();
            } else {
                self.sda.set_high().ok();
            }
            Self::half_cycle(delay);
            self.scl.set_high().ok();
            Self::half_cycle(delay);
            self.scl.set_low().ok();
            value <<= 1;
        }
        self.sda.set_high().ok();
        Self::half_cycle(delay);
        self.scl.set_high().ok();
        Self::half_cycle(delay);
        let acknowledged = self.sda.is_low().unwrap_or(false);
        self.scl.set_low().ok();
        acknowledged
    }

    fn read_byte(&mut self, acknowledge: bool, delay: &mut impl DelayUs<u32>) -> u8 {
        self.sda.set_high().ok();
        let mut value = 0u8;
        for _ in 0..8 {
            value <<= 1;
            self.scl.set_high().ok();
            Self::half_cycle(delay);
            if self.sda.is_high().unwrap_or(false) {
                value |= 1;
            }
            self.scl.set_low().ok();
            Self::half_cycle(delay);
        }
        if acknowledge {
            self.sda.set_low().ok();
        } else {
            self.sda.set_high().ok();
        }
        Self::half_cycle(delay);
        self.scl.set_high().ok();
        Self::half_cycle(delay);
        self.scl.set_low().ok();
        self.sda.set_high().ok();
        value
    }

    pub(crate) fn read_tmp1075(&mut self, delay: &mut impl DelayUs<u32>) -> Option<i16> {
        const ADDR_WRITE: u8 = 0x48 << 1;
        const ADDR_READ: u8 = ADDR_WRITE | 1;

        self.start(delay);
        if !self.write_byte(ADDR_WRITE, delay) || !self.write_byte(0, delay) {
            self.stop(delay);
            return None;
        }
        self.start(delay);
        if !self.write_byte(ADDR_READ, delay) {
            self.stop(delay);
            return None;
        }
        let msb = self.read_byte(true, delay);
        let lsb = self.read_byte(false, delay);
        self.stop(delay);
        Some(i16::from_be_bytes([msb, lsb]) >> 4)
    }

    pub(crate) fn write_register(
        &mut self,
        address: u8,
        register: u8,
        value: u8,
        delay: &mut impl DelayUs<u32>,
    ) -> Result<(), ()> {
        self.start(delay);
        let acknowledged = self.write_byte(address << 1, delay)
            && self.write_byte(register, delay)
            && self.write_byte(value, delay);
        self.stop(delay);
        if acknowledged {
            Ok(())
        } else {
            Err(())
        }
    }

    pub(crate) fn read_register(
        &mut self,
        address: u8,
        register: u8,
        delay: &mut impl DelayUs<u32>,
    ) -> Result<u8, ()> {
        self.start(delay);
        if !self.write_byte(address << 1, delay) || !self.write_byte(register, delay) {
            self.stop(delay);
            return Err(());
        }
        self.start(delay);
        if !self.write_byte((address << 1) | 1, delay) {
            self.stop(delay);
            return Err(());
        }
        let value = self.read_byte(false, delay);
        self.stop(delay);
        Ok(value)
    }

    pub(crate) fn read_registers(
        &mut self,
        address: u8,
        register: u8,
        values: &mut [u8],
        delay: &mut impl DelayUs<u32>,
    ) -> Result<(), ()> {
        self.start(delay);
        if !self.write_byte(address << 1, delay) || !self.write_byte(register, delay) {
            self.stop(delay);
            return Err(());
        }
        self.start(delay);
        if !self.write_byte((address << 1) | 1, delay) {
            self.stop(delay);
            return Err(());
        }
        let last = values.len().saturating_sub(1);
        for (index, value) in values.iter_mut().enumerate() {
            *value = self.read_byte(index != last, delay);
        }
        self.stop(delay);
        Ok(())
    }

    pub(crate) fn write_bytes(
        &mut self,
        address: u8,
        bytes: &[u8],
        delay: &mut impl DelayUs<u32>,
    ) -> Result<(), ()> {
        self.start(delay);
        if !self.write_byte(address << 1, delay) {
            self.stop(delay);
            return Err(());
        }
        for byte in bytes {
            if !self.write_byte(*byte, delay) {
                self.stop(delay);
                return Err(());
            }
        }
        self.stop(delay);
        Ok(())
    }

    pub(crate) fn write_registers(
        &mut self,
        address: u8,
        register: u8,
        values: &[u8],
        delay: &mut impl DelayUs<u32>,
    ) -> Result<(), ()> {
        self.start(delay);
        let mut acknowledged =
            self.write_byte(address << 1, delay) && self.write_byte(register, delay);
        for value in values {
            acknowledged &= self.write_byte(*value, delay);
        }
        self.stop(delay);
        if acknowledged {
            Ok(())
        } else {
            Err(())
        }
    }
}

pub(crate) struct SoftPdBus<'a, SCL, SDA> {
    bus: &'a mut SoftI2c<SCL, SDA, STUSB4500_I2C_HALF_CYCLE_US>,
    delay: &'a mut Delay,
}

impl<'a, SCL, SDA> SoftPdBus<'a, SCL, SDA> {
    pub(crate) fn new(
        bus: &'a mut SoftI2c<SCL, SDA, STUSB4500_I2C_HALF_CYCLE_US>,
        delay: &'a mut Delay,
    ) -> Self {
        Self { bus, delay }
    }
}

impl<SCL, SDA> PdBus for SoftPdBus<'_, SCL, SDA>
where
    SCL: OutputPin,
    SDA: OutputPin + InputPin,
{
    fn read(&mut self, register: u8, values: &mut [u8]) -> Result<(), PdBusError> {
        self.bus
            .read_registers(STUSB4500_ADDRESS, register, values, self.delay)
            .map_err(|_| PdBusError)
    }

    fn write(&mut self, register: u8, values: &[u8]) -> Result<(), PdBusError> {
        self.bus
            .write_registers(STUSB4500_ADDRESS, register, values, self.delay)
            .map_err(|_| PdBusError)
    }
}
