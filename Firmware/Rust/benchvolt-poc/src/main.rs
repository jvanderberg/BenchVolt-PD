#![no_main]
#![no_std]

mod view;

use core::{cell::RefCell, fmt::Write as _};

use benchvolt_poc::app::{Action, AppReducer, AppState, Measurement, RegulationMode};
use benchvolt_poc::power::{
    effect_for_transition, execute_effect, execute_global_shutdown, DriverOperation, PowerDriver,
    ProtectionMonitor, Rail,
};
use benchvolt_poc::settings::{
    decode as decode_settings, encode as encode_settings, PersistentSettings, SettingsDebouncer,
    SettingsRecord, RECORD_SIZE,
};
use cortex_m::interrupt::Mutex;
use cortex_m_rt::entry;
use display_interface_spi::SPIInterface;
use embedded_hal::{
    adc::{Channel, OneShot},
    blocking::delay::{DelayMs, DelayUs},
    digital::v2::{InputPin, OutputPin},
};
use heapless::{Deque, String};
use mipidsi::{Builder, ColorInversion, ModelOptions, Orientation};
use panic_halt as _;
use stm32_usbd::{MemoryAccess, UsbBus, UsbPeripheral};
use stm32f0xx_hal::{
    adc::{Adc, AdcSampleTime},
    delay::Delay,
    gpio::{
        gpioa::{PA11, PA12},
        Floating, Input,
    },
    pac::{self, interrupt},
    prelude::*,
    rcc::{HSEBypassMode, USBClockSource},
    spi::{Mode, Phase, Polarity, Spi},
};
use usb_device::device::StringDescriptors;
use usb_device::{bus::UsbBusAllocator, prelude::*};
use usbd_serial::{SerialPort, USB_CLASS_CDC};
use view::BenchVoltView;

const BOOT_METADATA_ADDR: usize = 0x0801_F800;
const SETTINGS_ADDR: usize = 0x0801_F000;
const FLASH_PAGE_SIZE: usize = 2_048;
const SETTINGS_SLOTS: usize = FLASH_PAGE_SIZE / RECORD_SIZE;
const USB_VID: u16 = 0x0483;
const USB_PID: u16 = 0x5740;
const OVERVIEW_HOLD_MS: u16 = 500;
const REBOOT_HOLD_MS: u16 = 3_000;
const ENCODER_ACCELERATION_IDLE_MS: u16 = 80;

type BenchUsbBus = UsbBus<BenchUsb>;

#[derive(Clone, Copy)]
struct UsbMessage {
    bytes: [u8; 64],
    len: u8,
}

impl UsbMessage {
    const fn empty() -> Self {
        Self {
            bytes: [0; 64],
            len: 0,
        }
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    fn from_slice(bytes: &[u8]) -> Self {
        let mut message = Self::empty();
        let len = bytes.len().min(message.bytes.len());
        message.bytes[..len].copy_from_slice(&bytes[..len]);
        message.len = len as u8;
        message
    }
}

struct UsbRuntime {
    device: UsbDevice<'static, BenchUsbBus>,
    serial: SerialPort<'static, BenchUsbBus>,
    rx_line: [u8; 64],
    rx_len: usize,
    commands: Deque<UsbMessage, 4>,
    responses: Deque<UsbMessage, 4>,
    response_offset: usize,
}

impl UsbRuntime {
    fn poll(&mut self) {
        if self.device.poll(&mut [&mut self.serial]) {
            let mut packet = [0u8; 64];
            while let Ok(count) = self.serial.read(&mut packet) {
                if count == 0 {
                    break;
                }
                for byte in &packet[..count] {
                    if *byte == b'\n' {
                        let mut message = UsbMessage::empty();
                        message.bytes[..self.rx_len].copy_from_slice(&self.rx_line[..self.rx_len]);
                        message.len = self.rx_len as u8;
                        if self.commands.push_back(message).is_err() {
                            self.responses
                                .push_back(UsbMessage::from_slice(b"ERR:BUSY\r\n"))
                                .ok();
                        }
                        self.rx_len = 0;
                    } else if self.rx_len < self.rx_line.len() {
                        self.rx_line[self.rx_len] = *byte;
                        self.rx_len += 1;
                    } else {
                        self.rx_len = 0;
                    }
                }
            }
        }

        if let Some(response) = self.responses.front().copied() {
            match self
                .serial
                .write(&response.as_slice()[self.response_offset..])
            {
                Ok(count) if count > 0 => {
                    self.response_offset += count;
                    if self.response_offset == usize::from(response.len) {
                        self.responses.pop_front();
                        self.response_offset = 0;
                    }
                }
                _ => {}
            }
        }
    }
}

static USB_RUNTIME: Mutex<RefCell<Option<UsbRuntime>>> = Mutex::new(RefCell::new(None));
static ENCODER_EVENTS: Mutex<RefCell<Deque<i8, 16>>> = Mutex::new(RefCell::new(Deque::new()));
static ENCODER_EDGE_COUNT: Mutex<RefCell<u32>> = Mutex::new(RefCell::new(0));
static ENCODER_DROP_COUNT: Mutex<RefCell<u32>> = Mutex::new(RefCell::new(0));

#[interrupt]
fn USB() {
    cortex_m::interrupt::free(|cs| {
        if let Some(runtime) = USB_RUNTIME.borrow(cs).borrow_mut().as_mut() {
            runtime.poll();
        }
    });
}

#[interrupt]
fn EXTI4_15() {
    let exti = unsafe { &*pac::EXTI::ptr() };
    if exti.pr.read().pr12().bit_is_set() {
        // Clear first so another edge can pend while this bounded ISR exits.
        exti.pr.write(|w| w.pr12().set_bit());
        let clockwise = unsafe { (*pac::GPIOB::ptr()).idr.read().idr13().bit_is_clear() };
        cortex_m::interrupt::free(|cs| {
            let pushed = ENCODER_EVENTS
                .borrow(cs)
                .borrow_mut()
                .push_back(if clockwise { 1 } else { -1 })
                .is_ok();
            let mut edges = ENCODER_EDGE_COUNT.borrow(cs).borrow_mut();
            *edges = edges.wrapping_add(1);
            if !pushed {
                let mut drops = ENCODER_DROP_COUNT.borrow(cs).borrow_mut();
                *drops = drops.wrapping_add(1);
            }
        });
    }
}

fn encoder_counts() -> (u32, u32) {
    cortex_m::interrupt::free(|cs| {
        (
            *ENCODER_EDGE_COUNT.borrow(cs).borrow(),
            *ENCODER_DROP_COUNT.borrow(cs).borrow(),
        )
    })
}

fn take_encoder_delta() -> i8 {
    cortex_m::interrupt::free(|cs| {
        let mut queue = ENCODER_EVENTS.borrow(cs).borrow_mut();
        let mut delta = 0i8;
        while let Some(direction) = queue.pop_front() {
            delta = delta.saturating_add(direction);
        }
        delta
    })
}

fn monotonic_ms() -> u16 {
    unsafe { (*pac::TIM3::ptr()).cnt.read().cnt().bits() }
}

fn take_usb_command() -> Option<UsbMessage> {
    cortex_m::interrupt::free(|cs| {
        USB_RUNTIME
            .borrow(cs)
            .borrow_mut()
            .as_mut()
            .and_then(|runtime| runtime.commands.pop_front())
    })
}

fn queue_usb_response(bytes: &[u8]) {
    let message = UsbMessage::from_slice(bytes);
    cortex_m::interrupt::free(|cs| {
        if let Some(runtime) = USB_RUNTIME.borrow(cs).borrow_mut().as_mut() {
            runtime.responses.push_back(message).ok();
        }
    });
    cortex_m::peripheral::NVIC::pend(pac::Interrupt::USB);
}

fn benchvolt_display_offset(_: &ModelOptions) -> (u16, u16) {
    (0, 35)
}

fn read_adc_mv<P>(adc: &mut Adc, pin: &mut P) -> Option<u16>
where
    P: Channel<Adc, ID = u8>,
{
    const SAMPLE_COUNT: u32 = 4;
    // The STM32F0 ADC sample capacitor retains the previous mux channel.
    // Discard the first conversion so a high-impedance voltage divider cannot
    // appear as a false current spike on the immediately following channel.
    let _: u16 = adc.read(pin).ok()?;
    let mut sum = 0u32;
    for _ in 0..SAMPLE_COUNT {
        let sample: u16 = match adc.read(pin) {
            Ok(value) => value,
            Err(_) => return None,
        };
        sum += u32::from(sample);
    }
    Some(((sum * 3_300 / SAMPLE_COUNT + 2_047) / 4_095) as u16)
}

fn read_channel_measurement<VP, IP>(
    adc: &mut Adc,
    voltage_pin: &mut VP,
    current_pin: &mut IP,
    voltage_scale_numerator: u16,
    voltage_scale_denominator: u16,
) -> Measurement
where
    VP: Channel<Adc, ID = u8>,
    IP: Channel<Adc, ID = u8>,
{
    match (read_adc_mv(adc, voltage_pin), read_adc_mv(adc, current_pin)) {
        (Some(input_mv), Some(current_input_mv)) => Measurement {
            millivolts: ((u32::from(input_mv) * u32::from(voltage_scale_numerator)
                / u32::from(voltage_scale_denominator))
            .min(u32::from(u16::MAX))) as u16,
            milliamps: u32::from(current_input_mv)
                .saturating_mul(2)
                .min(u32::from(u16::MAX)) as u16,
            valid: true,
        },
        _ => Measurement {
            millivolts: 0,
            milliamps: 0,
            valid: false,
        },
    }
}

#[derive(Clone, Copy)]
struct MeasurementAccumulator {
    millivolts: u32,
    milliamps: u32,
    samples: u8,
    valid: bool,
}

impl MeasurementAccumulator {
    const fn new() -> Self {
        Self {
            millivolts: 0,
            milliamps: 0,
            samples: 0,
            valid: true,
        }
    }

