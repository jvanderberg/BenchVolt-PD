#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaintCommand {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub color: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaintTransfer {
    Bytes {
        data_mode: bool,
        bytes: [u8; 4],
        len: u8,
    },
    RepeatedColor {
        color: u16,
        pixels: u32,
    },
    Complete,
}

pub const fn dma_deadline_reached(now: u16, deadline: u16) -> bool {
    now.wrapping_sub(deadline) < 0x8000
}

pub const MAX_COALESCED_PIXELS: u32 = 1_024;

#[derive(Clone, Copy)]
struct PackedPaintCommand(u64);

impl PackedPaintCommand {
    const EMPTY: Self = Self(0);

    const fn encode(command: PaintCommand) -> Self {
        Self(
            command.x as u64
                | (command.y as u64) << 9
                | (command.width as u64) << 17
                | (command.height as u64) << 26
                | (command.color as u64) << 34,
        )
    }

    const fn decode(self) -> PaintCommand {
        PaintCommand {
            x: (self.0 & 0x1ff) as u16,
            y: ((self.0 >> 9) & 0xff) as u16,
            width: ((self.0 >> 17) & 0x1ff) as u16,
            height: ((self.0 >> 26) & 0xff) as u16,
            color: ((self.0 >> 34) & 0xffff) as u16,
        }
    }
}

impl PaintCommand {
    pub const fn pixel_count(self) -> u32 {
        self.width as u32 * self.height as u32
    }

    /// Return one nonblocking DMA step for an ST7789 solid rectangle. The
    /// setup bytes and the repeated-color pixel batch all travel through DMA.
    pub const fn transfer(self, step: u8, y_offset: u16) -> PaintTransfer {
        let column_end = self.x + self.width - 1;
        let page_start = self.y + y_offset;
        let page_end = page_start + self.height - 1;
        match step {
            0 => PaintTransfer::Bytes {
                data_mode: false,
                bytes: [0x2a, 0, 0, 0],
                len: 1,
            },
            1 => PaintTransfer::Bytes {
                data_mode: true,
                bytes: [
                    (self.x >> 8) as u8,
                    self.x as u8,
                    (column_end >> 8) as u8,
                    column_end as u8,
                ],
                len: 4,
            },
            2 => PaintTransfer::Bytes {
                data_mode: false,
                bytes: [0x2b, 0, 0, 0],
                len: 1,
            },
            3 => PaintTransfer::Bytes {
                data_mode: true,
                bytes: [
                    (page_start >> 8) as u8,
                    page_start as u8,
                    (page_end >> 8) as u8,
                    page_end as u8,
                ],
                len: 4,
            },
            4 => PaintTransfer::Bytes {
                data_mode: false,
                bytes: [0x2c, 0, 0, 0],
                len: 1,
            },
            5 => PaintTransfer::RepeatedColor {
                color: self.color,
                pixels: self.pixel_count(),
            },
            _ => PaintTransfer::Complete,
        }
    }

    const fn can_merge_horizontal(self, next: Self) -> bool {
        self.y == next.y
            && self.height == next.height
            && self.color == next.color
            && self.x as u32 + self.width as u32 == next.x as u32
    }

    const fn can_merge_vertical(self, next: Self) -> bool {
        self.x == next.x
            && self.width == next.width
            && self.color == next.color
            && self.y as u32 + self.height as u32 == next.y as u32
    }
}

pub struct PaintQueue<const N: usize> {
    // `head` and `len` already identify the initialized entries. Storing an
    // Option around every command wastes two bytes per slot on this target.
    commands: [PackedPaintCommand; N],
    head: usize,
    len: usize,
}

impl<const N: usize> PaintQueue<N> {
    pub const fn new() -> Self {
        Self {
            commands: [PackedPaintCommand::EMPTY; N],
            head: 0,
            len: 0,
        }
    }

    /// Newly pushed commands only try to merge into the most recent few
    /// entries. This scan runs inside an interrupt-free section: unbounded,
    /// with its O(len) overlap check per candidate, it reaches tens of
    /// thousands of iterations against a full queue — milliseconds of
    /// interrupts-off time PER PUSH. A sustained push burst then starves the
    /// DMA drain ISR below the enqueue deadline and latches a false display
    /// failure (observed on hardware from the PD Source screen's repaints).
    /// Raster-order span coalescing — the merging that matters — happens
    /// within the last handful of entries.
    const MERGE_SCAN_DEPTH: usize = 8;

