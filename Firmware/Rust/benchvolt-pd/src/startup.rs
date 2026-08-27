//! Linear hardware bring-up: everything that runs exactly once before the
//! foreground loop, from the emergency-shutdown latch through USB install.
//! Returns the loop's owned resources as a [`Board`]; diverges on the
//! unrecoverable failures (unverified watchdog, dead ADC).

use benchvolt_pd::app::AppState;
use benchvolt_pd::early_shutdown::raw_emergency_shutdown;
use benchvolt_pd::power::PowerExecutor;
use benchvolt_pd::reset_cause::ResetReason;

use display_interface_spi::SPIInterface;
use mipidsi::{Builder, ColorInversion, ModelOptions, Orientation};
use reducto::EffectApp;
use stm32f0xx_hal::{
    delay::Delay,
    pac,
    prelude::*,
    rcc::{HSEBypassMode, USBClockSource},
    spi::{Mode, Phase, Polarity, Spi},
};

use crate::board::{
    adc::{AdcBank, BoundedAdc},
    i2c::SoftI2c,
    power::HardwarePowerDriver,
};
use crate::boot::{
    compact_settings_store, load_settings_store, persist_settings, SettingsStore, SETTINGS_SLOTS,
};
use crate::input::monotonic_ms;
use crate::types::{EncoderSwitch, FirmwareApp, FirmwarePower, PdI2c};
use crate::view::BenchVoltView;
use crate::{
    diagnostics, display_dma, emergency_reset, feed_watchdog, reset_marker, usb_transport,
    FLASH_READY_SPINS,
};

/// Everything the foreground loop owns after bring-up.
pub(crate) struct Board {
    pub app: FirmwareApp,
    pub power: FirmwarePower,
    pub pd_bus: PdI2c,
    pub adc_bank: AdcBank,
    pub settings_store: SettingsStore,
    pub encoder_sw: EncoderSwitch,
}

/// Make the raw shutdown path effective before watchdog and clock setup can
/// fail: clock the GPIO banks, latch every enable low, then select output mode.
unsafe fn prepare_emergency_shutdown() {
    use benchvolt_pd::early_shutdown::{output_modes, GPIO_CLOCK_ENABLE_MASK, PORTS, RCC_AHBENR};

    unsafe {
        let clocks = core::ptr::read_volatile(RCC_AHBENR);
        core::ptr::write_volatile(RCC_AHBENR, clocks | GPIO_CLOCK_ENABLE_MASK);
        // Readback provides the required peripheral-clock enable delay before
        // the first GPIO access.
        let _ = core::ptr::read_volatile(RCC_AHBENR);
    }
    for port in PORTS {
        let modes = output_modes(port.pin_mask);
        unsafe {
            core::ptr::write_volatile(port.bsrr, u32::from(port.pin_mask) << 16);
            let current = core::ptr::read_volatile(port.moder);
            core::ptr::write_volatile(port.moder, (current & !modes.clear) | modes.set);
        }
    }
}

fn start_watchdog() -> bool {
    const IWDG_BASE: usize = 0x4000_3000;
    const KR: *mut u32 = IWDG_BASE as *mut u32;
    const PR: *mut u32 = (IWDG_BASE + 0x04) as *mut u32;
    const RLR: *mut u32 = (IWDG_BASE + 0x08) as *mut u32;
    const SR: *const u32 = (IWDG_BASE + 0x0c) as *const u32;
    unsafe {
        reset_marker::record(ResetReason::WatchdogConfiguration);
        // 40 kHz LSI / 256 / (624 + 1) gives a nominal four-second timeout.
        core::ptr::write_volatile(KR, 0xcccc);
        core::ptr::write_volatile(KR, 0x5555);
        core::ptr::write_volatile(PR, 6);
        core::ptr::write_volatile(RLR, 624);
        let mut ready = false;
        for _ in 0..FLASH_READY_SPINS {
            if core::ptr::read_volatile(SR) & 0b11 == 0 {
                ready = true;
                break;
            }
        }
        if !ready
            || core::ptr::read_volatile(PR) & 0x07 != 6
            || core::ptr::read_volatile(RLR) & 0x0fff != 624
        {
            return false;
        }
        core::ptr::write_volatile(KR, 0xaaaa);
        reset_marker::clear();
    }
    true
}

