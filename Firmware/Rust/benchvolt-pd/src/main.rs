//! Firmware entry point: hardware bring-up followed by the foreground loop.
//! The loop below is deliberately a thin orchestration — every step's policy
//! lives in `loop_steps` (periodic services), `usb_intents` (USB command
//! execution), or `runtime` (dispatch and shutdown idioms), so reading the
//! `loop` block is a complete map of what one pass does and in what order.

#![no_main]
#![no_std]

mod arb_runtime;
mod board;
mod boot;
mod diagnostics;
mod display_dma;
mod input;
mod loop_steps;
mod reset_marker;
mod runtime;
mod types;
mod usb_intents;
mod usb_protocol;
mod usb_transport;
mod view;

use benchvolt_pd::app::{AppState, AwgStatus};
use benchvolt_pd::arb::UploadSession as ArbUploadSession;
use benchvolt_pd::cadence::ServiceCadence;
use benchvolt_pd::early_shutdown::raw_emergency_shutdown;
use benchvolt_pd::input_policy::{encoder_action, ButtonTracker};
use benchvolt_pd::measurement::MeasurementWindows;
use benchvolt_pd::monitoring::ProtectionService;
use benchvolt_pd::pd::Service as PdService;
use benchvolt_pd::power::{execute_global_shutdown, PowerExecutor};
use benchvolt_pd::reset_cause::ResetReason;
use benchvolt_pd::settings::{PersistentSettings, SettingsDebouncer};
use benchvolt_pd::usb_output::OutputTransaction;
use benchvolt_pd::waveform::Service as WaveformService;
use board::{
    adc::{AdcBank, BoundedAdc},
    i2c::{SoftI2c, SoftPdBus},
    power::HardwarePowerDriver,
};
use boot::{compact_settings_store, load_settings_store, persist_settings, SETTINGS_SLOTS};
use cortex_m_rt::{entry, exception, ExceptionFrame};
use display_interface_spi::SPIInterface;
use embedded_hal::digital::v2::InputPin;
use input::{monotonic_ms, take_encoder_adjustment};
use loop_steps::LoopState;
use mipidsi::{Builder, ColorInversion, ModelOptions, Orientation};
use reducto::EffectApp;
use runtime::{dispatch_app, service_profile_request};
use stm32f0xx_hal::{
    delay::Delay,
    pac,
    prelude::*,
    rcc::{HSEBypassMode, USBClockSource},
    spi::{Mode, Phase, Polarity, Spi},
};
use usb_intents::UsbCtx;
use view::BenchVoltView;

const FLASH_READY_SPINS: u32 = 12_000_000;

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

pub(crate) fn emergency_reset(reason: ResetReason) -> ! {
    cortex_m::interrupt::disable();
    unsafe { raw_emergency_shutdown() };
    unsafe { reset_marker::record(reason) };
    cortex_m::peripheral::SCB::sys_reset()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    emergency_reset(ResetReason::Panic)
}

