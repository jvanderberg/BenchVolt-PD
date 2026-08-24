#![no_main]
#![no_std]

mod board;
mod boot;
mod input;
mod reset_marker;
mod runtime;
mod usb_protocol;
mod usb_transport;
mod view;

use core::{
    cell::RefCell,
    fmt::Write as _,
    sync::atomic::{AtomicU32, AtomicU8, Ordering},
};

use benchvolt_poc::app::{Action, AppReducer, AppState, AwgSource, AwgStatus};
use benchvolt_poc::arb::{Buffer as ArbBuffer, UploadSession as ArbUploadSession};
use benchvolt_poc::input_policy::{encoder_action, ButtonTracker};
use benchvolt_poc::measurement::MeasurementWindows;
use benchvolt_poc::monitoring::{ProtectionService, TpsStatusObservation};
use benchvolt_poc::pd::{Service as PdService, ServiceEvent as PdServiceEvent};
use benchvolt_poc::reset_cause::ResetReason;
use benchvolt_poc::power::{
    execute_global_shutdown, FirmwareEffectPlanner, Rail,
};
use benchvolt_poc::settings::{PersistentSettings, SettingsDebouncer};
use benchvolt_poc::waveform::{Directive as WaveformDirective, Service as WaveformService};
use board::{
    adc::{read_channel_measurement, BoundedAdc},
    i2c::{SoftI2c, SoftPdBus},
    power::HardwarePowerDriver,
};
use boot::{
    compact_settings_store, erase_flash_page, invalidate_boot_metadata, load_settings_store,
    persist_settings, restore_boot_seal, BOOT_METADATA_ADDR, SETTINGS_SLOTS,
};
use cortex_m::interrupt::Mutex;
use cortex_m_rt::{entry, exception, ExceptionFrame};
use display_interface_spi::SPIInterface;
use embedded_hal::digital::v2::InputPin;
use heapless::String;
use input::{monotonic_ms, take_encoder_adjustment};
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
use usb_protocol::{handle_usb_command, UsbIntent};
use usb_transport::{queue_usb_response, take_usb_command};
use view::BenchVoltView;

const FLASH_READY_SPINS: u32 = 12_000_000;

