pub const RCC_AHBENR: *mut u32 = 0x4002_1014 as *mut u32;
pub const GPIO_CLOCK_ENABLE_MASK: u32 = (1 << 17) | (1 << 18) | (1 << 19);

#[derive(Clone, Copy)]
pub struct PortShutdown {
    pub moder: *mut u32,
    pub bsrr: *mut u32,
    pub pin_mask: u16,
}

unsafe impl Sync for PortShutdown {}

pub const PORTS: [PortShutdown; 3] = [
    PortShutdown {
        moder: 0x4800_0000 as *mut u32,
        bsrr: 0x4800_0018 as *mut u32,
        pin_mask: 1 << 15,
    },
    PortShutdown {
        moder: 0x4800_0400 as *mut u32,
        bsrr: 0x4800_0418 as *mut u32,
        pin_mask: (1 << 2) | (1 << 6) | (1 << 7) | (1 << 15),
    },
    PortShutdown {
        moder: 0x4800_0800 as *mut u32,
        bsrr: 0x4800_0818 as *mut u32,
        pin_mask: (1 << 12) | (1 << 13),
    },
];

#[derive(Clone, Copy)]
pub struct OutputModes {
    pub clear: u32,
    pub set: u32,
}

/// Turn off every independent output control without relying on initialized
/// drivers, interrupts, or either I2C bus.
///
/// # Safety
/// Performs raw volatile writes to fixed GPIO registers; only meaningful on
/// the target hardware after the GPIO clocks are enabled (the firmware's
/// `prepare_emergency_shutdown` does that before anything can fail).
pub unsafe fn raw_emergency_shutdown() {
    for port in PORTS {
        unsafe {
            core::ptr::write_volatile(port.bsrr, u32::from(port.pin_mask) << 16);
        }
    }
}

pub const fn output_modes(pin_mask: u16) -> OutputModes {
    let mut clear = 0;
    let mut set = 0;
    let mut pin = 0;
    while pin < 16 {
        if pin_mask & (1 << pin) != 0 {
            clear |= 0b11 << (pin * 2);
            set |= 0b01 << (pin * 2);
        }
        pin += 1;
    }
    OutputModes { clear, set }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_shutdown_plan_covers_every_power_enable_in_safe_order() {
        assert_eq!(GPIO_CLOCK_ENABLE_MASK, (1 << 17) | (1 << 18) | (1 << 19));
        assert_eq!(PORTS.len(), 3);
        assert_eq!(PORTS[0].pin_mask, 1 << 15);
        assert_eq!(
            PORTS[1].pin_mask,
            (1 << 2) | (1 << 6) | (1 << 7) | (1 << 15)
        );
        assert_eq!(PORTS[2].pin_mask, (1 << 12) | (1 << 13));

        for port in PORTS {
            let modes = output_modes(port.pin_mask);
            assert_eq!(modes.set & !modes.clear, 0);
            for pin in 0..16 {
                let expected = port.pin_mask & (1 << pin) != 0;
                assert_eq!(modes.clear & (0b11 << (pin * 2)) != 0, expected);
                assert_eq!(modes.set & (0b01 << (pin * 2)) != 0, expected);
            }
        }
    }
}
