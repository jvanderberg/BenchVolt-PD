#![no_main]
#![no_std]

mod arb_runtime;
mod board;
mod boot;
mod diagnostics;
mod display_dma;
mod input;
mod reset_marker;
mod runtime;
mod usb_protocol;
mod usb_transport;
mod view;

use core::fmt::Write as _;

use benchvolt_pd::app::{Action, AppReducer, AppState, AwgSource, AwgStatus};
use benchvolt_pd::arb::UploadSession as ArbUploadSession;
use benchvolt_pd::cadence::ServiceCadence;
use benchvolt_pd::input_policy::{encoder_action, ButtonTracker};
use benchvolt_pd::measurement::MeasurementWindows;
use benchvolt_pd::monitoring::{ProtectionService, TpsStatusObservation};
use benchvolt_pd::pd::{BootContractAction, Service as PdService, ServiceEvent as PdServiceEvent};
use benchvolt_pd::power::{execute_global_shutdown, FirmwareEffectPlanner, PowerExecutor, Rail};
use benchvolt_pd::reset_cause::ResetReason;
use benchvolt_pd::settings::{PersistentSettings, SettingsDebouncer};
use benchvolt_pd::usb_command::{
    output_completion_response, pd_completion_response, pd_diagnostics_response, UsbIntent,
};
use benchvolt_pd::usb_output::{Admission, OutputTransaction, RequestResult};
use benchvolt_pd::waveform::{Directive as WaveformDirective, Service as WaveformService};
use board::{
    adc::{read_channel_measurement, BoundedAdc},
    i2c::{SoftI2c, SoftPdBus},
    power::HardwarePowerDriver,
};
use boot::{
    compact_settings_store, erase_flash_page, load_settings_store, pd_attempt_pending,
    pd_renegotiation_allowed, persist_settings, record_pd_marker, BOOT_METADATA_ADDR,
    SETTINGS_SLOTS,
};
use cortex_m_rt::{entry, exception, ExceptionFrame};
use display_interface_spi::SPIInterface;
use embedded_hal::digital::v2::InputPin;
use heapless::String;
use input::{monotonic_ms, take_encoder_adjustment};
use mipidsi::{Builder, ColorInversion, ModelOptions, Orientation};
use reducto::EffectApp;
use runtime::{
    dispatch_app, service_profile_request, set_current_limit, set_regulation_mode, set_voltage,
};
use stm32f0xx_hal::{
    delay::Delay,
    pac,
    prelude::*,
    rcc::{HSEBypassMode, USBClockSource},
    spi::{Mode, Phase, Polarity, Spi},
};
use usb_protocol::handle_usb_command;
use usb_transport::{queue_usb_response, take_usb_command};
use view::BenchVoltView;

const FLASH_READY_SPINS: u32 = 12_000_000;