    fn push(&mut self, measurement: Measurement) {
        self.valid &= measurement.valid;
        self.millivolts += u32::from(measurement.millivolts);
        self.milliamps += u32::from(measurement.milliamps);
        self.samples = self.samples.saturating_add(1);
    }

    fn take(&mut self) -> Measurement {
        let result = if self.valid && self.samples > 0 {
            Measurement {
                millivolts: (self.millivolts / u32::from(self.samples)) as u16,
                milliamps: (self.milliamps / u32::from(self.samples)) as u16,
                valid: true,
            }
        } else {
            Measurement {
                millivolts: 0,
                milliamps: 0,
                valid: false,
            }
        };
        *self = Self::new();
        result
    }
}

struct BenchUsb {
    _usb: pac::USB,
    _dm: PA11<Input<Floating>>,
    _dp: PA12<Input<Floating>>,
}

unsafe impl Sync for BenchUsb {}

unsafe impl UsbPeripheral for BenchUsb {
    const REGISTERS: *const () = pac::USB::ptr() as *const ();
    const DP_PULL_UP_FEATURE: bool = true;
    const EP_MEMORY: *const () = 0x4000_6000 as *const ();
    const EP_MEMORY_SIZE: usize = 1024;
    const EP_MEMORY_ACCESS: MemoryAccess = MemoryAccess::Word16x2;

    fn enable() {
        let rcc = unsafe { &*pac::RCC::ptr() };
        cortex_m::interrupt::free(|_| {
            rcc.apb1enr.modify(|_, w| w.usben().set_bit());

            // The C bootloader jumps to the application with its internal D+
            // pull-up still enabled.  Hold it low long enough for the host to
            // observe a real disconnect before presenting new descriptors.
            let usb = unsafe { &*pac::USB::ptr() };
            usb.bcdr.modify(|_, w| w.dppu().clear_bit());
            cortex_m::asm::delay(960_000);

            rcc.apb1rstr.modify(|_, w| w.usbrst().set_bit());
            rcc.apb1rstr.modify(|_, w| w.usbrst().clear_bit());
        });
    }