    pub fn push(&mut self, command: PaintCommand) -> Result<(), PaintCommand> {
        if command.width == 0 || command.height == 0 {
            return Ok(());
        }
        let scan_start = self.len - self.len.min(Self::MERGE_SCAN_DEPTH);
        for offset in (scan_start..self.len).rev() {
            let index = (self.head + offset) % N;
            let Some(merged) = merge(self.commands[index].decode(), command) else {
                continue;
            };
            let later_overlaps = (offset + 1..self.len)
                .any(|later| overlaps(self.commands[(self.head + later) % N].decode(), command));
            if !later_overlaps {
                self.commands[index] = PackedPaintCommand::encode(merged);
                return Ok(());
            }
        }
        if self.len == N {
            return Err(command);
        }
        let tail = (self.head + self.len) % N;
        self.commands[tail] = PackedPaintCommand::encode(command);
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<PaintCommand> {
        if self.len == 0 {
            return None;
        }
        let command = self.commands[self.head].decode();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(command)
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

fn merge(previous: PaintCommand, next: PaintCommand) -> Option<PaintCommand> {
    if previous.can_merge_horizontal(next) {
        let width = u32::from(previous.width) + u32::from(next.width);
        if width <= u32::from(u16::MAX)
            && width * u32::from(previous.height) <= MAX_COALESCED_PIXELS
        {
            return Some(PaintCommand {
                width: width as u16,
                ..previous
            });
        }
    }
    if previous.can_merge_vertical(next) {
        let height = u32::from(previous.height) + u32::from(next.height);
        if height <= u32::from(u16::MAX)
            && u32::from(previous.width) * height <= MAX_COALESCED_PIXELS
        {
            return Some(PaintCommand {
                height: height as u16,
                ..previous
            });
        }
    }
    None
}

fn overlaps(a: PaintCommand, b: PaintCommand) -> bool {
    u32::from(a.x) < u32::from(b.x) + u32::from(b.width)
        && u32::from(b.x) < u32::from(a.x) + u32::from(a.width)
        && u32::from(a.y) < u32::from(b.y) + u32::from(b.height)
        && u32::from(b.y) < u32::from(a.y) + u32::from(a.height)
}

impl<const N: usize> Default for PaintQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: u16 = 0xf800;

    fn span(x: u16, y: u16, width: u16) -> PaintCommand {
        PaintCommand {
            x,
            y,
            width,
            height: 1,
            color: RED,
        }
    }

    #[test]
    fn adjacent_raster_spans_merge_before_reaching_dma() {
        let mut queue = PaintQueue::<2>::new();
        queue.push(span(3, 7, 4)).unwrap();
        queue.push(span(7, 7, 5)).unwrap();

        assert_eq!(queue.len(), 1);
        let merged = queue.pop().unwrap();
        assert_eq!(merged.x, 3);
        assert_eq!(merged.width, 9);
        assert_eq!(merged.pixel_count(), 9);
    }

    #[test]
    fn packed_commands_preserve_every_display_field() {
        let command = PaintCommand {
            x: 319,
            y: 169,
            width: 320,
            height: 170,
            color: 0xa55a,
        };
        assert_eq!(PackedPaintCommand::encode(command).decode(), command);
    }

    #[test]
    fn identical_spans_on_adjacent_rows_merge_vertically() {
        let mut queue = PaintQueue::<2>::new();
        queue.push(span(3, 7, 4)).unwrap();
        queue.push(span(3, 8, 4)).unwrap();

        let merged = queue.pop().unwrap();
        assert_eq!(merged.y, 7);
        assert_eq!(merged.height, 2);
        assert_eq!(merged.pixel_count(), 8);
    }

    #[test]
    fn rows_colors_and_noncontiguous_regions_remain_distinct() {
        let mut queue = PaintQueue::<4>::new();
        queue.push(span(0, 0, 2)).unwrap();
        queue.push(span(1, 1, 2)).unwrap();
        queue
            .push(PaintCommand {
                color: 0xffff,
                ..span(2, 1, 2)
            })
            .unwrap();
        queue.push(span(7, 1, 2)).unwrap();
        assert_eq!(queue.len(), 4);
    }

    #[test]
    fn overflow_is_explicit_and_never_discards_existing_work() {
        let mut queue = PaintQueue::<1>::new();
        let first = span(0, 0, 1);
        let second = span(4, 0, 1);
        queue.push(first).unwrap();
        assert!(queue.push(second) == Err(second));
        assert!(queue.pop() == Some(first));
        assert!(queue.is_empty());
    }

    #[test]
    fn wraparound_preserves_fifo_order() {
        let mut queue = PaintQueue::<2>::new();
        let first = span(0, 0, 1);
        let second = span(2, 0, 1);
        let third = span(4, 0, 1);
        queue.push(first).unwrap();
        queue.push(second).unwrap();
        assert!(queue.pop() == Some(first));
        queue.push(third).unwrap();
        assert!(queue.pop() == Some(second));
        assert!(queue.pop() == Some(third));
    }

    #[test]
    fn clear_drops_all_pending_commands_without_changing_capacity() {
        let mut queue = PaintQueue::<2>::new();
        queue.push(span(0, 0, 1)).unwrap();
        queue.push(span(2, 0, 1)).unwrap();
        queue.clear();
        assert!(queue.is_empty());
        queue.push(span(4, 0, 1)).unwrap();
        assert_eq!(queue.pop(), Some(span(4, 0, 1)));
    }

    #[test]
    fn solid_rectangle_is_one_setup_script_and_one_pixel_batch() {
        let command = PaintCommand {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
            color: 0xf81f,
        };
        assert_eq!(
            command.transfer(0, 35),
            PaintTransfer::Bytes {
                data_mode: false,
                bytes: [0x2a, 0, 0, 0],
                len: 1,
            }
        );
        assert_eq!(
            command.transfer(1, 35),
            PaintTransfer::Bytes {
                data_mode: true,
                bytes: [0, 0, 0, 15],
                len: 4,
            }
        );
        assert_eq!(
            command.transfer(2, 35),
            PaintTransfer::Bytes {
                data_mode: false,
                bytes: [0x2b, 0, 0, 0],
                len: 1,
            }
        );
        assert_eq!(
            command.transfer(3, 35),
            PaintTransfer::Bytes {
                data_mode: true,
                bytes: [0, 35, 0, 50],
                len: 4,
            }
        );
        assert_eq!(
            command.transfer(4, 35),
            PaintTransfer::Bytes {
                data_mode: false,
                bytes: [0x2c, 0, 0, 0],
                len: 1,
            }
        );
        assert_eq!(
            command.transfer(5, 35),
            PaintTransfer::RepeatedColor {
                color: 0xf81f,
                pixels: 256,
            }
        );
        assert_eq!(command.transfer(6, 35), PaintTransfer::Complete);
    }

    #[test]
    fn dma_deadline_comparison_handles_timer_wrap() {
        assert!(!dma_deadline_reached(u16::MAX - 2, 3));
        assert!(!dma_deadline_reached(2, 3));
        assert!(dma_deadline_reached(3, 3));
        assert!(dma_deadline_reached(4, 3));
    }

    #[test]
    fn queue_never_recombines_bands_beyond_dma_latency_bound() {
        let mut queue = PaintQueue::<2>::new();
        queue
            .push(PaintCommand {
                x: 0,
                y: 0,
                width: 320,
                height: 3,
                color: 0,
            })
            .unwrap();
        queue
            .push(PaintCommand {
                x: 0,
                y: 3,
                width: 320,
                height: 3,
                color: 0,
            })
            .unwrap();
        assert_eq!(queue.len(), 2);
        assert!(queue.pop().unwrap().pixel_count() <= MAX_COALESCED_PIXELS);
        assert!(queue.pop().unwrap().pixel_count() <= MAX_COALESCED_PIXELS);
    }

    #[test]
    fn merge_scan_is_bounded_so_pushes_stay_cheap_under_interrupt_free() {
        let mut queue = PaintQueue::<64>::new();
        queue.push(span(0, 0, 4)).unwrap();
        // Bury the mergeable entry deeper than the scan depth (staggered so
        // the fillers cannot merge with each other).
        for row in 1..=PaintQueue::<64>::MERGE_SCAN_DEPTH as u16 {
            queue.push(span(100 + row * 3, row * 2, 1)).unwrap();
        }
        queue.push(span(4, 0, 4)).unwrap();
        // No merge with the buried entry: it stays a separate command.
        assert_eq!(queue.len(), 2 + PaintQueue::<64>::MERGE_SCAN_DEPTH);

        // Within the scan depth the raster-order case still coalesces.
        let mut queue = PaintQueue::<64>::new();
        queue.push(span(0, 0, 4)).unwrap();
        queue.push(span(4, 0, 4)).unwrap();
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn nonoverlapping_later_work_does_not_prevent_scanline_coalescing() {
        let mut queue = PaintQueue::<3>::new();
        queue.push(span(0, 5, 2)).unwrap();
        queue.push(span(0, 6, 1)).unwrap();
        queue.push(span(2, 5, 3)).unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.pop().unwrap(), span(0, 5, 5));
    }

    #[test]
    fn intervening_overpaint_preserves_fifo_paint_order() {
        let mut queue = PaintQueue::<3>::new();
        queue.push(span(0, 5, 2)).unwrap();
        queue
            .push(PaintCommand {
                x: 2,
                y: 5,
                width: 1,
                height: 1,
                color: 0xffff,
            })
            .unwrap();
        queue.push(span(2, 5, 3)).unwrap();
        assert_eq!(queue.len(), 3);
    }
}