/// Turn off every independent output control without relying on initialized
/// drivers, interrupts, or either I2C bus.
unsafe fn raw_emergency_shutdown() {
    for port in benchvolt_pd::early_shutdown::PORTS {
        unsafe {
            core::ptr::write_volatile(port.bsrr, u32::from(port.pin_mask) << 16);
        }
    }
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

fn emergency_reset(reason: ResetReason) -> ! {
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

fn monotonic_awg_tick() -> u16 {
    unsafe { (*pac::TIM14::ptr()).cnt.read().cnt().bits() }
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
    let mut adc = match BoundedAdc::new(dp.ADC, &mut rcc) {
        Ok(adc) => adc,
        Err(()) => emergency_reset(ResetReason::AdcInitialization),
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
    }
    initial_state.profile_present =
        core::array::from_fn(|index| settings_store.profiles[index].is_some());
    let mut power_driver = PowerExecutor::new(power_driver, monotonic_ms());
    let mut app = EffectApp::<AppReducer, _, FirmwareEffectPlanner, 8>::new(
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
    let mut boot_pd_settled = false;
    let mut comm_capable_checked = false;
    let mut legacy_pd_requested_at = None;
    let mut button = ButtonTracker::new(encoder_sw.is_high().unwrap_or(true));
    let mut encoder_accumulator = benchvolt_pd::input_policy::EncoderAccumulator {
        last_tick: input_ticks,
        last_direction: 0,
        velocity: 0,
    };
    let mut usb_output = OutputTransaction::new();
    let mut display_failure_handled = false;

    loop {
        'usb_command: {
            let Some(command) = take_usb_command() else {
                break 'usb_command;
            };
            match handle_usb_command(
                command.as_slice(),
                app.state(),
                protection.channel_monitors(),
            ) {
                UsbIntent::None => {}
                UsbIntent::JumpToBootloader => {
                    // A bootloader transition is also a global safety transition.
                    // Attempt every independent off control before resetting, even
                    // if one driver operation reports a failure.
                    if execute_global_shutdown(&mut power_driver).is_err() {
                        unsafe { raw_emergency_shutdown() };
                        dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                        queue_usb_response(b"ERR:HARDWARE\r\n");
                        break 'usb_command;
                    }
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownApplied);
                    if !erase_flash_page(BOOT_METADATA_ADDR) {
                        unsafe { raw_emergency_shutdown() };
                        queue_usb_response(b"ERR:FLASH\r\n");
                        break 'usb_command;
                    }
                    queue_usb_response(b"OK:JUMPING_TO_BOOTLOADER\r\n");
                    unsafe { raw_emergency_shutdown() };
                    cortex_m::asm::delay(4_800_000);
                    emergency_reset(ResetReason::BootloaderRequest);
                }
                UsbIntent::Reboot => {
                    dispatch_app(&mut app, &mut power_driver, Action::RequestReboot);
                    queue_usb_response(b"OK:REBOOTING\r\n");
                }
                UsbIntent::SetOutput { channel, enabled } => {
                    if enabled && display_dma::has_failed() {
                        queue_usb_response(b"ERR:DISPLAY\r\n");
                        break 'usb_command;
                    }
                    match usb_output.begin_request(
                        channel,
                        enabled,
                        power_driver.is_busy(),
                        pd_service.command_pending(),
                    ) {
                        Admission::Proceed => {}
                        Admission::ProceedAfterCancellation => {
                            queue_usb_response(b"ERR:CANCELLED\r\n");
                        }
                        Admission::Busy => {
                            queue_usb_response(b"ERR:BUSY\r\n");
                            break 'usb_command;
                        }
                    }
                    if enabled
                        && !matches!(
                            app.state().awg_status,
                            AwgStatus::Stopped | AwgStatus::Fault
                        )
                    {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        break 'usb_command;
                    }
                    if !enabled
                        && channel == app.state().active_awg_channel()
                        && matches!(
                            app.state().awg_status,
                            AwgStatus::StartRequested
                                | AwgStatus::Starting
                                | AwgStatus::StopRequested
                        )
                    {
                        // The AWG start/stop sequence owns the channel for a
                        // bounded window; report busy instead of letting the
                        // reducer's guard surface a bogus ERR:HARDWARE.
                        queue_usb_response(b"ERR:BUSY\r\n");
                        break 'usb_command;
                    }
                    if !enabled
                        && channel == app.state().active_awg_channel()
                        && app.state().awg_status == AwgStatus::Running
                    {
                        if app.state().awg_source == AwgSource::Arbitrary {
                            waveform_service.cancel_arbitrary(channel);
                        }
                        if execute_global_shutdown(&mut power_driver).is_ok() {
                            dispatch_app(
                                &mut app,
                                &mut power_driver,
                                Action::GlobalShutdownApplied,
                            );
                            dispatch_app(&mut app, &mut power_driver, Action::AwgStopped);
                            queue_usb_response(b"OK\r\n");
                        } else {
                            unsafe { raw_emergency_shutdown() };
                            dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                            queue_usb_response(b"ERR:HARDWARE\r\n");
                        }
                        break 'usb_command;
                    }
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::SetOutputRequested { channel, enabled },
                    );
                    let output = &app.state().channels[usize::from(channel)];
                    if let RequestResult::Complete(result) =
                        usb_output.record_request(channel, enabled, output)
                    {
                        queue_usb_response(output_completion_response(result));
                    }
                }
                UsbIntent::SetCurrentLimit { channel, milliamps } => {
                    queue_usb_response(set_current_limit(
                        &mut app,
                        &mut power_driver,
                        channel,
                        milliamps,
                    ));
                }
                UsbIntent::SetVoltage {
                    channel,
                    millivolts,
                } => {
                    queue_usb_response(set_voltage(
                        &mut app,
                        &mut power_driver,
                        channel,
                        millivolts,
                    ));
                }
                UsbIntent::SetRegulationMode { channel, mode } => {
                    queue_usb_response(set_regulation_mode(
                        &mut app,
                        &mut power_driver,
                        channel,
                        mode,
                    ));
                }
                UsbIntent::SetSinkCurrentLimit(milliamps) => {
                    if pd_service.command_pending() {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        break 'usb_command;
                    }
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::SetSinkCurrentLimit(milliamps),
                    );
                    if app.state().sink_current_limit_ma == milliamps {
                        queue_usb_response(b"OK\r\n");
                    } else {
                        queue_usb_response(b"ERR:RANGE\r\n");
                    }
                }
                UsbIntent::PdDiagnostics => {
                    let result = benchvolt_pd::pd::read_diagnostics(&mut SoftPdBus::new(
                        &mut pd_bus,
                        power_driver.delay_mut(),
                    ));
                    match result {
                        Ok(snapshot) => {
                            let response = pd_diagnostics_response(snapshot);
                            queue_usb_response(response.as_bytes());
                        }
                        Err(_) => queue_usb_response(b"ERR:PD:BUS\r\n"),
                    }
                }
                UsbIntent::PdNegotiate => {
                    let outputs_off = app.state().outputs_inactive();
                    if pd_service.command_pending() || power_driver.is_busy() || !outputs_off {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        break 'usb_command;
                    }
                    let result = benchvolt_pd::pd::configure_request_source_current(
                        &mut SoftPdBus::new(&mut pd_bus, power_driver.delay_mut()),
                    );
                    match result {
                        Ok(benchvolt_pd::pd::NvmUpdate::Updated) => {
                            queue_usb_response(b"OK:PD:NVM_UPDATED:POWER_CYCLE\r\n")
                        }
                        Ok(benchvolt_pd::pd::NvmUpdate::AlreadyConfigured) => {
                            let result = benchvolt_pd::pd::request_legacy_boot_contract(
                                &mut SoftPdBus::new(&mut pd_bus, power_driver.delay_mut()),
                            );
                            queue_usb_response(pd_completion_response(result));
                        }
                        Err(_) => queue_usb_response(b"ERR:PD:NVM\r\n"),
                    }
                }
                UsbIntent::PdList => {
                    if pd_service.command_pending() {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        break 'usb_command;
                    }
                    let result = benchvolt_pd::pd::read_source_capabilities(&mut SoftPdBus::new(
                        &mut pd_bus,
                        power_driver.delay_mut(),
                    ));
                    // The desktop GUI collects lines between these markers and
                    // ignores any line that is not "index,mv,ma,mw".
                    let mut listing: String<176> = String::new();
                    listing.push_str("UI_PDO_LIST_START\r\n").ok();
                    match result {
                        Ok((raw_pdos, count)) => {
                            // Match the original C firmware: extract the fixed-PDO
                            // voltage/current fields from every object without
                            // filtering by supply type.
                            for (index, raw) in raw_pdos[..count].iter().enumerate() {
                                let millivolts = ((raw >> 10) & 0x3ff) * 50;
                                let milliamps = (raw & 0x3ff) * 10;
                                write!(
                                    &mut listing,
                                    "{},{},{},{}\r\n",
                                    index as u32,
                                    millivolts,
                                    milliamps,
                                    millivolts * milliamps / 1_000
                                )
                                .ok();
                            }
                        }
                        Err(_) => {
                            listing.push_str("ERR:PD:BUS\r\n").ok();
                        }
                    }
                    queue_usb_response(listing.as_bytes());
                    queue_usb_response(b"UI_PDO_LIST_END\r\n");
                }
                UsbIntent::PdoSet {
                    slot,
                    millivolts,
                    milliamps,
                } => {
                    let outputs_off = app.state().outputs_inactive();
                    if pd_service.command_pending() || power_driver.is_busy() || !outputs_off {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        break 'usb_command;
                    }
                    let result = benchvolt_pd::pd::set_sink_pdo(
                        &mut SoftPdBus::new(&mut pd_bus, power_driver.delay_mut()),
                        slot,
                        millivolts,
                        milliamps,
                    );
                    match result {
                        Ok(()) => queue_usb_response(b"OK:PD_PROFILED\r\n"),
                        Err(benchvolt_pd::pd::PdError::NoSuitablePdo) => {
                            queue_usb_response(b"ERR:PARAM_FORMAT\r\n")
                        }
                        Err(_) => queue_usb_response(b"ERR:PD_WRITE_FAILED\r\n"),
                    }
                }
                UsbIntent::ArbData(chunk) => {
                    if !matches!(
                        app.state().awg_status,
                        AwgStatus::Stopped | AwgStatus::Fault
                    ) {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        break 'usb_command;
                    }
                    if arb_upload.accept(chunk).is_err() {
                        queue_usb_response(b"ERR:SEQUENCE\r\n");
                        break 'usb_command;
                    }
                    arb_runtime::write(chunk);
                    let mut response: String<32> = String::new();
                    write!(&mut response, "OK:ACK:CH{}\r\n", chunk.channel + 1).ok();
                    queue_usb_response(response.as_bytes());
                }
                UsbIntent::ArbStart(start) => {
                    if !matches!(
                        app.state().awg_status,
                        AwgStatus::Stopped | AwgStatus::Fault
                    ) {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        break 'usb_command;
                    }
                    if !arb_upload.is_complete_for(start) {
                        queue_usb_response(b"ERR:INCOMPLETE\r\n");
                        break 'usb_command;
                    }
                    let bounds = arb_runtime::validate(start);
                    let Some((initial_mv, low_mv, high_mv)) = bounds else {
                        queue_usb_response(b"ERR:RANGE\r\n");
                        break 'usb_command;
                    };
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::RequestArbStart {
                            channel: start.channel,
                            initial_mv,
                            low_mv,
                            high_mv,
                        },
                    );
                    if app.state().awg_status == AwgStatus::StartRequested
                        && app.state().awg_source == AwgSource::Arbitrary
                    {
                        arb_runtime::reset_status();
                        // The buffer now belongs to this run; require a fresh
                        // contiguous upload before it can be started again.
                        arb_upload.invalidate();
                        waveform_service.arm_arbitrary(start);
                    } else {
                        queue_usb_response(b"ERR:BUSY\r\n");
                    }
                }
                UsbIntent::ArbStop(channel) => {
                    waveform_service.cancel_arbitrary(channel);
                    if app.state().awg_source == AwgSource::Arbitrary
                        && app.state().active_awg_channel() == channel
                        && !matches!(app.state().awg_status, AwgStatus::Stopped)
                    {
                        if execute_global_shutdown(&mut power_driver).is_ok() {
                            dispatch_app(
                                &mut app,
                                &mut power_driver,
                                Action::GlobalShutdownApplied,
                            );
                            dispatch_app(&mut app, &mut power_driver, Action::AwgStopped);
                            waveform_service.stop_arbitrary();
                            queue_usb_response(b"OK\r\n");
                        } else {
                            unsafe { raw_emergency_shutdown() };
                            dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                            queue_usb_response(b"ERR:HARDWARE\r\n");
                        }
                    } else {
                        queue_usb_response(b"OK\r\n");
                    }
                }
            }
        }

        // A display DMA failure during real rendering latches has_failed(),
        // which keeps boot fail-closed without requiring a serial cable.
        if display_dma::begin_full_render() {
            app.render_full();
            display_dma::finish_full_render();
        }
        if display_dma::has_failed() && !display_failure_handled {
            display_failure_handled = true;
            let _ = execute_global_shutdown(&mut power_driver);
            unsafe { raw_emergency_shutdown() };
            dispatch_app(
                &mut app,
                &mut power_driver,
                Action::BootRecoveryStatus(false),
            );
        }

        if app.state().awg_status != AwgStatus::Running {
            power_driver.delay_ms(1u8);
        }
        input_ticks = monotonic_ms();
        let elapsed_ms = input_ticks.wrapping_sub(service_tick);
        service_tick = input_ticks;
        let mut due = cadence.advance(elapsed_ms);

        let outputs_off = app.state().outputs_inactive();
        for event in pd_service
            .tick(
                elapsed_ms,
                input_ticks,
                outputs_off,
                app.state().sink_current_limit_ma,
                app.state()
                    .sink
                    .valid
                    .then_some(app.state().sink.millivolts),
                &mut SoftPdBus::new(&mut pd_bus, power_driver.delay_mut()),
            )
            .into_iter()
            .flatten()
        {
            let (action, pd_event) = match event {
                PdServiceEvent::NegotiationStarted => (Action::PdNegotiationStarted, None),
                PdServiceEvent::Pd(benchvolt_pd::pd::PdEvent::Negotiated(contract)) => (
                    Action::PdNegotiated(contract),
                    Some(benchvolt_pd::pd::PdEvent::Negotiated(contract)),
                ),
                PdServiceEvent::Pd(benchvolt_pd::pd::PdEvent::Lost(error)) => (
                    Action::PdFailed(error),
                    Some(benchvolt_pd::pd::PdEvent::Lost(error)),
                ),
            };
            dispatch_app(&mut app, &mut power_driver, action);
            if let Some(pd_event) = pd_event {
                if let Some(result) = pd_service.take_command_completion(pd_event) {
                    queue_usb_response(pd_completion_response(result));
                }
            }
        }

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

        service_profile_request(&mut app, &mut power_driver, &mut settings_store);

        let waveform_status = app.state().awg_status;
        let waveform_source = app.state().awg_source;
        let waveform_config = app.state().awg;
        let waveform_tick = monotonic_awg_tick();
        let waveform_directive =
            if waveform_status == AwgStatus::Running && waveform_source == AwgSource::Arbitrary {
                arb_runtime::with_buffer(|buffer| {
                    waveform_service.tick(
                        waveform_status,
                        waveform_source,
                        waveform_config,
                        waveform_tick,
                        Some(buffer),
                    )
                })
            } else {
                waveform_service.tick(
                    waveform_status,
                    waveform_source,
                    waveform_config,
                    waveform_tick,
                    None,
                )
            };
        let arb_status = waveform_service.arb_status();
        arb_runtime::update_status(arb_status);
        match waveform_directive {
            WaveformDirective::None => {}
            WaveformDirective::Sample(millivolts) => {
                dispatch_app(&mut app, &mut power_driver, Action::AwgSample(millivolts));
            }
            WaveformDirective::PrepareStart => {
                if execute_global_shutdown(&mut power_driver).is_ok() {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownApplied);
                    dispatch_app(&mut app, &mut power_driver, Action::AwgStartPrepared);
                } else {
                    unsafe { raw_emergency_shutdown() };
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                }
            }
            WaveformDirective::Stop => {
                if execute_global_shutdown(&mut power_driver).is_ok() {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownApplied);
                } else {
                    unsafe { raw_emergency_shutdown() };
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                }
            }
            WaveformDirective::Finished | WaveformDirective::FailSafeShutdown => {
                if execute_global_shutdown(&mut power_driver).is_ok() {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownApplied);
                    dispatch_app(&mut app, &mut power_driver, Action::AwgStopped);
                } else {
                    unsafe { raw_emergency_shutdown() };
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                }
            }
            WaveformDirective::FaultShutdown => {
                if execute_global_shutdown(&mut power_driver).is_err() {
                    unsafe { raw_emergency_shutdown() };
                }
            }
        }

        if let Some(start) = waveform_service.pending_arb_ack() {
            if app.state().awg_status == AwgStatus::Running
                && app.state().awg_source == AwgSource::Arbitrary
            {
                let mut response: String<64> = String::new();
                write!(
                    &mut response,
                    "OK:CH{}_ARB_STARTED_PTS:{}\r\n",
                    start.channel + 1,
                    start.count
                )
                .ok();
                queue_usb_response(response.as_bytes());
                waveform_service.take_pending_arb_ack();
            } else if matches!(
                app.state().awg_status,
                AwgStatus::Fault | AwgStatus::Stopped
            ) {
                queue_usb_response(b"ERR:HARDWARE\r\n");
                waveform_service.take_pending_arb_ack();
            }
        }

        if due.temperature {
            let temperature = power_driver.read_temperature();
            dispatch_app(
                &mut app,
                &mut power_driver,
                Action::Temperature(temperature),
            );
            let fault = ProtectionService::temperature_fault(temperature);
            if let Some(fault) = fault {
                for action in ProtectionService::temperature_trip_actions(app.state(), fault)
                    .into_iter()
                    .flatten()
                {
                    dispatch_app(&mut app, &mut power_driver, action);
                }
                let _ = execute_global_shutdown(&mut power_driver);
            }
        }
        if due.measurement {
            for (rail, channels) in [(Rail::Dc1, [0u8, 1]), (Rail::Dc2, [2u8, 3])] {
                let active = channels.into_iter().any(|channel| {
                    let output = &app.state().channels[usize::from(channel)];
                    output.requested_enabled || output.physical_enabled
                });
                let observation = if active {
                    match power_driver.read_rail_status(rail) {
                        Ok(status) => TpsStatusObservation::Value(status),
                        Err(_) => TpsStatusObservation::ReadError,
                    }
                } else {
                    TpsStatusObservation::Inactive
                };
                for action in protection
                    .observe_shared_status(app.state(), rail, observation)
                    .into_iter()
                    .flatten()
                {
                    dispatch_app(&mut app, &mut power_driver, action);
                }
            }
            let ch5_status = if app.state().channels[4].physical_enabled {
                match power_driver.read_ch5_status() {
                    Ok(status) => {
                        diagnostics::record_ch5_tps_status(status);
                        TpsStatusObservation::Value(status)
                    }
                    Err(_) => {
                        diagnostics::record_ch5_tps_status(0xff);
                        TpsStatusObservation::ReadError
                    }
                }
            } else {
                TpsStatusObservation::Inactive
            };
            if let Some(action) = protection.observe_ch5_status(app.state(), ch5_status) {
                dispatch_app(&mut app, &mut power_driver, action);
            }
            let measurements = [
                read_channel_measurement(&mut adc, &mut ch1_voltage, &mut ch1_current, 1, 1),
                read_channel_measurement(&mut adc, &mut ch2_voltage, &mut ch2_current, 1, 1),
                read_channel_measurement(&mut adc, &mut ch3_voltage, &mut ch3_current, 1, 1),
                read_channel_measurement(&mut adc, &mut ch4_voltage, &mut ch4_current, 2, 1),
                read_channel_measurement(&mut adc, &mut ch5_voltage, &mut ch5_current, 78, 10),
            ];
            let sink_measurement =
                read_channel_measurement(&mut adc, &mut sink_voltage, &mut sink_current, 67, 10);
            for rail in [Rail::Dc1, Rail::Dc2] {
                for action in protection
                    .observe_shared_current(app.state(), &measurements, rail)
                    .into_iter()
                    .flatten()
                {
                    dispatch_app(&mut app, &mut power_driver, action);
                }
            }
            if let Some(action) = protection.observe_sink(app.state(), sink_measurement) {
                dispatch_app(&mut app, &mut power_driver, action);
            }
            if !measurement_windows.record(app.state(), measurements, sink_measurement) {
                cadence.invalidate_awg_window(&mut due);
            }
            for channel in 0..5u8 {
                let measurement = measurements[usize::from(channel)];
                if let Some(action) = protection.observe_channel(app.state(), channel, measurement)
                {
                    dispatch_app(&mut app, &mut power_driver, action);
                }
            }
            for channel in 3..=4u8 {
                if app.state().awg_status != AwgStatus::Running
                    || channel != app.state().active_awg_channel()
                {
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::RegulateChannel {
                            channel,
                            measurement: measurements[usize::from(channel)],
                        },
                    );
                }
            }
        }
        if due.display_measurement {
            let (measurements, sink_measurement) = measurement_windows.take_display();
            dispatch_app(
                &mut app,
                &mut power_driver,
                Action::Measurements(measurements),
            );
            dispatch_app(
                &mut app,
                &mut power_driver,
                Action::SinkMeasurement(sink_measurement),
            );
        }
        if due.awg_load {
            dispatch_app(
                &mut app,
                &mut power_driver,
                Action::AwgLoadMeasurement(measurement_windows.take_awg_load()),
            );
        }

        // Safety observations above preempt a power-up stage that becomes due
        // in this same pass. One bounded stage can then advance before the
        // watchdog is fed, without blocking USB, PD, or protection cadence.
        if let Some(action) = power_driver.service(monotonic_ms(), app.state()) {
            dispatch_app(&mut app, &mut power_driver, action);
            let output_completion = usb_output.observe_completion(&action);
            if let Some(result) = output_completion {
                queue_usb_response(output_completion_response(result));
            }
        }
        if usb_output.cancel_if_idle(power_driver.is_busy()) {
            queue_usb_response(b"ERR:CANCELLED\r\n");
        }

        let current_settings = PersistentSettings::from_state(app.state());
        let outputs_stable = app.state().output_transitions_stable();
        let outputs_physically_off = app.state().outputs_physically_off();
        if let Some(settings) = settings_effect.tick(
            current_settings,
            outputs_stable,
            outputs_physically_off,
            elapsed_ms,
        ) {
            if persist_settings(&mut settings_store, settings, outputs_physically_off) {
                settings_effect.mark_saved(settings);
            }
        }

        if !boot_pd_settled
            && cadence.healthy_for(3_000)
            && app.state().temp_valid
            && outputs_physically_off
            && display_dma::ready_for_seal()
        {
            if !comm_capable_checked {
                // One-time STUSB4500 NVM check: declare USB data support in PD
                // requests so macOS keeps the port's data connection alive. A
                // bus error retries on the next boot; the update itself takes
                // effect at the next cold attach.
                comm_capable_checked = true;
                let _ = benchvolt_pd::pd::configure_usb_comm_capable(&mut SoftPdBus::new(
                    &mut pd_bus,
                    power_driver.delay_mut(),
                ));
            }
            match benchvolt_pd::pd::boot_contract_action(
                app.state().pd_contract.map(|contract| contract.millivolts),
                legacy_pd_requested_at,
                input_ticks,
            ) {
                BootContractAction::Wait => {}
                BootContractAction::Request => {
                    // The request can VBUS-reset the MCU. A flash marker
                    // written first turns any such reset into a skipped
                    // renegotiation on the next boot instead of a reset loop;
                    // if the marker cannot be written (or a previous attempt
                    // never settled), keep the current contract and let the
                    // settle window close out below.
                    legacy_pd_requested_at = Some(input_ticks);
                    if pd_renegotiation_allowed() && record_pd_marker() {
                        let _ = benchvolt_pd::pd::request_legacy_boot_contract(
                            &mut SoftPdBus::new(&mut pd_bus, power_driver.delay_mut()),
                        );
                    }
                }
                BootContractAction::Settle => {
                    boot_pd_settled = true;
                    if pd_attempt_pending() {
                        let _ = record_pd_marker();
                    }
                }
            }
        }
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
