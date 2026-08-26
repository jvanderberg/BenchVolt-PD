use core::{
    cell::RefCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
};
use cortex_m::interrupt::Mutex;
use heapless::Deque;
use stm32_usbd::{MemoryAccess, UsbBus, UsbPeripheral};
use stm32f0xx_hal::{
    gpio::{
        gpioa::{PA11, PA12},
        Floating, Input,
    },
    pac::{self, interrupt},
};
use usb_device::{bus::UsbBusAllocator, device::StringDescriptors, prelude::*};
use usbd_serial::{SerialPort, USB_CLASS_CDC};

const USB_VID: u16 = 0x0483;
const USB_PID: u16 = 0x5740;

type BenchUsbBus = UsbBus<BenchUsb>;

#[derive(Clone, Copy)]
pub(crate) struct UsbMessage {
    bytes: [u8; 192],
    len: u16,
}

impl UsbMessage {
    const fn empty() -> Self {
        Self {
            bytes: [0; 192],
            len: 0,
        }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    fn from_slice(bytes: &[u8]) -> Self {
        let mut message = Self::empty();
        let len = bytes.len().min(message.bytes.len());
        message.bytes[..len].copy_from_slice(&bytes[..len]);
        message.len = len as u16;
        message
    }
}

struct UsbRuntime {
    device: UsbDevice<'static, BenchUsbBus>,
    serial: SerialPort<'static, BenchUsbBus>,
    rx_line: [u8; 192],
    rx_len: usize,
    rx_overflow: bool,
    commands: Deque<UsbMessage, 4>,
    responses: Deque<UsbMessage, 4>,
    response_offset: usize,
    /// Set when a response (or ERR:BUSY marker) had to be dropped because
    /// the response queue was full; drained as one ERR:OVERFLOW line so the
    /// host learns replies were lost instead of hanging on a silent drop.
    responses_dropped: bool,
}

impl UsbRuntime {
    fn poll(&mut self) {
        if self.device.poll(&mut [&mut self.serial]) {
            let mut packet = [0u8; 64];
            while let Ok(count) = self.serial.read(&mut packet) {
                if count == 0 {
                    break;
                }
                for byte in &packet[..count] {
                    if *byte == b'\n' {
                        if self.rx_overflow {
                            if self
                                .responses
                                .push_back(UsbMessage::from_slice(b"ERR:LINE_TOO_LONG\r\n"))
                                .is_err()
                            {
                                self.responses_dropped = true;
                            }
                            self.rx_len = 0;
                            self.rx_overflow = false;
                            continue;
                        }
                        let mut message = UsbMessage::empty();
                        message.bytes[..self.rx_len].copy_from_slice(&self.rx_line[..self.rx_len]);
                        message.len = self.rx_len as u16;
                        if self.commands.push_back(message).is_err()
                            && self
                                .responses
                                .push_back(UsbMessage::from_slice(b"ERR:BUSY\r\n"))
                                .is_err()
                        {
                            self.responses_dropped = true;
                        }
                        self.rx_len = 0;
                    } else if self.rx_overflow {
                        continue;
                    } else if self.rx_len < self.rx_line.len() {
                        self.rx_line[self.rx_len] = *byte;
                        self.rx_len += 1;
                    } else {
                        self.rx_len = 0;
                        self.rx_overflow = true;
                    }
                }
            }
        }

        if self.responses_dropped && !self.responses.is_full() {
            self.responses
                .push_back(UsbMessage::from_slice(b"ERR:OVERFLOW\r\n"))
                .ok();
            self.responses_dropped = false;
        }
        if let Some(response) = self.responses.front().copied() {
            if response.len == 0 {
                // A zero-length message would never satisfy the count > 0
                // arm below and would stall the queue forever.
                self.responses.pop_front();
                self.response_offset = 0;
                return;
            }
            match self
                .serial
                .write(&response.as_slice()[self.response_offset..])
            {
                Ok(count) if count > 0 => {
                    self.response_offset += count;
                    if self.response_offset == usize::from(response.len) {
                        self.responses.pop_front();
                        self.response_offset = 0;
                    }
                }
                _ => {}
            }
        }
    }
}

// `Option<UsbRuntime>` requires a nonzero discriminant and placed the entire
// 2.7 KiB zero-filled runtime in `.data`, consuming the same amount of flash.
// Keep zero-initialized storage in `.bss`; the flag is published only after
// install completes and before the USB interrupt is unmasked.
static USB_RUNTIME: Mutex<RefCell<MaybeUninit<UsbRuntime>>> =
    Mutex::new(RefCell::new(MaybeUninit::uninit()));
static USB_RUNTIME_INSTALLED: AtomicBool = AtomicBool::new(false);

fn with_runtime<R>(operation: impl FnOnce(&mut UsbRuntime) -> R) -> Option<R> {
    cortex_m::interrupt::free(|cs| {
        if !USB_RUNTIME_INSTALLED.load(Ordering::Relaxed) {
            return None;
        }
        let mut storage = USB_RUNTIME.borrow(cs).borrow_mut();
        Some(operation(unsafe { storage.assume_init_mut() }))
    })
}

#[interrupt]
fn USB() {
    with_runtime(UsbRuntime::poll);
}

pub(crate) fn take_usb_command() -> Option<UsbMessage> {
    with_runtime(|runtime| runtime.commands.pop_front()).flatten()
}

pub(crate) fn queue_usb_response(bytes: &[u8]) {
    let message = UsbMessage::from_slice(bytes);
    with_runtime(|runtime| {
        if runtime.responses.push_back(message).is_err() {
            runtime.responses_dropped = true;
        }
    });
    cortex_m::peripheral::NVIC::pend(pac::Interrupt::USB);
}

struct BenchUsb {
    _usb: pac::USB,
    _dm: PA11<Input<Floating>>,
    _dp: PA12<Input<Floating>>,
}

unsafe impl Sync for BenchUsb {}

unsafe impl UsbPeripheral for BenchUsb {
    const REGISTERS: *const () = pac::USB::ptr() as *const ();
    const DP_PULL_UP_FEATURE: bool = true;
    const EP_MEMORY: *const () = 0x4000_6000 as *const ();
    const EP_MEMORY_SIZE: usize = 1024;
    const EP_MEMORY_ACCESS: MemoryAccess = MemoryAccess::Word16x2;

