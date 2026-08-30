//! Boot-core service loop over the hand-rolled polled USB stack
//! (`boot_usb`). The sectioned v2 protocol runs over the CDC port with
//! v1-style lockstep ACK/NACK.

use boot_usb::{Event, Usb, MAX_PACKET};

/// IWDG kicker: once the application has enabled the watchdog it keeps
/// running across resets, so every v2 boot path must feed it or the board
/// resets every ~4 s. The cores never enable IWDG themselves (the plan's
/// rule stands) — they only keep a v1-enabled one alive.
pub fn feed_iwdg() {
    unsafe { core::ptr::write_volatile(0x4000_3000usize as *mut u32, 0xaaaa) };
}

/// Handover hygiene: the stock bootloader jumps to us with its SysTick and
/// USB interrupts still enabled. This core polls; mask everything.
fn handover_hygiene() {
    // Diagnostics live in SRAM that survives soft resets: clear them so
    // every reading reflects THIS boot only. Word-aligned writes only —
    // the M0 faults on unaligned access.
    for offset in (0..=0x30).step_by(4) {
        unsafe { core::ptr::write_volatile((0x2000_0100 + offset) as *mut u32, 0) };
    }
    cortex_m::interrupt::disable();
    unsafe {
        core::ptr::write_volatile(0xE000_E180usize as *mut u32, 0xFFFF_FFFF);
        core::ptr::write_volatile(0xE000_E280usize as *mut u32, 0xFFFF_FFFF);
        core::ptr::write_volatile(0xE000_E010usize as *mut u32, 0);
    }
}

/// Sample the physical interlock: encoder switch on PB14, active low
/// (external pull-up). Two samples ~1 ms apart must both read pressed.
pub fn interlock_held() -> bool {
    unsafe {
        let ahbenr = 0x4002_1014usize as *mut u32;
        core::ptr::write_volatile(ahbenr, core::ptr::read_volatile(ahbenr) | (1 << 18));
        let idr = 0x4800_0410usize as *const u32;
        let first = core::ptr::read_volatile(idr) & (1 << 14) == 0;
        for _ in 0..8_000 {
            core::hint::spin_loop();
        }
        let second = core::ptr::read_volatile(idr) & (1 << 14) == 0;
        first && second
    }
}

/// Full boot-core entry for a real slot core (golden / worker): decide
/// launch-vs-updater from the metadata page and the app descriptor, then
/// either hand over to the application or stay resident as the updater.
/// `core_writes_with_interlock` is true only for golden — the worker never
/// writes core sections (the running core must not touch its own pages).
/// `core_id` is a diagnostics marker written to 0x20000140.
pub fn boot_core_entry(core_writes_with_interlock: bool, core_id: u32) -> ! {
    unsafe { core::ptr::write_volatile(0x2000_0140usize as *mut u32, core_id) };
    let decision = boot_shared::core_boot_decision(
        crate::meta::layout_word(),
        crate::meta::request_word(),
        &crate::meta::scan_records(),
        crate::launch::app_valid(),
    );
    // The interlock forces the updater even with a healthy app — it is the
    // physical "I want the flasher" gesture and the authorization for
    // core-section writes.
    let interlock = interlock_held();
    if decision == boot_shared::CoreBoot::LaunchApp && !interlock {
        crate::meta::claim_attempt();
        crate::launch::launch_app();
    }
    run_boot_core(core_writes_with_interlock && interlock)
}

/// One boot-core updater loop. `allow_core` gates non-app sections
/// (slot B writes stay golden-only; the trampoline's interlock decides
/// which core is even reachable).
pub fn run_boot_core(allow_core: bool) -> ! {
    handover_hygiene();

    let mut usb = Usb::new();
    let mut upload = crate::protocol::UploadState::new();
    let mut packet = [0u8; MAX_PACKET];

    loop {
        feed_iwdg();
        match usb.poll() {
            Event::BulkRx => {
                let count = usb.recv(&mut packet);
                if count == 0 {
                    usb.rx_release();
                    continue;
                }
                let cmd = packet[0];
                if cmd == crate::protocol::CMD_BOOT {
                    usb.send(&[crate::protocol::ACK]);
                    // Drain the ACK (host polls it within a frame or two),
                    // then reset; the trampoline takes it from there.
                    for _ in 0..2_400_000 {
                        core::hint::spin_loop();
                    }
                    unsafe {
                        cortex_m::asm::dsb();
                        core::ptr::write_volatile(0xE000_ED0Cusize as *mut u32, 0x05FA_0004);
                    }
                    loop {
                        core::hint::spin_loop();
                    }
                }
                if cmd == crate::protocol::CMD_INFO {
                    let info = crate::protocol::cmd_info();
                    usb.send(&info);
                } else if let Some(response) =
                    crate::protocol::dispatch(&mut upload, cmd, &packet[1..count], allow_core)
                {
                    usb.send(&[response]);
                }
                usb.rx_release();
            }
            Event::None => {}
        }
    }
}
