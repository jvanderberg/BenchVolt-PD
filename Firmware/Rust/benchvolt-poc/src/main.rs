#![no_main]
#![no_std]

mod board;
mod boot;
mod usb_protocol;
mod usb_transport;
mod view;

use core::{
    cell::RefCell,
    fmt::Write as _,
    sync::atomic::{AtomicU32, AtomicU8, Ordering},
};

use benchvolt_poc::app::{
    Action, AppReducer, AppState, AwgSource, AwgStatus, ProfileRequest, ProfileStatus,
};
use benchvolt_poc::arb::{
    Buffer as ArbBuffer, Scheduler as ArbScheduler, Start as ArbStart, Tick as ArbTick,
    UploadSession as ArbUploadSession,
};
use benchvolt_poc::awg::Scheduler as AwgScheduler;
use benchvolt_poc::load::LoadAccumulator;
use benchvolt_poc::pd::{Negotiator as PdNegotiator, PdEvent};
use benchvolt_poc::power::{
    execute_effect, execute_global_shutdown, protection_output, tps55289_status_fault,
    FirmwareEffectPlanner, PowerDriver, ProtectionMonitor, Rail, SharedRailProtectionMonitor,
    SinkProtectionEvent, SinkProtectionMonitor, OVERTEMPERATURE_TRIP_SIXTEENTHS_C,
};
use benchvolt_poc::settings::{PersistentSettings, RecordKind, SettingsDebouncer};
use board::{
    adc::{read_channel_measurement, BoundedAdc, MeasurementAccumulator},
    i2c::{SoftI2c, SoftPdBus},
    power::HardwarePowerDriver,
};
use boot::{
    compact_settings_store, erase_flash_page, invalidate_boot_metadata, load_settings_store,
    persist_settings, persist_settings_record, restore_boot_seal, BOOT_METADATA_ADDR,
    SETTINGS_SLOTS,
};
use cortex_m::interrupt::Mutex;
use cortex_m_rt::{entry, exception, ExceptionFrame};
use display_interface_spi::SPIInterface;
use embedded_hal::digital::v2::InputPin;
use heapless::{Deque, String};
use mipidsi::{Builder, ColorInversion, ModelOptions, Orientation};
use reducto::EffectApp;
use stm32f0xx_hal::{
    delay::Delay,
    pac::{self, interrupt},
    prelude::*,
    rcc::{HSEBypassMode, USBClockSource},
    spi::{Mode, Phase, Polarity, Spi},
};
use usb_protocol::{handle_usb_command, UsbIntent};
use usb_transport::{queue_usb_response, take_usb_command};
use view::BenchVoltView;

const OVERVIEW_HOLD_MS: u16 = 500;
const REBOOT_HOLD_MS: u16 = 3_000;
const ENCODER_ACCELERATION_IDLE_MS: u16 = 80;
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

fn emergency_reset() -> ! {
    cortex_m::interrupt::disable();
    unsafe { raw_emergency_shutdown() };
    cortex_m::peripheral::SCB::sys_reset()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    emergency_reset()
}

#[exception]
unsafe fn HardFault(_frame: &ExceptionFrame) -> ! {
    emergency_reset()
}