    fn enable() {
        let rcc = unsafe { &*pac::RCC::ptr() };
        cortex_m::interrupt::free(|_| {
            rcc.apb1enr.modify(|_, w| w.usben().set_bit());

            // The C bootloader jumps to the application with its internal D+
            // pull-up still enabled.  Hold it low long enough for the host to
            // observe a real disconnect before presenting new descriptors.
            let usb = unsafe { &*pac::USB::ptr() };
            usb.bcdr.modify(|_, w| w.dppu().clear_bit());
            cortex_m::asm::delay(960_000);

            rcc.apb1rstr.modify(|_, w| w.usbrst().set_bit());
            rcc.apb1rstr.modify(|_, w| w.usbrst().clear_bit());
        });
    }

    fn startup_delay() {
        cortex_m::asm::delay(72);
    }
}

/// The stock bootloader's CDC serial number is ST's `Get_SerialNum`: eight
/// uppercase hex digits of `UID0 + UID2` followed by the top four hex digits
/// of `UID1`. The application must present the identical serial so the host
/// assigns the same port name to both — the desktop GUI's firmware updater
/// reopens the pre-jump port name to reach the bootloader.
fn chip_serial() -> &'static str {
    const UID_BASE: usize = 0x1fff_f7ac;
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let serial = cortex_m::singleton!(: [u8; 12] = [0; 12]).unwrap();
    let uid = |offset: usize| unsafe { core::ptr::read_volatile((UID_BASE + offset) as *const u32) };
    let mut value = uid(0).wrapping_add(uid(8));
    let mut high = uid(4);
    for (index, byte) in serial.iter_mut().enumerate() {
        let source = if index < 8 { &mut value } else { &mut high };
        *byte = HEX[(*source >> 28) as usize];
        *source <<= 4;
    }
    // Every byte is an ASCII hex digit from the table above.
    unsafe { core::str::from_utf8_unchecked(serial) }
}

pub(crate) fn install(usb: pac::USB, dm: PA11<Input<Floating>>, dp: PA12<Input<Floating>>) {
    let peripheral = BenchUsb {
        _usb: usb,
        _dm: dm,
        _dp: dp,
    };
    let usb_bus: &'static UsbBusAllocator<UsbBus<BenchUsb>> =
        cortex_m::singleton!(: UsbBusAllocator<UsbBus<BenchUsb>> = UsbBus::new(peripheral))
            .unwrap();
    let serial = SerialPort::new(usb_bus);
    let strings = [StringDescriptors::default()
        .manufacturer("BenchVolt-PD")
        .product("BenchVolt PD")
        .serial_number(chip_serial())];
    let device = UsbDeviceBuilder::new(usb_bus, UsbVidPid(USB_VID, USB_PID))
        .strings(&strings)
        .unwrap()
        .device_class(USB_CLASS_CDC)
        .device_release(0x0200)
        .build();
    cortex_m::interrupt::free(|cs| {
        USB_RUNTIME.borrow(cs).borrow_mut().write(UsbRuntime {
            device,
            serial,
            rx_line: [0; 192],
            rx_len: 0,
            rx_overflow: false,
            commands: Deque::new(),
            responses: Deque::new(),
            response_offset: 0,
            responses_dropped: false,
        });
        USB_RUNTIME_INSTALLED.store(true, Ordering::Relaxed);
    });
}
