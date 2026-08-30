//! Application verification and handover, mirroring the stock bootloader's
//! JumpToMainApp (main.c:136-180) instruction for instruction: peripherals
//! quiesced, vectors copied to SRAM@0, SYSCFG remap, MSP, jump.

use boot_shared::{image, layout};
use core::ptr::{read_volatile, write_volatile};

/// Full application check: descriptor magic/version/size, CRC over the
/// image, and the same vector sanity the stock bootloader applies.
pub fn app_valid() -> bool {
    let mut descriptor = [0u8; 64];
    for (index, byte) in descriptor.iter_mut().enumerate() {
        *byte = unsafe { read_volatile((layout::DESC_ADDR as usize + index) as *const u8) };
    }
    let Some(desc) = image::parse_app_descriptor(&descriptor) else {
        return false;
    };
    let bytes = unsafe {
        core::slice::from_raw_parts(layout::APP_BASE as *const u8, desc.size as usize)
    };
    if boot_shared::crc::crc32(bytes) != desc.crc {
        return false;
    }
    let check = image::VectorCheck {
        initial_sp: unsafe { read_volatile(layout::APP_BASE as *const u32) },
        reset_vector: unsafe { read_volatile((layout::APP_BASE + 4) as *const u32) },
    };
    image::vectors_valid(check, layout::APP_BASE, desc.size)
}

/// Hand over to the verified application. Never returns.
pub fn launch_app() -> ! {
    unsafe {
        let app_stack = read_volatile(layout::APP_BASE as *const u32);
        let app_entry = read_volatile((layout::APP_BASE + 4) as *const u32);

        cortex_m::interrupt::disable();

        // SysTick off (CTRL, LOAD, VAL).
        write_volatile(0xE000_E010usize as *mut u32, 0);
        write_volatile(0xE000_E014usize as *mut u32, 0);
        write_volatile(0xE000_E018usize as *mut u32, 0);

        // USB: force reset, release, drop the pull-up, gate the clock —
        // the application re-initializes it from scratch.
        write_volatile(0x4000_5C40usize as *mut u32, 1); // CNTR = FRES
        write_volatile(0x4000_5C40usize as *mut u32, 0);
        let bcdr = 0x4000_5C58usize as *mut u32;
        write_volatile(bcdr, read_volatile(bcdr) & !(1 << 15));
        let apb1enr = 0x4002_101Cusize as *mut u32;
        write_volatile(apb1enr, read_volatile(apb1enr) & !(1 << 23));

        // NVIC: disable and clear everything.
        write_volatile(0xE000_E180usize as *mut u32, 0xFFFF_FFFF);
        write_volatile(0xE000_E280usize as *mut u32, 0xFFFF_FFFF);

        // Vector copy to SRAM@0 and remap (contract lines 12-13).
        for word in 0..(layout::VECTOR_TABLE_SIZE as usize / 4) {
            let value = read_volatile((layout::APP_BASE as usize + word * 4) as *const u32);
            write_volatile((0x2000_0000usize + word * 4) as *mut u32, value);
        }
        let apb2enr = 0x4002_1018usize as *mut u32;
        write_volatile(apb2enr, read_volatile(apb2enr) | 1); // SYSCFGEN
        let cfgr1 = 0x4001_0000usize as *mut u32;
        write_volatile(cfgr1, read_volatile(cfgr1) | 0b11); // MEM_MODE = SRAM

        cortex_m::asm::dsb();
        cortex_m::asm::isb();

        core::arch::asm!(
            "msr msp, {stack}",
            "bx {entry}",
            stack = in(reg) app_stack,
            entry = in(reg) app_entry | 1,
            options(noreturn),
        );
    }
}