#[exception]
unsafe fn HardFault(_frame: &ExceptionFrame) -> ! {
    emergency_reset(ResetReason::HardFault)
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

fn feed_watchdog() {
    unsafe { core::ptr::write_volatile(0x4000_3000 as *mut u32, 0xaaaa) };
}

// STM32F0 stalls instruction fetches from flash while the same bank is busy.
// Keep the bounded wait in the copied-to-RAM `.data` output section so its
// timeout and the watchdog remain meaningful during erase/program operations.
#[inline(never)]
#[link_section = ".data.flash_wait"]
#[no_mangle]
fn benchvolt_wait_for_flash_ready(status: *const u32) -> bool {
    for _ in 0..FLASH_READY_SPINS {
        if unsafe { core::ptr::read_volatile(status) } & 1 == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

fn benchvolt_display_offset(_: &ModelOptions) -> (u16, u16) {
    (0, 35)
}

#[entry]
fn main() -> ! {
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
    let mut adc_bank = AdcBank {
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
    let mut pd_bus =
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
    let mut power_driver = PowerExecutor::new(power_driver, monotonic_ms());
    let mut app: types::FirmwareApp = EffectApp::new(
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

    let mut cadence = ServiceCadence::default();
    let mut measurement_windows = MeasurementWindows::new();
    let mut protection = ProtectionService::default();
    let mut waveform_service = WaveformService::new();
    let mut arb_upload = ArbUploadSession::new();
    let mut settings_effect = SettingsDebouncer::new(PersistentSettings::from_state(app.state()));
    let mut pd_service = PdService::new(app.state().sink_current_limit_ma);
    let mut input_ticks = monotonic_ms();
    let mut service_tick = input_ticks;
    let mut button = ButtonTracker::new(encoder_sw.is_high().unwrap_or(true));
    let mut encoder_accumulator = benchvolt_pd::input_policy::EncoderAccumulator {
        last_tick: input_ticks,
        last_direction: 0,
        velocity: 0,
    };
    let mut usb_output = OutputTransaction::new();
    let mut ls = LoopState {
        pd_deferred_elapsed_ms: 0,
        comm_capable_checked: false,
        display_failure_handled: false,
        pd_list_failures: 0,
        pd_list_not_before: 0,
        was_on_pd_source: false,
        pending_awg_ack: None,
        last_waveform_tick: loop_steps::monotonic_awg_tick(),
    };

    loop {
        usb_intents::service_usb_command(
            &mut UsbCtx {
                app: &mut app,
                power: &mut power_driver,
                pd_bus: &mut pd_bus,
                pd_service: &mut pd_service,
                waveform: &mut waveform_service,
                arb_upload: &mut arb_upload,
                usb_output: &mut usb_output,
                pending_awg_ack: &mut ls.pending_awg_ack,
            },
            &protection,
        );

        loop_steps::render_step(&mut app, &mut power_driver, &mut ls);

        if app.state().awg_status != AwgStatus::Running {
            power_driver.delay_ms(1u8);
        }
        input_ticks = monotonic_ms();
        let elapsed_ms = input_ticks.wrapping_sub(service_tick);
        service_tick = input_ticks;
        let mut due = cadence.advance(elapsed_ms);

        // Hot mode: while a waveform runs, the 2 kHz sampler owns the loop
        // and every multi-hundred-microsecond periodic service is suspended
        // (see the README's waveform section for the full rationale).
        let awg_hot = app.state().awg_status == AwgStatus::Running;
        if awg_hot {
            due.temperature = false;
            due.display_measurement = false;
            due.measurement = false;
        }

        loop_steps::pd_step(
            &mut app,
            &mut power_driver,
            &mut pd_bus,
            &mut pd_service,
            &mut ls,
            elapsed_ms,
            awg_hot,
        );

        let (direction, accelerated) = take_encoder_adjustment(&mut encoder_accumulator);
        if !display_dma::has_failed() {
            if let Some(action) = encoder_action(app.state(), direction, accelerated) {
                dispatch_app(&mut app, &mut power_driver, action);
            }
        }
        let next_sw_high = encoder_sw.is_high().unwrap_or(button.is_high());
        if let Some(action) = button.sample(input_ticks, next_sw_high) {
            if !display_dma::has_failed() {
                dispatch_app(&mut app, &mut power_driver, action);
            }
        }

        if service_profile_request(&mut app, &mut power_driver, &mut settings_store) {
            // Fail-safe: factory defaults also restore the STUSB4500 NVM to
            // its canonical 20 V / request-max / comm-capable configuration,
            // recovering a unit profiled to an unusual PD voltage. Best
            // effort — the settings reset above stands either way — and the
            // NVM takes effect at the next cold attach (power replug).
            let _ = benchvolt_pd::pd::restore_canonical_nvm(&mut SoftPdBus::new(
                &mut pd_bus,
                power_driver.delay_mut(),
            ));
        }

        loop_steps::pd_source_list_step(
            &mut app,
            &mut power_driver,
            &mut pd_bus,
            &pd_service,
            &mut ls,
        );
        loop_steps::pdo_apply_step(
            &mut app,
            &mut power_driver,
            &mut pd_bus,
            &pd_service,
            &mut settings_store,
            &mut settings_effect,
        );
        loop_steps::waveform_step(&mut app, &mut power_driver, &mut waveform_service, &mut ls);
        loop_steps::awg_ack_step(&app, &mut waveform_service, &mut ls);

        if due.temperature {
            loop_steps::temperature_step(&mut app, &mut power_driver);
        }
        if due.measurement {
            loop_steps::measurement_step(
                &mut app,
                &mut power_driver,
                &mut protection,
                &mut adc_bank,
                &mut measurement_windows,
                &mut cadence,
                &mut due,
            );
        }
        if due.display_measurement {
            loop_steps::display_measurement_step(&mut app, &mut power_driver, &mut measurement_windows);
        }

        // Safety observations above preempt a power-up stage that becomes due
        // in this same pass. One bounded stage can then advance before the
        // watchdog is fed, without blocking USB, PD, or protection cadence.
        loop_steps::executor_step(&mut app, &mut power_driver, &mut usb_output);
        loop_steps::persistence_step(&app, &mut settings_effect, &mut settings_store, elapsed_ms);
        loop_steps::comm_capable_step(&app, &mut power_driver, &mut pd_bus, &cadence, &mut ls);

        if app.state().reboot_requested {
            // A physical reboot is safe only after every independent output-off
            // control has been attempted.
            let _ = execute_global_shutdown(&mut power_driver);
            unsafe { raw_emergency_shutdown() };
            cortex_m::asm::delay(480_000);
            emergency_reset(ResetReason::UserReboot);
        }
        display_dma::service();
        feed_watchdog();
    }
}
