use benchvolt_pd::app::Measurement;
use embedded_hal::adc::Channel;
use stm32f0xx_hal::{adc::Adc as HalAdc, gpio, pac, rcc::Rcc};

// At 48 MHz this permits roughly 500 us per hardware transition. A normal
// 71.5-cycle conversion completes far sooner, while a failed ADC cannot hold
// the protection loop forever.
const ADC_READY_SPINS: u32 = 24_000;
const SAMPLE_COUNT: u32 = 4;

pub(crate) struct BoundedAdc {
    _adc: pac::ADC,
}

impl BoundedAdc {
    pub(crate) fn new(adc: pac::ADC, _rcc: &mut Rcc) -> Result<Self, ()> {
        // Rcc is mutably borrowed by the caller so no HAL clock operation can
        // race this PAC access.
        let rcc = unsafe { &*pac::RCC::ptr() };
        rcc.apb2enr.modify(|_, w| w.adcen().enabled());
        rcc.cr2.modify(|_, w| w.hsi14on().on());
        if !wait_until(|| rcc.cr2.read().hsi14rdy().is_ready()) {
            return Err(());
        }

        let registers = unsafe { &*pac::ADC::ptr() };
        if !power_down(registers) {
            return Err(());
        }
        registers.cfgr1.modify(|_, w| w.dmaen().disabled());
        registers.cr.modify(|_, w| w.adcal().start_calibration());
        if !wait_until(|| registers.cr.read().adcal().is_not_calibrating()) {
            return Err(());
        }
        Ok(Self { _adc: adc })
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..ADC_READY_SPINS {
        if ready() {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn power_down(registers: &pac::adc::RegisterBlock) -> bool {
    if registers.cr.read().adstart().is_active() {
        registers.cr.modify(|_, w| w.adstp().stop_conversion());
        if !wait_until(|| registers.cr.read().adstp().is_not_stopping()) {
            return false;
        }
    }
    if registers.cr.read().aden().is_enabled() {
        registers.cr.modify(|_, w| w.addis().disable());
        if !wait_until(|| registers.cr.read().aden().is_disabled()) {
            return false;
        }
    }
    true
}

#[inline(never)]
fn read_raw_channel(_adc: &mut BoundedAdc, channel: u8) -> Option<u16> {
    // The HAL owns ADC, and the mutable Adc reference above proves exclusive
    // access. We use the PAC view because the HAL's OneShot implementation has
    // four unbounded busy waits.
    let registers = unsafe { &*pac::ADC::ptr() };
    if !power_down(registers) {
        return None;
    }

    if registers.isr.read().adrdy().is_ready() {
        registers.isr.write(|w| w.adrdy().clear());
    }
    registers.cr.modify(|_, w| w.aden().enabled());
    if !wait_until(|| registers.isr.read().adrdy().is_ready()) {
        let _ = power_down(registers);
        return None;
    }

    registers
        .chselr
        .write(|w| unsafe { w.bits(1_u32 << channel) });
    registers.smpr.write(|w| w.smp().cycles71_5());
    registers
        .cfgr1
        .modify(|_, w| w.res().twelve_bit().align().right());
    registers.cr.modify(|_, w| w.adstart().start_conversion());

    if !wait_until(|| registers.isr.read().eoc().is_complete()) {
        let _ = power_down(registers);
        return None;
    }
    let sample = registers.dr.read().bits() as u16;
    power_down(registers).then_some(sample)
}

fn read_adc_mv<P>(adc: &mut BoundedAdc, _pin: &mut P) -> Option<u16>
where
    P: Channel<HalAdc, ID = u8>,
{
    let channel = P::channel();
    // The sample capacitor retains the previous mux channel. Discard the first
    // conversion so a high-impedance divider cannot look like a current spike.
    read_raw_channel(adc, channel)?;
    let mut sum = 0u32;
    for _ in 0..SAMPLE_COUNT {
        sum += u32::from(read_raw_channel(adc, channel)?);
    }
    Some(((sum * 3_300 / SAMPLE_COUNT + 2_047) / 4_095) as u16)
}

pub(crate) fn read_channel_measurement<VP, IP>(
    adc: &mut BoundedAdc,
    voltage_pin: &mut VP,
    current_pin: &mut IP,
    voltage_scale_numerator: u16,
    voltage_scale_denominator: u16,
) -> Measurement
where
    VP: Channel<HalAdc, ID = u8>,
    IP: Channel<HalAdc, ID = u8>,
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

/// Every analog input the foreground loop samples, bundled with the bounded
/// ADC so the measurement pass is one call from the loop.
pub(crate) struct AdcBank {
    pub(crate) adc: BoundedAdc,
    pub(crate) ch1_voltage: gpio::gpioc::PC5<gpio::Analog>,
    pub(crate) ch1_current: gpio::gpioa::PA3<gpio::Analog>,
    pub(crate) ch2_voltage: gpio::gpioc::PC4<gpio::Analog>,
    pub(crate) ch2_current: gpio::gpioa::PA2<gpio::Analog>,
    pub(crate) ch3_voltage: gpio::gpioa::PA7<gpio::Analog>,
    pub(crate) ch3_current: gpio::gpioa::PA1<gpio::Analog>,
    pub(crate) ch4_voltage: gpio::gpiob::PB0<gpio::Analog>,
    pub(crate) ch4_current: gpio::gpioa::PA4<gpio::Analog>,
    pub(crate) ch5_voltage: gpio::gpiob::PB1<gpio::Analog>,
    pub(crate) ch5_current: gpio::gpioa::PA5<gpio::Analog>,
    pub(crate) sink_voltage: gpio::gpioc::PC0<gpio::Analog>,
    pub(crate) sink_current: gpio::gpioa::PA0<gpio::Analog>,
}

impl AdcBank {
    /// One protection-grade sample of all five outputs, with each channel's
    /// voltage-divider scale applied.
    pub(crate) fn read_outputs(&mut self) -> [Measurement; 5] {
        [
            read_channel_measurement(&mut self.adc, &mut self.ch1_voltage, &mut self.ch1_current, 1, 1),
            read_channel_measurement(&mut self.adc, &mut self.ch2_voltage, &mut self.ch2_current, 1, 1),
            read_channel_measurement(&mut self.adc, &mut self.ch3_voltage, &mut self.ch3_current, 1, 1),
            read_channel_measurement(&mut self.adc, &mut self.ch4_voltage, &mut self.ch4_current, 2, 1),
            read_channel_measurement(&mut self.adc, &mut self.ch5_voltage, &mut self.ch5_current, 78, 10),
        ]
    }

    pub(crate) fn read_sink(&mut self) -> Measurement {
        read_channel_measurement(&mut self.adc, &mut self.sink_voltage, &mut self.sink_current, 67, 10)
    }
}