    fn startup_delay() {
        cortex_m::asm::delay(72);
    }
}

struct SoftI2c<SCL, SDA> {
    scl: SCL,
    sda: SDA,
}

impl<SCL, SDA> SoftI2c<SCL, SDA>
where
    SCL: OutputPin,
    SDA: OutputPin + InputPin,
{
    fn new(mut scl: SCL, mut sda: SDA) -> Self {
        scl.set_high().ok();
        sda.set_high().ok();
        Self { scl, sda }
    }

    fn half_cycle(delay: &mut impl DelayUs<u32>) {
        delay.delay_us(2);
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

    fn read_tmp1075(&mut self, delay: &mut impl DelayUs<u32>) -> Option<i16> {
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

    fn write_register(
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

    fn read_register(
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

    fn write_bytes(
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
}

#[derive(Clone, Copy)]
enum HardwareError {
    Bus,
    Verify,
}

struct HardwarePowerDriver<B2SCL, B2SDA, B3SCL, B3SDA> {
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

    fn new(
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

    fn delay_ms(&mut self, milliseconds: u8) {
        self.delay.delay_ms(milliseconds);
    }

    fn read_temperature(&mut self) -> Option<i16> {
        self.shared_bus.read_tmp1075(&mut self.delay)
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
        unsafe {
            let bits = match port {
                'A' => (*pac::GPIOA::ptr()).odr.read().bits(),
                'B' => (*pac::GPIOB::ptr()).odr.read().bits(),
                _ => (*pac::GPIOC::ptr()).odr.read().bits(),
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

    fn tps_voltage_code(millivolts: u16) -> u16 {
        let reference_uv = u32::from(millivolts).saturating_mul(564) / 10;
        let delta_uv = reference_uv.saturating_sub(45_000);
        ((delta_uv.saturating_mul(10) + 2_822) / 5_645).min(0x07fe) as u16
    }

    fn tps_current_code(milliamps: u16) -> u8 {
        if milliamps == 0 {
            0
        } else {
            0x80 | ((milliamps / 50).min(127) as u8)
        }
    }

    fn tps_configure<SCL, SDA>(
        bus: &mut SoftI2c<SCL, SDA>,
        delay: &mut Delay,
        address: u8,
        millivolts: u16,
        current_limit_ma: u16,
        enable_output: bool,
    ) -> Result<(), HardwareError>
    where
        SCL: OutputPin,
        SDA: OutputPin + InputPin,
    {
        const REF_LSB: u8 = 0x00;
        const REF_MSB: u8 = 0x01;
        const IOUT_LIMIT: u8 = 0x02;
        const VOUT_FS: u8 = 0x04;
        const MODE: u8 = 0x06;

        let vout_fs = bus
            .read_register(address, VOUT_FS, delay)
            .map_err(|_| HardwareError::Bus)?;
        let vout_fs = (vout_fs & !(0x80 | 0x03)) | 0x03;
        let code = Self::tps_voltage_code(millivolts);
        let current_code = Self::tps_current_code(current_limit_ma);
        let mode = bus
            .read_register(address, MODE, delay)
            .map_err(|_| HardwareError::Bus)?;
        let mode = if enable_output {
            mode | 0x80
        } else {
            mode & !0x80
        };
        for (register, value) in [
            (VOUT_FS, vout_fs),
            (REF_LSB, code as u8),
            (REF_MSB, ((code >> 8) & 0x07) as u8),
            (IOUT_LIMIT, current_code),
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
        let next = if enabled { old | 0x80 } else { old & !0x80 };
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
        let code = Self::tps_voltage_code(millivolts);
        for (register, value) in [(0x00, code as u8), (0x01, ((code >> 8) & 0x07) as u8)] {
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
        if mode & 0x80 != 0 {
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
        match operation {
            DriverOperation::ChannelGate { channel, enabled } => {
                Self::channel_gate(channel, enabled)
            }
            DriverOperation::RailEnable { rail, enabled } => {
                Self::rail_enable(rail, enabled)?;
                if enabled {
                    self.delay.delay_ms(50u8);
                }
                Ok(())
            }
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
                    6_000,
                    true,
                )?;
                self.delay.delay_ms(50u8);
                Ok(())
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
                let millivolts = millivolts.clamp(500, 5_000);
                let code = 3_975u32
                    .saturating_sub((u32::from(millivolts - 500) * 3_635 + 2_250) / 4_500)
                    as u16;
                self.adjustable_bus
                    .write_bytes(
                        0x60,
                        &[((code >> 8) & 0x0f) as u8, code as u8],
                        &mut self.delay,
                    )
                    .map_err(|_| HardwareError::Bus)
            }
            DriverOperation::Ch5Enable(enabled) => {
                Self::write_gpio('B', 7, enabled);
                if Self::gpio_is_set('B', 7) != enabled {
                    return Err(HardwareError::Verify);
                }
                if enabled {
                    self.delay.delay_ms(50u8);
                }
                Ok(())
            }
            DriverOperation::ConfigureCh5 {
                millivolts,
                current_limit_ma,
            } => Self::tps_configure(
                &mut self.adjustable_bus,
                &mut self.delay,
                Self::TPS_CH5,
                millivolts,
                current_limit_ma,
                false,
            ),
            DriverOperation::Ch5OutputEnable(enabled) => {
                Self::tps_set_output(
                    &mut self.adjustable_bus,
                    &mut self.delay,
                    Self::TPS_CH5,
                    enabled,
                )?;
                if enabled {
                    self.delay.delay_ms(50u8);
                }
                Ok(())
            }
            DriverOperation::Ch5Voltage(millivolts) => Self::tps_set_voltage(
                &mut self.adjustable_bus,
                &mut self.delay,
                Self::TPS_CH5,
                millivolts,
            ),
            DriverOperation::VerifyOutput { channel, .. } if channel == 4 => {
                Self::tps_verify_output_enabled(
                    &mut self.adjustable_bus,
                    &mut self.delay,
                    Self::TPS_CH5,
                )
            }
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
        }
    }
}

#[derive(Clone, Copy)]
struct BootSeal {
    crc: u32,
    size: u32,
}

fn invalidate_boot_metadata() -> (bool, Option<BootSeal>) {
    const FLASH_BASE: usize = 0x4002_2000;
    const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
    const SR: *mut u32 = (FLASH_BASE + 0x0c) as *mut u32;
    const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
    const AR: *mut u32 = (FLASH_BASE + 0x14) as *mut u32;
    const SR_BSY: u32 = 1 << 0;
    const SR_ERRORS: u32 = (1 << 2) | (1 << 4);
    const SR_EOP: u32 = 1 << 5;
    const CR_PER: u32 = 1 << 1;
    const CR_STRT: u32 = 1 << 6;
    const CR_LOCK: u32 = 1 << 7;

    unsafe {
        let crc = core::ptr::read_volatile(BOOT_METADATA_ADDR as *const u32);
        let size = core::ptr::read_volatile((BOOT_METADATA_ADDR + 4) as *const u32);
        if crc == u32::MAX {
            return (true, None);
        }
        let seal = (size >= 192 && size <= (SETTINGS_ADDR - 0x0800_8000) as u32)
            .then_some(BootSeal { crc, size });
        while core::ptr::read_volatile(SR) & SR_BSY != 0 {}
        if core::ptr::read_volatile(CR) & CR_LOCK != 0 {
            core::ptr::write_volatile(KEYR, 0x4567_0123);
            core::ptr::write_volatile(KEYR, 0xcdef_89ab);
        }
        core::ptr::write_volatile(SR, SR_EOP | SR_ERRORS);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_PER);
        core::ptr::write_volatile(AR, BOOT_METADATA_ADDR as u32);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_STRT);
        while core::ptr::read_volatile(SR) & SR_BSY != 0 {}
        let ok = core::ptr::read_volatile(SR) & SR_ERRORS == 0
            && core::ptr::read_volatile(BOOT_METADATA_ADDR as *const u32) == u32::MAX;
        core::ptr::write_volatile(CR, (core::ptr::read_volatile(CR) & !CR_PER) | CR_LOCK);
        (ok, seal)
    }
}

fn restore_boot_seal(seal: BootSeal) -> bool {
    const FLASH_BASE: usize = 0x4002_2000;
    const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
    const SR: *mut u32 = (FLASH_BASE + 0x0c) as *mut u32;
    const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
    const SR_BSY: u32 = 1 << 0;
    const SR_ERRORS: u32 = (1 << 2) | (1 << 4);
    const SR_EOP: u32 = 1 << 5;
    const CR_PG: u32 = 1 << 0;
    const CR_LOCK: u32 = 1 << 7;
    if unsafe { core::ptr::read_volatile(BOOT_METADATA_ADDR as *const u32) } != u32::MAX {
        return false;
    }
    // Size is data; CRC at offset zero is the commit marker the bootloader
    // checks. Program CRC last so a torn seal remains invalid.
    let words = [
        (BOOT_METADATA_ADDR + 4, seal.size.to_le_bytes()),
        (BOOT_METADATA_ADDR, seal.crc.to_le_bytes()),
    ];
    unsafe {
        while core::ptr::read_volatile(SR) & SR_BSY != 0 {}
        if core::ptr::read_volatile(CR) & CR_LOCK != 0 {
            core::ptr::write_volatile(KEYR, 0x4567_0123);
            core::ptr::write_volatile(KEYR, 0xcdef_89ab);
        }
        core::ptr::write_volatile(SR, SR_EOP | SR_ERRORS);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_PG);
        for (address, source) in words {
            for half in 0..2 {
                let offset = half * 2;
                let value = u16::from_le_bytes([source[offset], source[offset + 1]]);
                core::ptr::write_volatile((address + offset) as *mut u16, value);
                while core::ptr::read_volatile(SR) & SR_BSY != 0 {}
                if core::ptr::read_volatile(SR) & SR_ERRORS != 0 {
                    core::ptr::write_volatile(
                        CR,
                        (core::ptr::read_volatile(CR) & !CR_PG) | CR_LOCK,
                    );
                    return false;
                }
            }
        }
        core::ptr::write_volatile(CR, (core::ptr::read_volatile(CR) & !CR_PG) | CR_LOCK);
        core::ptr::read_volatile(BOOT_METADATA_ADDR as *const u32) == seal.crc
            && core::ptr::read_volatile((BOOT_METADATA_ADDR + 4) as *const u32) == seal.size
    }
}

#[derive(Clone, Copy)]
struct SettingsStore {
    latest: Option<SettingsRecord>,
    next_slot: usize,
}

fn read_settings_slot(slot: usize) -> [u8; RECORD_SIZE] {
    let mut bytes = [0; RECORD_SIZE];
    for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe {
            core::ptr::read_volatile((SETTINGS_ADDR + slot * RECORD_SIZE + offset) as *const u8)
        };
    }
    bytes
}

fn load_settings_store() -> SettingsStore {
    let mut latest: Option<SettingsRecord> = None;
    let mut next_slot = SETTINGS_SLOTS;
    for slot in 0..SETTINGS_SLOTS {
        let bytes = read_settings_slot(slot);
        if bytes.iter().all(|byte| *byte == 0xff) {
            next_slot = next_slot.min(slot);
        } else if let Some(record) = decode_settings(&bytes) {
            if latest
                .map(|old| record.sequence > old.sequence)
                .unwrap_or(true)
            {
                latest = Some(record);
            }
        }
    }
    SettingsStore { latest, next_slot }
}

fn erase_flash_page(address: usize) -> bool {
    const FLASH_BASE: usize = 0x4002_2000;
    const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
    const SR: *mut u32 = (FLASH_BASE + 0x0c) as *mut u32;
    const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
    const AR: *mut u32 = (FLASH_BASE + 0x14) as *mut u32;
    const SR_BSY: u32 = 1 << 0;
    const SR_ERRORS: u32 = (1 << 2) | (1 << 4);
    const SR_EOP: u32 = 1 << 5;
    const CR_PER: u32 = 1 << 1;
    const CR_STRT: u32 = 1 << 6;
    const CR_LOCK: u32 = 1 << 7;
    unsafe {
        while core::ptr::read_volatile(SR) & SR_BSY != 0 {}
        if core::ptr::read_volatile(CR) & CR_LOCK != 0 {
            core::ptr::write_volatile(KEYR, 0x4567_0123);
            core::ptr::write_volatile(KEYR, 0xcdef_89ab);
        }
        core::ptr::write_volatile(SR, SR_EOP | SR_ERRORS);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_PER);
        core::ptr::write_volatile(AR, address as u32);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_STRT);
        while core::ptr::read_volatile(SR) & SR_BSY != 0 {}
        let ok = core::ptr::read_volatile(SR) & SR_ERRORS == 0
            && (0..FLASH_PAGE_SIZE)
                .all(|offset| core::ptr::read_volatile((address + offset) as *const u8) == 0xff);
        core::ptr::write_volatile(CR, (core::ptr::read_volatile(CR) & !CR_PER) | CR_LOCK);
        ok
    }
}

fn program_settings_slot(slot: usize, record: SettingsRecord) -> bool {
    if slot >= SETTINGS_SLOTS {
        return false;
    }
    let address = SETTINGS_ADDR + slot * RECORD_SIZE;
    if !(0..RECORD_SIZE)
        .all(|offset| unsafe { core::ptr::read_volatile((address + offset) as *const u8) == 0xff })
    {
        return false;
    }
    const FLASH_BASE: usize = 0x4002_2000;
    const KEYR: *mut u32 = (FLASH_BASE + 0x04) as *mut u32;
    const SR: *mut u32 = (FLASH_BASE + 0x0c) as *mut u32;
    const CR: *mut u32 = (FLASH_BASE + 0x10) as *mut u32;
    const SR_BSY: u32 = 1 << 0;
    const SR_ERRORS: u32 = (1 << 2) | (1 << 4);
    const SR_EOP: u32 = 1 << 5;
    const CR_PG: u32 = 1 << 0;
    const CR_LOCK: u32 = 1 << 7;
    let bytes = encode_settings(record);
    let mut ok = true;
    unsafe {
        while core::ptr::read_volatile(SR) & SR_BSY != 0 {}
        if core::ptr::read_volatile(CR) & CR_LOCK != 0 {
            core::ptr::write_volatile(KEYR, 0x4567_0123);
            core::ptr::write_volatile(KEYR, 0xcdef_89ab);
        }
        core::ptr::write_volatile(SR, SR_EOP | SR_ERRORS);
        core::ptr::write_volatile(CR, core::ptr::read_volatile(CR) | CR_PG);
        for offset in (0..RECORD_SIZE).step_by(2) {
            let value = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
            core::ptr::write_volatile((address + offset) as *mut u16, value);
            while core::ptr::read_volatile(SR) & SR_BSY != 0 {}
            if core::ptr::read_volatile(SR) & SR_ERRORS != 0 {
                ok = false;
                break;
            }
        }
        core::ptr::write_volatile(CR, (core::ptr::read_volatile(CR) & !CR_PG) | CR_LOCK);
    }
    ok && decode_settings(&read_settings_slot(slot)) == Some(record)
}

fn compact_settings_store(store: &mut SettingsStore) -> bool {
    let latest = store.latest;
    if !erase_flash_page(SETTINGS_ADDR) {
        return false;
    }
    store.next_slot = 0;
    if let Some(record) = latest {
        if !program_settings_slot(0, record) {
            return false;
        }
        store.next_slot = 1;
    }
    true
}

fn persist_settings(
    store: &mut SettingsStore,
    settings: PersistentSettings,
    outputs_physically_off: bool,
) -> bool {
    if store.next_slot >= SETTINGS_SLOTS {
        // Erasing a flash page briefly stalls instruction fetch on this MCU.
        // Never introduce that service gap while any power output is live.
        if !outputs_physically_off || !compact_settings_store(store) {
            return false;
        }
    }
    let record = SettingsRecord {
        sequence: store
            .latest
            .map(|record| record.sequence.wrapping_add(1))
            .unwrap_or(1),
        settings,
    };
    if !program_settings_slot(store.next_slot, record) {
        return false;
    }
    store.latest = Some(record);
    store.next_slot += 1;
    true
}

enum UsbIntent {
    None,
    JumpToBootloader,
    Reboot,
    SetOutput { channel: u8, enabled: bool },
    SetCurrentLimit { channel: u8, milliamps: u16 },
    SetRegulationMode { channel: u8, mode: RegulationMode },
    SetSinkCurrentLimit(u16),
}

fn parse_milliamps(text: &[u8]) -> Option<u16> {
    let (whole, fraction) = match text.iter().position(|byte| *byte == b'.') {
        Some(dot) => (&text[..dot], &text[dot + 1..]),
        None => (text, &[][..]),
    };
    if whole.is_empty()
        || whole.iter().any(|byte| !byte.is_ascii_digit())
        || fraction.len() > 3
        || fraction.iter().any(|byte| !byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.iter().try_fold(0u32, |value, byte| {
        value.checked_mul(10)?.checked_add(u32::from(*byte - b'0'))
    })?;
    let mut fractional = 0u32;
    for byte in fraction {
        fractional = fractional * 10 + u32::from(*byte - b'0');
    }
    for _ in fraction.len()..3 {
        fractional *= 10;
    }
    u16::try_from(whole.checked_mul(1_000)?.checked_add(fractional)?).ok()
}

fn handle_usb_command(
    command: &[u8],
    state: &AppState,
    protection_monitors: &[ProtectionMonitor; 5],
) -> UsbIntent {
    let command = command.strip_suffix(b"\r").unwrap_or(command);
    if let Some(rest) = command.strip_prefix(b"SYST:PROT:CH") {
        let Some(channel) = rest
            .first()
            .and_then(|byte| byte.checked_sub(b'1'))
            .filter(|channel| *channel < 5 && rest.get(1..) == Some(b"?"))
        else {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        };
        let snapshot = protection_monitors[usize::from(channel)].snapshot();
        let mut response: String<64> = String::new();
        write!(
            &mut response,
            "A{} R{},{} P{} G{} O{} V{} T{},{} N{}\r\n",
            u8::from(snapshot.active),
            snapshot.last.millivolts,
            snapshot.last.milliamps,
            snapshot.peak_milliamps,
            snapshot.grace_remaining,
            snapshot.overcurrent_samples,
            snapshot.voltage_samples,
            snapshot.trip.millivolts,
            snapshot.trip.milliamps,
            snapshot.samples_since_enable,
        )
        .ok();
        queue_usb_response(response.as_bytes());
        return UsbIntent::None;
    }
    if let Some(rest) = command.strip_prefix(b"OUTP:CH") {
        if rest.len() == 2 && rest[1] == b'?' {
            let Some(channel) = rest[0].checked_sub(b'1').filter(|channel| *channel < 5) else {
                queue_usb_response(b"ERR:RANGE\r\n");
                return UsbIntent::None;
            };
            let output = &state.channels[usize::from(channel)];
            let status = match output.fault {
                benchvolt_poc::app::Fault::OverCurrent => "FAULT:OVERCURRENT",
                benchvolt_poc::app::Fault::OverTemperature => "FAULT:OVERTEMP",
                benchvolt_poc::app::Fault::Sensor => "FAULT:SENSOR",
                benchvolt_poc::app::Fault::Hardware => "FAULT:HARDWARE",
                benchvolt_poc::app::Fault::None if output.physical_enabled => "ON",
                benchvolt_poc::app::Fault::None => "OFF",
            };
            let mut response: String<32> = String::new();
            write!(&mut response, "{}\r\n", status).ok();
            queue_usb_response(response.as_bytes());
            return UsbIntent::None;
        }
    }
    if let Some(rest) = command.strip_prefix(b"SOUR:CURR:CH") {
        let Some(channel) = rest.first().and_then(|byte| byte.checked_sub(b'1')) else {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        };
        if channel >= 5 {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        }
        if rest.get(1..) == Some(b"?") {
            let limit = state.channels[usize::from(channel)].current_limit_ma;
            let mut response: String<20> = String::new();
            write!(&mut response, "{}.{:03}A\r\n", limit / 1_000, limit % 1_000).ok();
            queue_usb_response(response.as_bytes());
            return UsbIntent::None;
        }
        let Some(value) = rest.get(2..).filter(|_| rest.get(1) == Some(&b' ')) else {
            queue_usb_response(b"ERR:SYNTAX\r\n");
            return UsbIntent::None;
        };
        let Some(milliamps) = parse_milliamps(value).filter(|value| *value <= 3_000) else {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        };
        return UsbIntent::SetCurrentLimit { channel, milliamps };
    }
    if let Some(value) = command.strip_prefix(b"SINK:LIMIT ") {
        let Some(milliamps) = parse_milliamps(value).filter(|value| *value <= 5_000) else {
            queue_usb_response(b"ERR:RANGE\r\n");
            return UsbIntent::None;
        };
        return UsbIntent::SetSinkCurrentLimit(milliamps);
    }
    match command {
        b"*IDN?" => queue_usb_response(b"BenchVolt-PD,RUST-POC,S/N:2026-01\r\n"),
        b"SYST:BUILD?" => queue_usb_response(b"Rust POC 0.1.0\r\n"),
        b"SYST:TICK?" => {
            let mut response: String<16> = String::new();
            write!(&mut response, "{}\r\n", monotonic_ms()).ok();
            queue_usb_response(response.as_bytes());
        }
        b"MEAS:TEMP?" => {
            let mut response: String<32> = String::new();
            if state.temp_valid {
                let raw = i32::from(state.temp_sixteenths_c);
                let hundredths = raw * 100 / 16;
                write!(
                    &mut response,
                    "{}.{:02}\r\n",
                    hundredths / 100,
                    hundredths.abs() % 100
                )
                .ok();
            } else {
                response.push_str("ERR:SENSOR\r\n").ok();
            }
            queue_usb_response(response.as_bytes());
        }
        b"MEAS:CH1?" | b"MEAS:CH2?" | b"MEAS:CH3?" | b"MEAS:CH4?" | b"MEAS:CH5?" => {
            let channel = usize::from(command[7] - b'1');
            let measurement = state.channels[channel].measurement;
            let mut response: String<40> = String::new();
            if measurement.valid {
                write!(
                    &mut response,
                    "{}.{:03}V,{}.{:03}A\r\n",
                    measurement.millivolts / 1_000,
                    measurement.millivolts % 1_000,
                    measurement.milliamps / 1_000,
                    measurement.milliamps % 1_000
                )
                .ok();
            } else {
                response.push_str("ERR:SENSOR\r\n").ok();
            }
            queue_usb_response(response.as_bytes());
        }
        b"MEAS:SINK?" => {
            let measurement = state.sink;
            let mut response: String<48> = String::new();
            if measurement.valid {
                let milliwatts = u32::from(measurement.millivolts)
                    .saturating_mul(u32::from(measurement.milliamps))
                    / 1_000;
                write!(
                    &mut response,
                    "{}.{:03}V,{}.{:03}A,{}.{:03}W\r\n",
                    measurement.millivolts / 1_000,
                    measurement.millivolts % 1_000,
                    measurement.milliamps / 1_000,
                    measurement.milliamps % 1_000,
                    milliwatts / 1_000,
                    milliwatts % 1_000
                )
                .ok();
            } else {
                response.push_str("ERR:SENSOR\r\n").ok();
            }
            queue_usb_response(response.as_bytes());
        }
        b"SINK:LIMIT?" => {
            let mut response: String<20> = String::new();
            write!(
                &mut response,
                "{}.{:03}A\r\n",
                state.sink_current_limit_ma / 1_000,
                state.sink_current_limit_ma % 1_000
            )
            .ok();
            queue_usb_response(response.as_bytes());
        }
        b"SOUR:MODE:CH4?" => {
            queue_usb_response(if state.channels[3].regulation_mode == RegulationMode::Cc {
                b"CC\r\n"
            } else {
                b"CV\r\n"
            })
        }
        b"SOUR:MODE:CH5?" => {
            queue_usb_response(if state.channels[4].regulation_mode == RegulationMode::Cc {
                b"CC\r\n"
            } else {
                b"CV\r\n"
            })
        }
        b"SOUR:MODE:CH4 CV" => {
            return UsbIntent::SetRegulationMode {
                channel: 3,
                mode: RegulationMode::Cv,
            }
        }
        b"SOUR:MODE:CH4 CC" => {
            return UsbIntent::SetRegulationMode {
                channel: 3,
                mode: RegulationMode::Cc,
            }
        }
        b"SOUR:MODE:CH5 CV" => {
            return UsbIntent::SetRegulationMode {
                channel: 4,
                mode: RegulationMode::Cv,
            }
        }
        b"SOUR:MODE:CH5 CC" => {
            return UsbIntent::SetRegulationMode {
                channel: 4,
                mode: RegulationMode::Cc,
            }
        }
        b"SYST:UI?" => {
            let (edges, drops) = encoder_counts();
            let mut response: String<64> = String::new();
            let focus = match state.focus {
                benchvolt_poc::app::ControlFocus::None => "NONE",
                benchvolt_poc::app::ControlFocus::OverviewOutput(_) => "OVOUT",
                benchvolt_poc::app::ControlFocus::Output => "OUT",
                benchvolt_poc::app::ControlFocus::Voltage => "VOLT",
                benchvolt_poc::app::ControlFocus::CurrentLimit => "CURR",
                benchvolt_poc::app::ControlFocus::RegulationMode => "MODE",
            };
            match state.screen {
                benchvolt_poc::app::Screen::Channel(channel) => {
                    let output = &state.channels[usize::from(channel)];
                    write!(
                        &mut response,
                        "CH{},{} V:{} I:{} E:{} D:{}\r\n",
                        channel + 1,
                        focus,
                        output.setpoint_mv,
                        output.current_limit_ma,
                        edges,
                        drops
                    )
                    .ok();
                }
                benchvolt_poc::app::Screen::Overview => {
                    write!(&mut response, "OVERVIEW E:{} D:{}\r\n", edges, drops).ok();
                }
                benchvolt_poc::app::Screen::UsbPdInput => {
                    write!(
                        &mut response,
                        "USBPD,{} I:{} E:{} D:{}\r\n",
                        focus, state.sink_current_limit_ma, edges, drops
                    )
                    .ok();
                }
            }
            queue_usb_response(response.as_bytes());
        }
        b"OUTP:CH1 ON" => {
            return UsbIntent::SetOutput {
                channel: 0,
                enabled: true,
            }
        }
        b"OUTP:CH1 OFF" => {
            return UsbIntent::SetOutput {
                channel: 0,
                enabled: false,
            }
        }
        b"OUTP:CH2 ON" => {
            return UsbIntent::SetOutput {
                channel: 1,
                enabled: true,
            }
        }
        b"OUTP:CH2 OFF" => {
            return UsbIntent::SetOutput {
                channel: 1,
                enabled: false,
            }
        }
        b"OUTP:CH3 ON" => {
            return UsbIntent::SetOutput {
                channel: 2,
                enabled: true,
            }
        }
        b"OUTP:CH3 OFF" => {
            return UsbIntent::SetOutput {
                channel: 2,
                enabled: false,
            }
        }
        b"OUTP:CH4 ON" => {
            return UsbIntent::SetOutput {
                channel: 3,
                enabled: true,
            }
        }
        b"OUTP:CH4 OFF" => {
            return UsbIntent::SetOutput {
                channel: 3,
                enabled: false,
            }
        }
        b"OUTP:CH5 ON" => {
            return UsbIntent::SetOutput {
                channel: 4,
                enabled: true,
            }
        }
        b"OUTP:CH5 OFF" => {
            return UsbIntent::SetOutput {
                channel: 4,
                enabled: false,
            }
        }
        b"JUMP:BOOTLOADER" => return UsbIntent::JumpToBootloader,
        b"SYST:REBOOT" => return UsbIntent::Reboot,
        _ => queue_usb_response(b"ERR:UNKNOWN_COMMAND\r\n"),
    }
    UsbIntent::None
}

#[entry]
fn main() -> ! {
    let mut dp = pac::Peripherals::take().unwrap();
    let mut cp = cortex_m::Peripherals::take().unwrap();

    // USB has the highest urgency. Encoder capture is deliberately lowest and
    // performs only a pending-bit clear, GPIO read, and bounded queue push.
    unsafe {
        cp.NVIC.set_priority(pac::Interrupt::USB, 0);
        cp.NVIC.set_priority(pac::Interrupt::EXTI4_15, 192);
    }

    // Arm reset recovery before clock, display, sensor, or USB initialization can fail.
    let (recovery_armed, boot_seal) = invalidate_boot_metadata();

    let mut rcc = dp
        .RCC
        .configure()
        .hse(8.mhz(), HSEBypassMode::NotBypassed)
        .sysclk(48.mhz())
        .hclk(48.mhz())
        .pclk(48.mhz())
        .usbsrc(USBClockSource::PLL)
        .freeze(&mut dp.FLASH);

    // Free-running 1 kHz TIM3 counter: button thresholds and encoder velocity
    // must use elapsed time, not foreground-loop iterations that vary with TFT work.
    unsafe {
        (*pac::RCC::ptr())
            .apb1enr
            .modify(|_, w| w.tim3en().set_bit());
    }
    dp.TIM3.psc.write(|w| w.psc().bits(47_999));
    dp.TIM3.arr.write(|w| w.arr().bits(u16::MAX));
    dp.TIM3.egr.write(|w| w.ug().set_bit());
    dp.TIM3.cr1.write(|w| w.cen().set_bit());
    let mut delay = Delay::new(cp.SYST, &rcc);

    let gpioa = dp.GPIOA.split(&mut rcc);
    let gpiob = dp.GPIOB.split(&mut rcc);
    let gpioc = dp.GPIOC.split(&mut rcc);
    let gpiod = dp.GPIOD.split(&mut rcc);

    // Set all power-control latches low before changing their pins to outputs.
    unsafe {
        (*pac::GPIOA::ptr()).bsrr.write(|w| w.bits(1 << (15 + 16)));
        (*pac::GPIOB::ptr()).bsrr.write(|w| {
            w.bits((1 << (2 + 16)) | (1 << (6 + 16)) | (1 << (7 + 16)) | (1 << (15 + 16)))
        });
        (*pac::GPIOC::ptr())
            .bsrr
            .write(|w| w.bits((1 << (12 + 16)) | (1 << (13 + 16))));
    }

    let (_en2, _en_dc2, _en4, _en5, _en1, _en3, _en_dc1) = cortex_m::interrupt::free(|cs| {
        (
            gpioa.pa15.into_push_pull_output(cs),
            gpiob.pb2.into_push_pull_output(cs),
            gpiob.pb6.into_push_pull_output(cs),
            gpiob.pb7.into_push_pull_output(cs),
            gpiob.pb15.into_push_pull_output(cs),
            gpioc.pc12.into_push_pull_output(cs),
            gpioc.pc13.into_push_pull_output(cs),
        )
    });
    let mut settings_store = load_settings_store();
    if settings_store.next_slot >= SETTINGS_SLOTS {
        let _ = compact_settings_store(&mut settings_store);
    }

    // Encoder inputs are polled in foreground code. No display or reducer work
    // runs in EXTI context, and rotation cannot change output state yet.
    let (_encoder_clk, _encoder_dt, encoder_sw) = cortex_m::interrupt::free(|cs| {
        (
            gpiob.pb12.into_floating_input(cs),
            gpiob.pb13.into_pull_up_input(cs),
            gpiob.pb14.into_floating_input(cs),
        )
    });

    unsafe {
        (*pac::RCC::ptr())
            .apb2enr
            .modify(|_, w| w.syscfgen().set_bit());
    }
    dp.SYSCFG.exticr4.modify(|_, w| w.exti12().pb12());
    dp.EXTI.imr.modify(|_, w| w.mr12().set_bit());
    dp.EXTI.rtsr.modify(|_, w| w.tr12().set_bit());
    dp.EXTI.ftsr.modify(|_, w| w.tr12().clear_bit());
    dp.EXTI.pr.write(|w| w.pr12().set_bit());

    let (
        mut ch1_current,
        mut ch2_current,
        mut ch3_current,
        mut ch4_current,
        mut ch5_current,
        mut ch1_voltage,
        mut ch2_voltage,
        mut ch3_voltage,
        mut ch4_voltage,
        mut ch5_voltage,
        mut sink_current,
        mut sink_voltage,
    ) = cortex_m::interrupt::free(|cs| {
        (
            gpioa.pa3.into_analog(cs),
            gpioa.pa2.into_analog(cs),
            gpioa.pa1.into_analog(cs),
            gpioa.pa4.into_analog(cs),
            gpioa.pa5.into_analog(cs),
            gpioc.pc5.into_analog(cs),
            gpioc.pc4.into_analog(cs),
            gpioa.pa7.into_analog(cs),
            gpiob.pb0.into_analog(cs),
            gpiob.pb1.into_analog(cs),
            gpioa.pa0.into_analog(cs),
            gpioc.pc0.into_analog(cs),
        )
    });
    let mut adc = Adc::new(dp.ADC, &mut rcc);
    adc.set_sample_time(AdcSampleTime::T_71);

    let (sck, miso, mosi, dc, rst, cs, scl, sda, aux_scl, aux_sda) =
        cortex_m::interrupt::free(|cs_token| {
            (
                gpiob.pb3.into_alternate_af0(cs_token),
                gpiob.pb4.into_alternate_af0(cs_token),
                gpiob.pb5.into_alternate_af0(cs_token),
                gpioc.pc10.into_push_pull_output(cs_token),
                gpioc.pc11.into_push_pull_output(cs_token),
                gpiod.pd2.into_push_pull_output(cs_token),
                gpioc.pc8.into_open_drain_output(cs_token),
                gpioc.pc9.into_open_drain_output(cs_token),
                gpioc.pc6.into_open_drain_output(cs_token),
                gpioc.pc7.into_open_drain_output(cs_token),
            )
        });

    const DISPLAY_MODE: Mode = Mode {
        polarity: Polarity::IdleHigh,
        phase: Phase::CaptureOnSecondTransition,
    };
    let spi = Spi::spi1(dp.SPI1, (sck, miso, mosi), DISPLAY_MODE, 24.mhz(), &mut rcc);
    let interface = SPIInterface::new(spi, dc, cs);
    let display = Builder::st7789(interface)
        .with_display_size(170, 320)
        .with_framebuffer_size(240, 320)
        .with_orientation(Orientation::Landscape(true))
        .with_window_offset_handler(benchvolt_display_offset)
        .with_invert_colors(ColorInversion::Inverted)
        .init(&mut delay, Some(rst))
        .unwrap();
    let mut sensor = SoftI2c::new(scl, sda);
    let initial_temperature = sensor.read_tmp1075(&mut delay);
    let mut power_driver = HardwarePowerDriver::new(sensor, SoftI2c::new(aux_scl, aux_sda), delay);
    let mut initial_state = AppState::new(recovery_armed, initial_temperature);
    if let Some(record) = settings_store.latest {
        record.settings.apply_to(&mut initial_state);
    }
    let mut app = reducto::App::<AppReducer, _, 8>::new(BenchVoltView::new(display), initial_state);
    app.render_full();

    // USB transport is interrupt-owned so display and I2C work cannot starve it.
    let usb = BenchUsb {
        _usb: dp.USB,
        _dm: gpioa.pa11,
        _dp: gpioa.pa12,
    };
    let usb_bus: &'static UsbBusAllocator<UsbBus<BenchUsb>> =
        cortex_m::singleton!(: UsbBusAllocator<UsbBus<BenchUsb>> = UsbBus::new(usb)).unwrap();
    let serial = SerialPort::new(usb_bus);
    let strings = [StringDescriptors::default()
        .manufacturer("BenchVolt-PD")
        .product("BenchVolt Rust POC")
        .serial_number("RUST-POC-01")];
    let usb_device = UsbDeviceBuilder::new(usb_bus, UsbVidPid(USB_VID, USB_PID))
        .strings(&strings)
        .unwrap()
        .device_class(USB_CLASS_CDC)
        .device_release(0x0200)
        .build();
    cortex_m::interrupt::free(|cs| {
        USB_RUNTIME.borrow(cs).replace(Some(UsbRuntime {
            device: usb_device,
            serial,
            rx_line: [0; 64],
            rx_len: 0,
            commands: Deque::new(),
            responses: Deque::new(),
            response_offset: 0,
        }));
    });
    unsafe { cortex_m::peripheral::NVIC::unmask(pac::Interrupt::USB) };
    unsafe { cortex_m::peripheral::NVIC::unmask(pac::Interrupt::EXTI4_15) };
    // The stock bootloader jumps with PRIMASK set after disabling all IRQs.
    // Re-enable the core only after the complete USB runtime is installed.
    unsafe { cortex_m::interrupt::enable() };
    cortex_m::peripheral::NVIC::pend(pac::Interrupt::USB);

    let mut temperature_ticks = 0u16;
    let mut measurement_ticks = 0u16;
    let mut display_measurement_ticks = 0u16;
    let mut channel_accumulators = [MeasurementAccumulator::new(); 5];
    let mut sink_accumulator = MeasurementAccumulator::new();
    let mut protection_monitors = [ProtectionMonitor::default(); 5];
    let mut settings_effect = SettingsDebouncer::new(PersistentSettings::from_state(app.state()));
    let mut input_ticks = monotonic_ms();
    let mut health_ticks = 0u32;
    let mut seal_attempted = false;
    let mut last_button_tick = 0u16;
    let mut button_press_tick = None;
    let mut overview_hold_fired = false;
    let mut encoder_sw_high = encoder_sw.is_high().unwrap_or(true);
    let mut last_encoder_tick = input_ticks;
    let mut last_encoder_direction = 0i8;
    let mut encoder_velocity = 0u8;

    macro_rules! dispatch_with_power_effects {
        ($action:expr) => {{
            let mut pending_effect = None;
            app.dispatch_with($action, |old, new| {
                pending_effect = effect_for_transition(old, new);
            });
            if let Some(effect) = pending_effect {
                let completion = execute_effect(&mut power_driver, app.state(), effect);
                app.dispatch(completion);
            }
        }};
    }

    loop {
        while let Some(command) = take_usb_command() {
            match handle_usb_command(command.as_slice(), app.state(), &protection_monitors) {
                UsbIntent::None => {}
                UsbIntent::JumpToBootloader => {
                    // A bootloader transition is also a global safety transition.
                    // Attempt every independent off control before resetting, even
                    // if one driver operation reports a failure.
                    let _ = execute_global_shutdown(&mut power_driver);
                    let _ = erase_flash_page(BOOT_METADATA_ADDR);
                    queue_usb_response(b"OK:JUMPING_TO_BOOTLOADER\r\n");
                    cortex_m::asm::delay(4_800_000);
                    cortex_m::peripheral::SCB::sys_reset();
                }
                UsbIntent::Reboot => {
                    app.dispatch(Action::RequestReboot);
                    queue_usb_response(b"OK:REBOOTING\r\n");
                }
                UsbIntent::SetOutput { channel, enabled } => {
                    dispatch_with_power_effects!(Action::SetOutputRequested { channel, enabled });
                    let output = &app.state().channels[usize::from(channel)];
                    if output.physical_enabled == enabled
                        && output.requested_enabled == enabled
                        && (!enabled || output.fault == benchvolt_poc::app::Fault::None)
                    {
                        queue_usb_response(b"OK\r\n");
                    } else {
                        let response = match output.fault {
                            benchvolt_poc::app::Fault::OverCurrent => {
                                b"ERR:OVERCURRENT\r\n" as &[u8]
                            }
                            benchvolt_poc::app::Fault::OverTemperature => b"ERR:OVERTEMP\r\n",
                            benchvolt_poc::app::Fault::Sensor => b"ERR:SENSOR\r\n",
                            _ => b"ERR:HARDWARE\r\n",
                        };
                        queue_usb_response(response);
                    }
                }
                UsbIntent::SetCurrentLimit { channel, milliamps } => {
                    dispatch_with_power_effects!(Action::SetCurrentLimit { channel, milliamps });
                    let output = &app.state().channels[usize::from(channel)];
                    if output.current_limit_ma == milliamps
                        && output.fault != benchvolt_poc::app::Fault::Hardware
                    {
                        queue_usb_response(b"OK\r\n");
                    } else {
                        queue_usb_response(b"ERR:HARDWARE\r\n");
                    }
                }
                UsbIntent::SetRegulationMode { channel, mode } => {
                    dispatch_with_power_effects!(Action::SetRegulationMode { channel, mode });
                    let output = &app.state().channels[usize::from(channel)];
                    if output.regulation_mode == mode
                        && output.fault != benchvolt_poc::app::Fault::Hardware
                    {
                        queue_usb_response(b"OK\r\n");
                    } else {
                        queue_usb_response(b"ERR:HARDWARE\r\n");
                    }
                }
                UsbIntent::SetSinkCurrentLimit(milliamps) => {
                    app.dispatch(Action::SetSinkCurrentLimit(milliamps));
                    if app.state().sink_current_limit_ma == milliamps {
                        queue_usb_response(b"OK\r\n");
                    } else {
                        queue_usb_response(b"ERR:RANGE\r\n");
                    }
                }
            }
        }

        power_driver.delay_ms(1u8);
        input_ticks = monotonic_ms();
        health_ticks = health_ticks.saturating_add(1);
        temperature_ticks = temperature_ticks.wrapping_add(1);
        measurement_ticks = measurement_ticks.wrapping_add(1);
        display_measurement_ticks = display_measurement_ticks.wrapping_add(1);

        let direction = take_encoder_delta();
        if direction != 0 {
            let sign = direction.signum();
            let elapsed = input_ticks.wrapping_sub(last_encoder_tick);
            if sign != last_encoder_direction || elapsed > ENCODER_ACCELERATION_IDLE_MS {
                encoder_velocity = direction.unsigned_abs().min(16);
            } else {
                encoder_velocity = encoder_velocity
                    .saturating_add(direction.unsigned_abs())
                    .min(16);
            }
            last_encoder_tick = input_ticks;
            last_encoder_direction = sign;
            match app.state().focus {
                benchvolt_poc::app::ControlFocus::None => app.dispatch(if direction < 0 {
                    Action::PreviousScreen
                } else {
                    Action::NextScreen
                }),
                benchvolt_poc::app::ControlFocus::Output => {
                    if let benchvolt_poc::app::Screen::Channel(channel) = app.state().screen {
                        dispatch_with_power_effects!(Action::ToggleOutputRequested { channel });
                    }
                    true
                }
                benchvolt_poc::app::ControlFocus::OverviewOutput(channel) => {
                    dispatch_with_power_effects!(Action::ToggleOutputRequested { channel });
                    true
                }
                _ => {
                    let multiplier = match encoder_velocity {
                        0 | 1 => 1,
                        2..=3 => 2,
                        4..=5 => 4,
                        6..=8 => 8,
                        _ => 16,
                    };
                    let accelerated = direction.saturating_mul(multiplier);
                    app.dispatch(Action::AdjustFocused(accelerated))
                }
            };
        }

        let next_sw_high = encoder_sw.is_high().unwrap_or(encoder_sw_high);
        if encoder_sw_high && !next_sw_high {
            if input_ticks.wrapping_sub(last_button_tick) >= 50 {
                button_press_tick = Some(input_ticks);
                overview_hold_fired = false;
            }
        }
        if !next_sw_high {
            if let Some(pressed_at) = button_press_tick {
                let held_ms = input_ticks.wrapping_sub(pressed_at);
                if held_ms >= REBOOT_HOLD_MS {
                    button_press_tick = None;
                    app.dispatch(Action::RequestReboot);
                } else if held_ms >= OVERVIEW_HOLD_MS && !overview_hold_fired {
                    overview_hold_fired = true;
                    app.dispatch(Action::GoOverview);
                }
            }
        } else if !encoder_sw_high && next_sw_high {
            if let Some(pressed_at) = button_press_tick.take() {
                let held_ms = input_ticks.wrapping_sub(pressed_at);
                last_button_tick = input_ticks;
                if held_ms >= OVERVIEW_HOLD_MS {
                    app.dispatch(Action::GoOverview);
                } else if held_ms >= 30 {
                    app.dispatch(Action::NextControl);
                }
            }
        }
        encoder_sw_high = next_sw_high;

        if temperature_ticks >= 100 {
            temperature_ticks = 0;
            let temperature = power_driver.read_temperature();
            app.dispatch(Action::Temperature(temperature));
            let fault = match temperature {
                Some(raw) if raw >= 75 * 16 => Some(benchvolt_poc::app::Fault::OverTemperature),
                None => Some(benchvolt_poc::app::Fault::Sensor),
                _ => None,
            };
            if let Some(fault) = fault {
                for channel in 0..5u8 {
                    let output = &app.state().channels[usize::from(channel)];
                    if output.requested_enabled || output.physical_enabled {
                        dispatch_with_power_effects!(Action::ProtectionTrip { channel, fault });
                    }
                }
                let _ = execute_global_shutdown(&mut power_driver);
            }
        }
        if measurement_ticks >= 20 {
            measurement_ticks = 0;
            let measurements = [
                read_channel_measurement(&mut adc, &mut ch1_voltage, &mut ch1_current, 1, 1),
                read_channel_measurement(&mut adc, &mut ch2_voltage, &mut ch2_current, 1, 1),
                read_channel_measurement(&mut adc, &mut ch3_voltage, &mut ch3_current, 1, 1),
                read_channel_measurement(&mut adc, &mut ch4_voltage, &mut ch4_current, 2, 1),
                read_channel_measurement(&mut adc, &mut ch5_voltage, &mut ch5_current, 78, 10),
            ];
            let sink_measurement =
                read_channel_measurement(&mut adc, &mut sink_voltage, &mut sink_current, 67, 10);
            for (accumulator, measurement) in channel_accumulators.iter_mut().zip(measurements) {
                accumulator.push(measurement);
            }
            sink_accumulator.push(sink_measurement);
            for channel in 0..5u8 {
                let output = &app.state().channels[usize::from(channel)];
                let measurement = measurements[usize::from(channel)];
                let fault = protection_monitors[usize::from(channel)].observe(output, measurement);
                if let Some(fault) = fault {
                    dispatch_with_power_effects!(Action::ProtectionTrip { channel, fault });
                }
            }
            for channel in 3..=4u8 {
                dispatch_with_power_effects!(Action::RegulateChannel {
                    channel,
                    measurement: measurements[usize::from(channel)],
                });
            }
        }
        if display_measurement_ticks >= 200 {
            display_measurement_ticks = 0;
            app.dispatch(Action::Measurements([
                channel_accumulators[0].take(),
                channel_accumulators[1].take(),
                channel_accumulators[2].take(),
                channel_accumulators[3].take(),
                channel_accumulators[4].take(),
            ]));
            app.dispatch(Action::SinkMeasurement(sink_accumulator.take()));
        }

        let current_settings = PersistentSettings::from_state(app.state());
        let outputs_stable = app
            .state()
            .channels
            .iter()
            .all(|channel| channel.transition == benchvolt_poc::app::OutputTransition::Stable);
        let outputs_physically_off = app
            .state()
            .channels
            .iter()
            .all(|channel| !channel.physical_enabled);
        if let Some(settings) = settings_effect.tick(current_settings, outputs_stable) {
            if persist_settings(&mut settings_store, settings, outputs_physically_off) {
                settings_effect.mark_saved(settings);
            }
        }

        if !seal_attempted && health_ticks >= 3_000 && app.state().temp_valid {
            seal_attempted = true;
            if let Some(seal) = boot_seal {
                let _ = restore_boot_seal(seal);
            }
        }
        if app.state().reboot_requested {
            // A physical reboot is safe only after every independent output-off
            // control has been attempted. If health sealing failed, reset still
            // lands in the stock bootloader instead of risking a boot loop.
            let _ = execute_global_shutdown(&mut power_driver);
            cortex_m::asm::delay(480_000);
            cortex_m::peripheral::SCB::sys_reset();
        }
    }
}