fn benchvolt_display_offset(_: &ModelOptions) -> (u16, u16) {
    (0, 35)
}

pub(crate) fn initialize() -> Board {
    let mut dp = pac::Peripherals::take().unwrap();
    let mut cp = cortex_m::Peripherals::take().unwrap();

    // Establish a physical all-off state before any fallible boot operation.
    unsafe { prepare_emergency_shutdown() };

    // RCC reset flags are sticky and may overlap (for example PIN + POR).
    // Capture them before any initialization, then clear them for the next boot.
    let reset_causes = benchvolt_pd::reset_cause::decode_rcc_csr(dp.RCC.csr.read().bits());
    let reset_reason = unsafe {
        reset_marker::take(
            reset_causes,
            dp.FLASH.obr.read().ram_parity_check().is_disabled(),
        )
    };
    diagnostics::record_reset(reset_causes, reset_reason);
    dp.RCC.csr.modify(|_, w| w.rmvf().set_bit());

    // Start recovery supervision before boot-metadata flash access. Startup is
    // bounded and feeds explicitly; steady-state feeds only after a complete
    // foreground pass.
    if !start_watchdog() {
        cortex_m::interrupt::disable();
        unsafe { raw_emergency_shutdown() };
        // IWDG was already started and cannot be disabled. Do not continue
        // initialization with an unverified timeout; let it reset the device.
        loop {
            cortex_m::asm::nop();
        }
    }

    // USB has the highest urgency. Display DMA only advances a bounded transfer
    // phase and stays below USB; encoder capture is deliberately lowest.
    unsafe {
        cp.NVIC.set_priority(pac::Interrupt::USB, 0);
        cp.NVIC.set_priority(pac::Interrupt::DMA1_CH2_3, 128);
        cp.NVIC.set_priority(pac::Interrupt::EXTI4_15, 192);
    }

    feed_watchdog();

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

    // Dedicated free-running 2 kHz AWG clock. Keeping this separate preserves
    // the millisecond semantics of button holds, encoder acceleration, sensor
    // cadence, persistence debounce, and protection timing.
    unsafe {
        (*pac::RCC::ptr())
            .apb1enr
            .modify(|_, w| w.tim14en().set_bit());
    }
    dp.TIM14.psc.write(|w| w.psc().bits(23_999));
    dp.TIM14.arr.write(|w| w.arr().bits(u16::MAX));
    dp.TIM14.egr.write(|w| w.ug().set_bit());
    dp.TIM14.cr1.write(|w| w.cen().set_bit());
    let mut delay = Delay::new(cp.SYST, &rcc);

    let gpioa = dp.GPIOA.split(&mut rcc);
    let gpiob = dp.GPIOB.split(&mut rcc);
    let gpioc = dp.GPIOC.split(&mut rcc);
    let gpiod = dp.GPIOD.split(&mut rcc);

    // Set all power-control latches low before changing their pins to outputs.
    unsafe {
        (*pac::GPIOA::ptr()).bsrr.write(|w| w.bits(1 << (15 + 16)));
        (*pac::GPIOB::ptr()).bsrr.write(|w| {
            w.bits(
                (1 << (2 + 16))
                    | (1 << (6 + 16))
                    | (1 << (7 + 16))
                    | (1 << (8 + 16))
                    | (1 << (9 + 16))
                    | (1 << (15 + 16)),
            )
        });
        (*pac::GPIOC::ptr())
            .bsrr
            .write(|w| w.bits((1 << (12 + 16)) | (1 << (13 + 16))));
    }

    let (_en2, _en_dc2, _en4, _en5, _led_red, _led_blue, _en1, _en3, _en_dc1) =
        cortex_m::interrupt::free(|cs| {
            (
                gpioa.pa15.into_push_pull_output(cs),
                gpiob.pb2.into_push_pull_output(cs),
                gpiob.pb6.into_push_pull_output(cs),
                gpiob.pb7.into_push_pull_output(cs),
                gpiob.pb8.into_push_pull_output(cs),
                gpiob.pb9.into_push_pull_output(cs),
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
        ch1_current,
        ch2_current,
        ch3_current,
        ch4_current,
        ch5_current,
        ch1_voltage,
        ch2_voltage,
        ch3_voltage,
        ch4_voltage,
        ch5_voltage,
        sink_current,
        sink_voltage,
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
    let adc = match BoundedAdc::new(dp.ADC, &mut rcc) {
        Ok(adc) => adc,
        Err(()) => emergency_reset(ResetReason::AdcInitialization),
    };
    let adc_bank = AdcBank {
        adc,
        ch1_voltage,
        ch1_current,
        ch2_voltage,
        ch2_current,
        ch3_voltage,
        ch3_current,
        ch4_voltage,
        ch4_current,
        ch5_voltage,
        ch5_current,
        sink_voltage,
        sink_current,
    };

    let (sck, miso, mosi, dc, rst, cs, scl, sda, aux_scl, aux_sda, pd_scl, pd_sda, _pd_alert) =
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
                gpioa.pa8.into_open_drain_output(cs_token),
                gpioa.pa9.into_open_drain_output(cs_token),
                gpioa.pa10.into_floating_input(cs_token),
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
    let (interface, model, reset) = display.release();
    let (spi, dc, cs) = interface.release();
    let (spi, pins) = spi.release();
    display_dma::install((dp.DMA1, spi, pins, dc, cs, model, reset));
    feed_watchdog();
    let mut sensor = SoftI2c::new(scl, sda);
    let initial_temperature = sensor.read_tmp1075(&mut delay);
    let power_driver = HardwarePowerDriver::new(sensor, SoftI2c::new(aux_scl, aux_sda), delay);
    let pd_bus =
        SoftI2c::<_, _, { benchvolt_pd::pd::STUSB4500_I2C_HALF_CYCLE_US }>::new(pd_scl, pd_sda);
    let mut initial_state = AppState::new(true, initial_temperature);
    if let Some(record) = settings_store.latest {
        record.settings.apply_to(&mut initial_state);
        let pending_mv = record.settings.pdo_apply_pending_mv;
        if pending_mv != 0 {
            // A PDO apply hard-reset VBUS mid-interaction. Route straight to
            // the PD Source screen with the requested-vs-actual banner. This
            // path is display-only: it never re-attempts the apply or writes
            // the STUSB, and the flag clears after this single boot, so a
            // pathological charger converges to a normal boot instead of a
            // boot loop. If the clearing write fails the banner merely
            // sticks for another boot.
            initial_state.screen = benchvolt_pd::app::Screen::PdSource;
            initial_state.pd_source_stale = true;
            initial_state.pd_banner_mv = Some(pending_mv);
            let mut cleared = record.settings;
            cleared.pdo_apply_pending_mv = 0;
            let _ = persist_settings(&mut settings_store, cleared, true);
        }
    }
    initial_state.profile_present =
        core::array::from_fn(|index| settings_store.profiles[index].is_some());
    let power_driver = PowerExecutor::new(power_driver, monotonic_ms());
    let app: crate::types::FirmwareApp = EffectApp::new(
        BenchVoltView::new(display_dma::QueuedDisplay::new()),
        initial_state,
    );

    // USB transport is interrupt-owned so display and I2C work cannot starve it.
    usb_transport::install(dp.USB, gpioa.pa11, gpioa.pa12);
    feed_watchdog();
    unsafe { cortex_m::peripheral::NVIC::unmask(pac::Interrupt::USB) };
    unsafe { cortex_m::peripheral::NVIC::unmask(pac::Interrupt::EXTI4_15) };
    // The stock bootloader jumps with PRIMASK set after disabling all IRQs.
    // Re-enable the core only after the complete USB runtime is installed.
    unsafe { cortex_m::interrupt::enable() };
    cortex_m::peripheral::NVIC::pend(pac::Interrupt::USB);


    Board {
        app,
        power: power_driver,
        pd_bus,
        adc_bank,
        settings_store,
        encoder_sw,
    }
}
