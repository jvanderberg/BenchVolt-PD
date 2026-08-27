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
mod startup;
mod types;
mod usb_intents;
mod usb_protocol;
mod usb_transport;
mod view;

use benchvolt_pd::app::AwgStatus;
use benchvolt_pd::arb::UploadSession as ArbUploadSession;
use benchvolt_pd::cadence::ServiceCadence;
use benchvolt_pd::early_shutdown::raw_emergency_shutdown;
use benchvolt_pd::input_policy::{encoder_action, ButtonTracker};
use benchvolt_pd::measurement::MeasurementWindows;
use benchvolt_pd::monitoring::ProtectionService;
use benchvolt_pd::pd::Service as PdService;
use benchvolt_pd::power::execute_global_shutdown;
use benchvolt_pd::reset_cause::ResetReason;
use benchvolt_pd::settings::{PersistentSettings, SettingsDebouncer};
use benchvolt_pd::usb_output::OutputTransaction;
use benchvolt_pd::waveform::Service as WaveformService;
use board::i2c::SoftPdBus;
use cortex_m_rt::{entry, exception, ExceptionFrame};
use embedded_hal::digital::v2::InputPin;
use input::{monotonic_ms, take_encoder_adjustment};
use loop_steps::LoopState;
use runtime::{dispatch_app, service_profile_request};
use usb_intents::UsbCtx;

pub(crate) const FLASH_READY_SPINS: u32 = 12_000_000;

/// Last-resort exit: raw all-off, record why, reset. Also the panic and
/// hard-fault destination.
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

/// IWDG feed — called exactly once per completed loop pass, so a wedged
/// pass resets the board within four seconds.
pub(crate) fn feed_watchdog() {
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

#[entry]
fn main() -> ! {
    let startup::Board {
        mut app,
        power: mut power_driver,
        mut pd_bus,
        mut adc_bank,
        mut settings_store,
        encoder_sw,
    } = startup::initialize();

    // Loop-owned services and state; everything hardware-shaped arrived in
    // `Board`. From here on, one pass = the body of the loop below, in order.
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
        journal_maintenance_done: false,
        pdo_flag_clear_needed: settings_store
            .latest
            .is_some_and(|record| record.settings.pdo_apply_pending_mv != 0),
        pdo_refresh_pending: false,
        display_failure_handled: false,
        pd_list_failures: 0,
        pd_list_not_before: 0,
        was_on_pd_source: false,
        pending_awg_ack: None,
        last_waveform_tick: loop_steps::monotonic_awg_tick(),
        pd_disturbed: false,
        pd_quiet_after: 0,
        pdo_settle_armed: false,
        pdo_settle_after: 0,
    };

    loop {
        // One framed USB command per pass; parsing, execution, and replies
        // all live in `usb_intents`.
        usb_intents::service_usb_command(
            &mut UsbCtx {
                app: &mut app,
                power: &mut power_driver,
                pd_bus: &mut pd_bus,
                pd_service: &mut pd_service,
                waveform: &mut waveform_service,
                arb_upload: &mut arb_upload,
                usb_output: &mut usb_output,
                ls: &mut ls,
            },
            &protection,
        );

        // Deferred boot repaint, plus the fail-closed display-death latch.
        loop_steps::render_step(&mut app, &mut power_driver, &mut ls);

        // Idle pacing: ~1 ms per pass, dropped while the waveform sampler
        // owns the loop.
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

        // PD contract watchdog: passive import, renegotiation events, and
        // USB negotiate-command completions.
        loop_steps::pd_step(
            &mut app,
            &mut power_driver,
            &mut pd_bus,
            &mut pd_service,
            &mut ls,
            elapsed_ms,
            awg_hot,
        );

        // Front-panel input. Deliberately inert after a display failure:
        // a UI that cannot show state must not change it.
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

        // Profile save/load and factory defaults (journal + verified shutdown).
        if service_profile_request(
            &mut app,
            &mut power_driver,
            &mut settings_store,
            cadence.healthy_for(3_000),
        ) {
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

        // PD Source screen: the once-per-boot capability read, then any
        // armed front-panel PDO apply (journal first, then STUSB reprofile).
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
            &cadence,
            &mut settings_store,
            &mut settings_effect,
            &mut ls,
        );
        // 2 kHz waveform scheduler, its start/stop directives, and the
        // deferred USB acks that wait on the engine actually running.
        loop_steps::waveform_step(&mut app, &mut power_driver, &mut waveform_service, &mut ls);
        loop_steps::awg_ack_step(&app, &mut waveform_service, &mut ls);

        // Periodic sensing: 100 ms temperature, 20 ms protection sweep and
        // CC regulation, 200 ms display measurement sync.
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
        // Debounced settings journal, then the healthy-loop one-shots:
        // deferred journal compaction, the PDO-flag clear, and the STUSB
        // comm-capable NVM check.
        loop_steps::persistence_step(
            &app,
            &mut settings_effect,
            &mut settings_store,
            elapsed_ms,
            cadence.healthy_for(3_000),
        );
        // Post-apply-reboot fast path: clear the journaled flag and reload
        // the PDO list as soon as it is safe, well before the 3 s window.
        loop_steps::pdo_flag_clear_step(
            &mut app,
            &mut power_driver,
            &cadence,
            &mut ls,
            &mut settings_store,
            &mut settings_effect,
        );
        loop_steps::maintenance_step(
            &app,
            &mut power_driver,
            &mut pd_bus,
            &cadence,
            &mut ls,
            &mut settings_store,
        );

        if app.state().reboot_requested {
            // A physical reboot is safe only after every independent output-off
            // control has been attempted.
            let _ = execute_global_shutdown(&mut power_driver);
            unsafe { raw_emergency_shutdown() };
            cortex_m::asm::delay(480_000);
            emergency_reset(ResetReason::UserReboot);
        }
        // Advance display DMA and certify this pass to the watchdog.
        display_dma::service();
        feed_watchdog();
    }
}