/// Turn off every independent output control without relying on initialized
/// drivers, interrupts, or either I2C bus.
unsafe fn raw_emergency_shutdown() {
    const GPIOA_BSRR: *mut u32 = 0x4800_0018 as *mut u32;
    const GPIOB_BSRR: *mut u32 = 0x4800_0418 as *mut u32;
    const GPIOC_BSRR: *mut u32 = 0x4800_0818 as *mut u32;
    core::ptr::write_volatile(GPIOA_BSRR, 1 << (15 + 16));
    core::ptr::write_volatile(
        GPIOB_BSRR,
        (1 << (2 + 16)) | (1 << (6 + 16)) | (1 << (7 + 16)) | (1 << (15 + 16)),
    );
    core::ptr::write_volatile(GPIOC_BSRR, (1 << (12 + 16)) | (1 << (13 + 16)));
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

static LAST_HW_OPERATION: AtomicU8 = AtomicU8::new(0);
static LAST_HW_ERROR: AtomicU8 = AtomicU8::new(0);
static RESET_CAUSES: AtomicU8 = AtomicU8::new(0);
static RESET_REASON: AtomicU8 = AtomicU8::new(0);
static HW_RETRY_COUNT: AtomicU32 = AtomicU32::new(0);
static CH5_TPS_STATUS: AtomicU8 = AtomicU8::new(0);
static ARB_INDEX: AtomicU32 = AtomicU32::new(0);
static ARB_CYCLES: AtomicU32 = AtomicU32::new(0);
static ARB_LATE_UPDATES: AtomicU32 = AtomicU32::new(0);
static ARB_SKIPPED_CYCLES: AtomicU32 = AtomicU32::new(0);
static ARB_BUFFER: Mutex<RefCell<ArbBuffer>> = Mutex::new(RefCell::new(ArbBuffer::new()));

fn record_hw_retries(count: u32) {
    let current = HW_RETRY_COUNT.load(Ordering::Relaxed);
    HW_RETRY_COUNT.store(current.saturating_add(count), Ordering::Relaxed);
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

    // RCC reset flags are sticky and may overlap (for example PIN + POR).
    // Capture them before any initialization, then clear them for the next boot.
    RESET_CAUSES.store(
        benchvolt_poc::reset_cause::decode_rcc_csr(dp.RCC.csr.read().bits()),
        Ordering::Relaxed,
    );
    let reset_reason = unsafe {
        reset_marker::take(
            RESET_CAUSES.load(Ordering::Relaxed),
            dp.FLASH.obr.read().ram_parity_check().is_disabled(),
        )
    };
    RESET_REASON.store(reset_reason.map_or(0, |reason| reason as u8), Ordering::Relaxed);
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

    // USB has the highest urgency. Encoder capture is deliberately lowest and
    // performs only a pending-bit clear, GPIO read, and bounded queue push.
    unsafe {
        cp.NVIC.set_priority(pac::Interrupt::USB, 0);
        cp.NVIC.set_priority(pac::Interrupt::EXTI4_15, 192);
    }

    // Arm reset recovery before clock, display, sensor, or USB initialization can fail.
    let (recovery_armed, boot_seal) = invalidate_boot_metadata();
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
    feed_watchdog();
    let mut sensor = SoftI2c::new(scl, sda);
    let initial_temperature = sensor.read_tmp1075(&mut delay);
    let mut power_driver = HardwarePowerDriver::new(sensor, SoftI2c::new(aux_scl, aux_sda), delay);
    let mut pd_bus = SoftI2c::<_, _, 1>::new(pd_scl, pd_sda);
    let mut initial_state = AppState::new(recovery_armed, initial_temperature);
    if let Some(record) = settings_store.latest {
        record.settings.apply_to(&mut initial_state);
    }
    initial_state.profile_present =
        core::array::from_fn(|index| settings_store.profiles[index].is_some());
    let mut app = EffectApp::<AppReducer, _, FirmwareEffectPlanner, 8>::new(
        BenchVoltView::new(display),
        initial_state,
    );
    app.render_full();

    // USB transport is interrupt-owned so display and I2C work cannot starve it.
    usb_transport::install(dp.USB, gpioa.pa11, gpioa.pa12);
    feed_watchdog();
    unsafe { cortex_m::peripheral::NVIC::unmask(pac::Interrupt::USB) };
    unsafe { cortex_m::peripheral::NVIC::unmask(pac::Interrupt::EXTI4_15) };
    // The stock bootloader jumps with PRIMASK set after disabling all IRQs.
    // Re-enable the core only after the complete USB runtime is installed.
    unsafe { cortex_m::interrupt::enable() };
    cortex_m::peripheral::NVIC::pend(pac::Interrupt::USB);

    let mut temperature_ticks = 0u16;
    let mut measurement_ticks = 0u16;
    let mut display_measurement_ticks = 0u16;
    let mut awg_load_ticks = 0u16;
    let mut measurement_windows = MeasurementWindows::new();
    let mut protection = ProtectionService::default();
    let mut waveform_service = WaveformService::new();
    let mut arb_upload = ArbUploadSession::new();
    let mut settings_effect = SettingsDebouncer::new(PersistentSettings::from_state(app.state()));
    let mut pd_service = PdService::new(app.state().sink_current_limit_ma);
    let mut input_ticks = monotonic_ms();
    let mut service_tick = input_ticks;
    let mut health_ticks = 0u32;
    let mut seal_attempted = false;
    let mut button = ButtonTracker::new(encoder_sw.is_high().unwrap_or(true));
    let mut last_encoder_tick = input_ticks;
    let mut last_encoder_direction = 0i8;
    let mut encoder_velocity = 0u8;

    loop {
        while let Some(command) = take_usb_command() {
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
                        dispatch_app(
                            &mut app,
                            &mut power_driver,
                            Action::GlobalShutdownFailed,
                        );
                        queue_usb_response(b"ERR:HARDWARE\r\n");
                        continue;
                    }
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::GlobalShutdownApplied,
                    );
                    if !erase_flash_page(BOOT_METADATA_ADDR) {
                        unsafe { raw_emergency_shutdown() };
                        queue_usb_response(b"ERR:FLASH\r\n");
                        continue;
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
                    if enabled
                        && !matches!(
                            app.state().awg_status,
                            AwgStatus::Stopped | AwgStatus::Fault
                        )
                    {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        continue;
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
                            dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                            queue_usb_response(b"ERR:HARDWARE\r\n");
                        }
                        continue;
                    }
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::SetOutputRequested { channel, enabled },
                    );
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
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::SetCurrentLimit { channel, milliamps },
                    );
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
                    if channel == app.state().active_awg_channel()
                        && !matches!(
                            app.state().awg_status,
                            AwgStatus::Stopped | AwgStatus::Fault
                        )
                    {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        continue;
                    }
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::SetRegulationMode { channel, mode },
                    );
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
                UsbIntent::ArbData(chunk) => {
                    if !matches!(
                        app.state().awg_status,
                        AwgStatus::Stopped | AwgStatus::Fault
                    ) {
                        queue_usb_response(b"ERR:BUSY\r\n");
                        continue;
                    }
                    if arb_upload.accept(chunk).is_err() {
                        queue_usb_response(b"ERR:SEQUENCE\r\n");
                        continue;
                    }
                    cortex_m::interrupt::free(|cs| {
                        ARB_BUFFER.borrow(cs).borrow_mut().write(chunk);
                    });
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
                        continue;
                    }
                    if !arb_upload.is_complete_for(start) {
                        queue_usb_response(b"ERR:INCOMPLETE\r\n");
                        continue;
                    }
                    let bounds = cortex_m::interrupt::free(|cs| {
                        ARB_BUFFER.borrow(cs).borrow().validate(start)
                    });
                    let Some((initial_mv, low_mv, high_mv)) = bounds else {
                        queue_usb_response(b"ERR:RANGE\r\n");
                        continue;
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
                        ARB_INDEX.store(0, Ordering::Relaxed);
                        ARB_CYCLES.store(0, Ordering::Relaxed);
                        ARB_LATE_UPDATES.store(0, Ordering::Relaxed);
                        ARB_SKIPPED_CYCLES.store(0, Ordering::Relaxed);
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
                            dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                            queue_usb_response(b"ERR:HARDWARE\r\n");
                        }
                    } else {
                        queue_usb_response(b"OK\r\n");
                    }
                }
            }
        }

        if app.state().awg_status != AwgStatus::Running {
            power_driver.delay_ms(1u8);
        }
        input_ticks = monotonic_ms();
        let elapsed_ms = input_ticks.wrapping_sub(service_tick);
        service_tick = input_ticks;
        health_ticks = health_ticks.saturating_add(u32::from(elapsed_ms));
        temperature_ticks = temperature_ticks.wrapping_add(elapsed_ms);
        measurement_ticks = measurement_ticks.wrapping_add(elapsed_ms);
        display_measurement_ticks = display_measurement_ticks.wrapping_add(elapsed_ms);
        awg_load_ticks = awg_load_ticks.wrapping_add(elapsed_ms);

        let outputs_off = app
            .state()
            .channels
            .iter()
            .all(|output| !output.requested_enabled && !output.physical_enabled);
        for event in pd_service
            .tick(
                elapsed_ms,
                input_ticks,
                outputs_off,
                app.state().sink_current_limit_ma,
                &mut SoftPdBus::new(&mut pd_bus, power_driver.delay_mut()),
            )
            .into_iter()
            .flatten()
        {
            let action = match event {
                PdServiceEvent::NegotiationStarted => Action::PdNegotiationStarted,
                PdServiceEvent::Pd(benchvolt_poc::pd::PdEvent::Negotiated(contract)) => {
                    Action::PdNegotiated(contract)
                }
                PdServiceEvent::Pd(benchvolt_poc::pd::PdEvent::Lost(error)) => {
                    Action::PdFailed(error)
                }
            };
            dispatch_app(&mut app, &mut power_driver, action);
        }

        let (direction, accelerated) = take_encoder_adjustment(
            &mut last_encoder_tick,
            &mut last_encoder_direction,
            &mut encoder_velocity,
        );
        if let Some(action) = encoder_action(app.state(), direction, accelerated) {
            dispatch_app(&mut app, &mut power_driver, action);
        }

        let next_sw_high = encoder_sw.is_high().unwrap_or(button.is_high());
        if let Some(action) = button.sample(input_ticks, next_sw_high) {
            dispatch_app(&mut app, &mut power_driver, action);
        }

        service_profile_request(&mut app, &mut power_driver, &mut settings_store);

        let waveform_status = app.state().awg_status;
        let waveform_source = app.state().awg_source;
        let waveform_config = app.state().awg;
        let waveform_tick = monotonic_awg_tick();
        let waveform_directive = if waveform_status == AwgStatus::Running
            && waveform_source == AwgSource::Arbitrary
        {
            cortex_m::interrupt::free(|cs| {
                let buffer = ARB_BUFFER.borrow(cs).borrow();
                waveform_service.tick(
                    waveform_status,
                    waveform_source,
                    waveform_config,
                    waveform_tick,
                    Some(&buffer),
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
        ARB_INDEX.store(u32::from(arb_status.index), Ordering::Relaxed);
        ARB_CYCLES.store(arb_status.cycles, Ordering::Relaxed);
        ARB_LATE_UPDATES.store(arb_status.late_updates, Ordering::Relaxed);
        ARB_SKIPPED_CYCLES.store(arb_status.skipped_cycles, Ordering::Relaxed);
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
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                }
            }
            WaveformDirective::Stop => {
                if execute_global_shutdown(&mut power_driver).is_ok() {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownApplied);
                } else {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                }
            }
            WaveformDirective::Finished | WaveformDirective::FailSafeShutdown => {
                if execute_global_shutdown(&mut power_driver).is_ok() {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownApplied);
                    dispatch_app(&mut app, &mut power_driver, Action::AwgStopped);
                } else {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                }
            }
            WaveformDirective::FaultShutdown => {
                let _ = execute_global_shutdown(&mut power_driver);
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

        if temperature_ticks >= 100 {
            temperature_ticks = 0;
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
        if measurement_ticks >= 20 {
            measurement_ticks = 0;
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
                        CH5_TPS_STATUS.store(status, Ordering::Relaxed);
                        TpsStatusObservation::Value(status)
                    }
                    Err(_) => {
                        CH5_TPS_STATUS.store(0xff, Ordering::Relaxed);
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
                awg_load_ticks = 0;
            }
            for channel in 0..5u8 {
                let measurement = measurements[usize::from(channel)];
                if let Some(action) =
                    protection.observe_channel(app.state(), channel, measurement)
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
        if display_measurement_ticks >= 200 {
            display_measurement_ticks = 0;
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
        if awg_load_ticks >= 1_000 {
            awg_load_ticks = 0;
            dispatch_app(
                &mut app,
                &mut power_driver,
                Action::AwgLoadMeasurement(measurement_windows.take_awg_load()),
            );
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
        if let Some(settings) = settings_effect.tick(current_settings, outputs_stable, elapsed_ms) {
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
            unsafe { raw_emergency_shutdown() };
            cortex_m::asm::delay(480_000);
            emergency_reset(ResetReason::UserReboot);
        }
        feed_watchdog();
    }
}