fn start_watchdog() {
    const IWDG_BASE: usize = 0x4000_3000;
    const KR: *mut u32 = IWDG_BASE as *mut u32;
    const PR: *mut u32 = (IWDG_BASE + 0x04) as *mut u32;
    const RLR: *mut u32 = (IWDG_BASE + 0x08) as *mut u32;
    const SR: *const u32 = (IWDG_BASE + 0x0c) as *const u32;
    unsafe {
        // 40 kHz LSI / 256 / (624 + 1) gives a nominal four-second timeout.
        core::ptr::write_volatile(KR, 0xcccc);
        core::ptr::write_volatile(KR, 0x5555);
        core::ptr::write_volatile(PR, 6);
        core::ptr::write_volatile(RLR, 624);
        for _ in 0..FLASH_READY_SPINS {
            if core::ptr::read_volatile(SR) & 0b11 == 0 {
                break;
            }
        }
        core::ptr::write_volatile(KR, 0xaaaa);
    }
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

#[derive(Clone, Copy)]
struct EncoderEvent {
    direction: i8,
    tick: u16,
}

static ENCODER_EVENTS: Mutex<RefCell<Deque<EncoderEvent, 16>>> =
    Mutex::new(RefCell::new(Deque::new()));
static ENCODER_EDGE_COUNT: Mutex<RefCell<u32>> = Mutex::new(RefCell::new(0));
static ENCODER_DROP_COUNT: Mutex<RefCell<u32>> = Mutex::new(RefCell::new(0));


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
                .push_back(EncoderEvent {
                    direction: if clockwise { 1 } else { -1 },
                    tick: monotonic_ms(),
                })
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

fn take_encoder_adjustment(
    last_tick: &mut u16,
    last_direction: &mut i8,
    velocity: &mut u8,
) -> (i8, i8) {
    cortex_m::interrupt::free(|cs| {
        let mut queue = ENCODER_EVENTS.borrow(cs).borrow_mut();
        let mut raw = 0i16;
        let mut accelerated = 0i16;
        while let Some(event) = queue.pop_front() {
            let elapsed = event.tick.wrapping_sub(*last_tick);
            if event.direction != *last_direction || elapsed > ENCODER_ACCELERATION_IDLE_MS {
                *velocity = 1;
            } else {
                *velocity = velocity.saturating_add(1).min(16);
            }
            *last_tick = event.tick;
            *last_direction = event.direction;
            let multiplier: i16 = match *velocity {
                0 | 1 => 1,
                2..=3 => 2,
                4..=5 => 4,
                6..=8 => 8,
                _ => 16,
            };
            raw = raw.saturating_add(i16::from(event.direction));
            accelerated = accelerated.saturating_add(i16::from(event.direction) * multiplier);
        }
        (
            raw.clamp(i16::from(i8::MIN), i16::from(i8::MAX)) as i8,
            accelerated.clamp(i16::from(i8::MIN), i16::from(i8::MAX)) as i8,
        )
    })
}

fn monotonic_ms() -> u16 {
    unsafe { (*pac::TIM3::ptr()).cnt.read().cnt().bits() }
}

fn monotonic_awg_tick() -> u16 {
    unsafe { (*pac::TIM14::ptr()).cnt.read().cnt().bits() }
}


fn benchvolt_display_offset(_: &ModelOptions) -> (u16, u16) {
    (0, 35)
}





fn dispatch_app<V, D, const Q: usize>(
    app: &mut EffectApp<AppReducer, V, FirmwareEffectPlanner, Q>,
    power_driver: &mut D,
    action: Action,
) -> bool
where
    V: reducto::View<State = AppState>,
    D: PowerDriver,
{
    let mut pending_action = Some(action);
    let mut changed = false;
    while let Some(action) = pending_action.take() {
        let outcome = app.dispatch(action);
        changed |= outcome.changed();
        pending_action = match outcome.effect() {
            Some(effect) if effect.global_shutdown => {
                Some(if execute_global_shutdown(power_driver).is_ok() {
                    Action::GlobalShutdownApplied
                } else {
                    Action::GlobalShutdownFailed
                })
            }
            Some(effect) => effect
                .power
                .map(|power| execute_effect(power_driver, app.state(), power)),
            None => None,
        };
    }
    changed
}

#[entry]
fn main() -> ! {
    let mut dp = pac::Peripherals::take().unwrap();
    let mut cp = cortex_m::Peripherals::take().unwrap();

    // Start recovery supervision before boot-metadata flash access. Startup is
    // bounded and feeds explicitly; steady-state feeds only after a complete
    // foreground pass.
    start_watchdog();

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
        Err(()) => emergency_reset(),
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
    let mut pd_ticks = 0u16;
    let mut measurement_ticks = 0u16;
    let mut display_measurement_ticks = 0u16;
    let mut awg_load_ticks = 0u16;
    let mut channel_accumulators = [MeasurementAccumulator::new(); 5];
    let mut sink_accumulator = MeasurementAccumulator::new();
    let mut awg_load_accumulator = LoadAccumulator::new();
    let mut protection_monitors = [ProtectionMonitor::default(); 5];
    let mut shared_rail_protection_monitor = SharedRailProtectionMonitor::default();
    let mut sink_protection_monitor = SinkProtectionMonitor::default();
    // TPS STATUS is latched and read-to-clear. A single observation can be a
    // completed startup/current-regulation event; only a same-class
    // reassertion on the following poll represents a persistent fault.
    let mut ch5_pending_tps_fault = None;
    let mut shared_pending_tps_faults = [None; 2];
    let mut awg_scheduler = AwgScheduler::new();
    let mut arb_scheduler = ArbScheduler::new();
    let mut active_arb_start: Option<ArbStart> = None;
    let mut pending_arb_ack: Option<ArbStart> = None;
    let mut arb_upload = ArbUploadSession::new();
    let mut settings_effect = SettingsDebouncer::new(PersistentSettings::from_state(app.state()));
    let mut pd_negotiator = PdNegotiator::new(app.state().sink_current_limit_ma);
    let mut input_ticks = monotonic_ms();
    let mut service_tick = input_ticks;
    let mut health_ticks = 0u32;
    let mut seal_attempted = false;
    let mut last_button_tick = 0u16;
    let mut button_press_tick = None;
    let mut overview_hold_fired = false;
    let mut encoder_sw_high = encoder_sw.is_high().unwrap_or(true);
    let mut last_encoder_tick = input_ticks;
    let mut last_encoder_direction = 0i8;
    let mut encoder_velocity = 0u8;

    loop {
        while let Some(command) = take_usb_command() {
            match handle_usb_command(command.as_slice(), app.state(), &protection_monitors) {
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
                    cortex_m::peripheral::SCB::sys_reset();
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
                        arb_scheduler.stop();
                        ARB_INDEX.store(0, Ordering::Relaxed);
                        ARB_CYCLES.store(0, Ordering::Relaxed);
                        ARB_LATE_UPDATES.store(0, Ordering::Relaxed);
                        ARB_SKIPPED_CYCLES.store(0, Ordering::Relaxed);
                        active_arb_start = Some(start);
                        pending_arb_ack = Some(start);
                    } else {
                        queue_usb_response(b"ERR:BUSY\r\n");
                    }
                }
                UsbIntent::ArbStop(channel) => {
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
                            active_arb_start = None;
                            arb_scheduler.stop();
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
        pd_ticks = pd_ticks.wrapping_add(elapsed_ms);
        measurement_ticks = measurement_ticks.wrapping_add(elapsed_ms);
        display_measurement_ticks = display_measurement_ticks.wrapping_add(elapsed_ms);
        awg_load_ticks = awg_load_ticks.wrapping_add(elapsed_ms);

        if pd_ticks >= 20 {
            pd_ticks = 0;
            let outputs_off = app
                .state()
                .channels
                .iter()
                .all(|output| !output.requested_enabled && !output.physical_enabled);
            if outputs_off && pd_negotiator.current_cap_ma() != app.state().sink_current_limit_ma {
                dispatch_app(&mut app, &mut power_driver, Action::PdNegotiationStarted);
                pd_negotiator.restart(app.state().sink_current_limit_ma);
            }
            let event = pd_negotiator.step(
                &mut SoftPdBus::new(&mut pd_bus, power_driver.delay_mut()),
                input_ticks,
            );
            if let Some(event) = event {
                let action = match event {
                    PdEvent::Negotiated(contract) => Action::PdNegotiated(contract),
                    PdEvent::Lost(error) => Action::PdFailed(error),
                };
                dispatch_app(&mut app, &mut power_driver, action);
            }
        }

        let (direction, accelerated) = take_encoder_adjustment(
            &mut last_encoder_tick,
            &mut last_encoder_direction,
            &mut encoder_velocity,
        );
        if direction != 0 {
            match app.state().focus {
                benchvolt_poc::app::ControlFocus::None => {
                    let action = if app.state().screen == benchvolt_poc::app::Screen::Awg
                        && app.state().awg_editing
                    {
                        Action::AdjustAwg(accelerated)
                    } else if matches!(
                        app.state().screen,
                        benchvolt_poc::app::Screen::MainMenu
                            | benchvolt_poc::app::Screen::Awg
                            | benchvolt_poc::app::Screen::Settings
                            | benchvolt_poc::app::Screen::ProfileSave
                            | benchvolt_poc::app::Screen::ProfileLoad
                            | benchvolt_poc::app::Screen::System
                            | benchvolt_poc::app::Screen::Help
                    ) {
                        Action::NavigateMenu(direction)
                    } else if direction < 0 {
                        Action::PreviousScreen
                    } else {
                        Action::NextScreen
                    };
                    dispatch_app(&mut app, &mut power_driver, action)
                }
                benchvolt_poc::app::ControlFocus::Output => {
                    if let benchvolt_poc::app::Screen::Channel(channel) = app.state().screen {
                        dispatch_app(
                            &mut app,
                            &mut power_driver,
                            Action::ToggleOutputRequested { channel },
                        );
                    }
                    true
                }
                benchvolt_poc::app::ControlFocus::OverviewOutput(channel) => {
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::ToggleOutputRequested { channel },
                    );
                    true
                }
                _ => dispatch_app(
                    &mut app,
                    &mut power_driver,
                    Action::AdjustFocused(accelerated),
                ),
            };
        }

        let next_sw_high = encoder_sw.is_high().unwrap_or(encoder_sw_high);
        if encoder_sw_high && !next_sw_high && input_ticks.wrapping_sub(last_button_tick) >= 50 {
            button_press_tick = Some(input_ticks);
            overview_hold_fired = false;
        }
        if !next_sw_high {
            if let Some(pressed_at) = button_press_tick {
                let held_ms = input_ticks.wrapping_sub(pressed_at);
                if held_ms >= REBOOT_HOLD_MS {
                    button_press_tick = None;
                    dispatch_app(&mut app, &mut power_driver, Action::RequestReboot);
                } else if held_ms >= OVERVIEW_HOLD_MS && !overview_hold_fired {
                    overview_hold_fired = true;
                    dispatch_app(&mut app, &mut power_driver, Action::GoMainMenu);
                }
            }
        } else if !encoder_sw_high && next_sw_high {
            if let Some(pressed_at) = button_press_tick.take() {
                let held_ms = input_ticks.wrapping_sub(pressed_at);
                last_button_tick = input_ticks;
                if held_ms >= OVERVIEW_HOLD_MS {
                    dispatch_app(&mut app, &mut power_driver, Action::GoMainMenu);
                } else if held_ms >= 30 {
                    dispatch_app(&mut app, &mut power_driver, Action::NextControl);
                }
            }
        }
        encoder_sw_high = next_sw_high;

        match app.state().profile_request {
            ProfileRequest::None => {}
            ProfileRequest::Save(slot) => {
                let outputs_physically_off = app
                    .state()
                    .channels
                    .iter()
                    .all(|channel| !channel.physical_enabled);
                let settings = PersistentSettings::from_state(app.state());
                let status = if persist_settings_record(
                    &mut settings_store,
                    RecordKind::Profile(slot),
                    settings,
                    outputs_physically_off,
                ) {
                    ProfileStatus::Saved(slot)
                } else {
                    ProfileStatus::Failed
                };
                dispatch_app(
                    &mut app,
                    &mut power_driver,
                    Action::ProfileOperationFinished(status),
                );
            }
            ProfileRequest::Load(slot) => {
                if let Some(record) = settings_store.profiles[usize::from(slot)] {
                    if execute_global_shutdown(&mut power_driver).is_ok() {
                        dispatch_app(
                            &mut app,
                            &mut power_driver,
                            Action::ApplyProfile(record.settings, ProfileStatus::Loaded(slot)),
                        );
                    } else {
                        dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                        dispatch_app(
                            &mut app,
                            &mut power_driver,
                            Action::ProfileOperationFinished(ProfileStatus::Failed),
                        );
                    }
                } else {
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::ProfileOperationFinished(ProfileStatus::Empty(slot)),
                    );
                }
            }
            ProfileRequest::FactoryDefaults => {
                if execute_global_shutdown(&mut power_driver).is_ok() {
                    let defaults = AppState::new(app.state().recovery_armed, None);
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::ApplyProfile(
                            PersistentSettings::from_state(&defaults),
                            ProfileStatus::DefaultsLoaded,
                        ),
                    );
                } else {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::ProfileOperationFinished(ProfileStatus::Failed),
                    );
                }
            }
        }

        match app.state().awg_status {
            AwgStatus::StartRequested => {
                awg_scheduler.stop();
                arb_scheduler.stop();
                if execute_global_shutdown(&mut power_driver).is_ok() {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownApplied);
                    dispatch_app(&mut app, &mut power_driver, Action::AwgStartPrepared);
                } else {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                }
            }
            AwgStatus::StopRequested => {
                awg_scheduler.stop();
                arb_scheduler.stop();
                if execute_global_shutdown(&mut power_driver).is_ok() {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownApplied);
                } else {
                    dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                }
            }
            AwgStatus::Running => match app.state().awg_source {
                AwgSource::Builtin => {
                    arb_scheduler.stop();
                    if let Some(millivolts) =
                        awg_scheduler.tick(monotonic_awg_tick(), app.state().awg)
                    {
                        dispatch_app(&mut app, &mut power_driver, Action::AwgSample(millivolts));
                    }
                }
                AwgSource::Arbitrary => {
                    awg_scheduler.stop();
                    if let Some(start) = active_arb_start {
                        let tick = cortex_m::interrupt::free(|cs| {
                            let buffer = ARB_BUFFER.borrow(cs).borrow();
                            arb_scheduler.tick(monotonic_awg_tick(), start, &buffer)
                        });
                        let status = arb_scheduler.status();
                        ARB_INDEX.store(u32::from(status.index), Ordering::Relaxed);
                        ARB_CYCLES.store(status.cycles, Ordering::Relaxed);
                        ARB_LATE_UPDATES.store(status.late_updates, Ordering::Relaxed);
                        ARB_SKIPPED_CYCLES.store(status.skipped_cycles, Ordering::Relaxed);
                        match tick {
                            Some(ArbTick::Sample(millivolts)) => {
                                dispatch_app(
                                    &mut app,
                                    &mut power_driver,
                                    Action::AwgSample(millivolts),
                                );
                            }
                            Some(ArbTick::Finished) => {
                                if execute_global_shutdown(&mut power_driver).is_ok() {
                                    dispatch_app(
                                        &mut app,
                                        &mut power_driver,
                                        Action::GlobalShutdownApplied,
                                    );
                                    dispatch_app(&mut app, &mut power_driver, Action::AwgStopped);
                                } else {
                                    dispatch_app(
                                        &mut app,
                                        &mut power_driver,
                                        Action::GlobalShutdownFailed,
                                    );
                                }
                            }
                            None => {}
                        }
                    } else {
                        dispatch_app(&mut app, &mut power_driver, Action::GlobalShutdownFailed);
                    }
                }
            },
            AwgStatus::Fault => {
                if awg_scheduler.is_active() {
                    awg_scheduler.stop();
                    let _ = execute_global_shutdown(&mut power_driver);
                }
                if arb_scheduler.is_active() {
                    arb_scheduler.stop();
                    let _ = execute_global_shutdown(&mut power_driver);
                }
            }
            AwgStatus::Stopped => {
                awg_scheduler.stop();
                arb_scheduler.stop();
            }
            AwgStatus::Starting => {}
        }

        if let Some(start) = pending_arb_ack {
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
                pending_arb_ack = None;
            } else if matches!(
                app.state().awg_status,
                AwgStatus::Fault | AwgStatus::Stopped
            ) {
                queue_usb_response(b"ERR:HARDWARE\r\n");
                pending_arb_ack = None;
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
            let fault = match temperature {
                Some(raw) if raw >= OVERTEMPERATURE_TRIP_SIXTEENTHS_C => {
                    Some(benchvolt_poc::app::Fault::OverTemperature)
                }
                None => Some(benchvolt_poc::app::Fault::Sensor),
                _ => None,
            };
            if let Some(fault) = fault {
                for channel in 0..5u8 {
                    let output = &app.state().channels[usize::from(channel)];
                    if output.requested_enabled || output.physical_enabled {
                        dispatch_app(
                            &mut app,
                            &mut power_driver,
                            Action::ProtectionTrip { channel, fault },
                        );
                    }
                }
                let _ = execute_global_shutdown(&mut power_driver);
            }
        }
        if measurement_ticks >= 20 {
            measurement_ticks = 0;
            for (rail_index, rail, channels) in [
                (0usize, Rail::Dc1, [0u8, 1]),
                (1usize, Rail::Dc2, [2u8, 3]),
            ] {
                let active = channels.map(|channel| {
                    let output = &app.state().channels[usize::from(channel)];
                    output.requested_enabled || output.physical_enabled
                });
                if active.into_iter().any(|enabled| enabled) {
                    let fault = match power_driver.read_rail_status(rail) {
                        Ok(status) => tps55289_status_fault(status),
                        Err(_) => Some(benchvolt_poc::app::Fault::Hardware),
                    };
                    if let Some(fault) = fault {
                        if shared_pending_tps_faults[rail_index] == Some(fault) {
                            shared_pending_tps_faults[rail_index] = None;
                            for (channel, active) in channels.into_iter().zip(active) {
                                if active {
                                    dispatch_app(
                                        &mut app,
                                        &mut power_driver,
                                        Action::ProtectionTrip { channel, fault },
                                    );
                                }
                            }
                        } else {
                            shared_pending_tps_faults[rail_index] = Some(fault);
                        }
                    } else {
                        shared_pending_tps_faults[rail_index] = None;
                    }
                } else {
                    shared_pending_tps_faults[rail_index] = None;
                }
            }
            if app.state().channels[4].physical_enabled {
                match power_driver.read_ch5_status() {
                    Ok(status) => {
                        CH5_TPS_STATUS.store(status, Ordering::Relaxed);
                        if let Some(fault) = tps55289_status_fault(status) {
                            if ch5_pending_tps_fault == Some(fault) {
                                ch5_pending_tps_fault = None;
                                dispatch_app(
                                    &mut app,
                                    &mut power_driver,
                                    Action::ProtectionTrip { channel: 4, fault },
                                );
                            } else {
                                ch5_pending_tps_fault = Some(fault);
                            }
                        } else {
                            ch5_pending_tps_fault = None;
                        }
                    }
                    Err(_) => {
                        CH5_TPS_STATUS.store(0xff, Ordering::Relaxed);
                        dispatch_app(
                            &mut app,
                            &mut power_driver,
                            Action::ProtectionTrip {
                                channel: 4,
                                fault: benchvolt_poc::app::Fault::Hardware,
                            },
                        );
                    }
                }
            } else {
                ch5_pending_tps_fault = None;
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
            for (rail, channels) in [(Rail::Dc1, [0u8, 1]), (Rail::Dc2, [2u8, 3])] {
                if let Some(fault) =
                    shared_rail_protection_monitor.observe(app.state(), &measurements, rail)
                {
                    let active = channels.map(|channel| {
                        let output = &app.state().channels[usize::from(channel)];
                        output.requested_enabled || output.physical_enabled
                    });
                    for (channel, active) in channels.into_iter().zip(active) {
                        if active {
                            dispatch_app(
                                &mut app,
                                &mut power_driver,
                                Action::ProtectionTrip { channel, fault },
                            );
                        }
                    }
                }
            }
            if let Some(event) = sink_protection_monitor.observe(app.state(), sink_measurement) {
                let action = match event {
                    SinkProtectionEvent::Trip(fault) => Action::SinkProtectionTrip(fault),
                    SinkProtectionEvent::Recovered => Action::SinkProtectionRecovered,
                };
                dispatch_app(&mut app, &mut power_driver, action);
            }
            for (accumulator, measurement) in channel_accumulators.iter_mut().zip(measurements) {
                accumulator.push(measurement);
            }
            sink_accumulator.push(sink_measurement);
            if app.state().awg_status == AwgStatus::Running {
                awg_load_accumulator
                    .push(measurements[usize::from(app.state().active_awg_channel())]);
            } else {
                awg_load_accumulator.reset();
                awg_load_ticks = 0;
            }
            for channel in 0..5u8 {
                let protection_output = protection_output(app.state(), channel);
                let measurement = measurements[usize::from(channel)];
                let voltage_tracking = !(app.state().awg_status == AwgStatus::Running
                    && channel == app.state().active_awg_channel());
                let fault = protection_monitors[usize::from(channel)]
                    .observe_with_voltage_tracking(
                        &protection_output,
                        measurement,
                        voltage_tracking,
                    );
                if let Some(fault) = fault {
                    dispatch_app(
                        &mut app,
                        &mut power_driver,
                        Action::ProtectionTrip { channel, fault },
                    );
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
            dispatch_app(
                &mut app,
                &mut power_driver,
                Action::Measurements([
                    channel_accumulators[0].take(),
                    channel_accumulators[1].take(),
                    channel_accumulators[2].take(),
                    channel_accumulators[3].take(),
                    channel_accumulators[4].take(),
                ]),
            );
            dispatch_app(
                &mut app,
                &mut power_driver,
                Action::SinkMeasurement(sink_accumulator.take()),
            );
        }
        if awg_load_ticks >= 1_000 {
            awg_load_ticks = 0;
            dispatch_app(
                &mut app,
                &mut power_driver,
                Action::AwgLoadMeasurement(awg_load_accumulator.take()),
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
            cortex_m::peripheral::SCB::sys_reset();
        }
        feed_watchdog();
    }
}
