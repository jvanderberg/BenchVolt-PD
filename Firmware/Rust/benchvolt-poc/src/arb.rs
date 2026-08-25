use crate::limits::CH5_MAX_VOLTAGE_MV;

pub const MAX_POINTS: usize = 1_024;
pub const MAX_CHUNK_POINTS: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Point {
    /// Desktop protocol voltage units: 0.01 V.
    pub centivolts: u16,
    /// Desktop protocol dwell units, scaled by the START multiplier.
    pub dwell: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DataChunk {
    pub channel: u8,
    pub start: u16,
    pub points: [Point; MAX_CHUNK_POINTS],
    pub len: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Start {
    pub channel: u8,
    pub count: u16,
    /// Half-millisecond scheduler ticks per dwell unit.
    pub multiplier_ticks: u32,
    pub repetitions: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeDirective {
    Run(Start),
    Shutdown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeState {
    active: Option<Start>,
    pending_ack: Option<Start>,
}

impl RuntimeState {
    pub const fn new() -> Self {
        Self {
            active: None,
            pending_ack: None,
        }
    }

    pub fn arm(&mut self, start: Start) {
        self.active = Some(start);
        self.pending_ack = Some(start);
    }

    pub fn directive(&self) -> RuntimeDirective {
        self.active
            .map_or(RuntimeDirective::Shutdown, RuntimeDirective::Run)
    }

    pub fn pending_ack(&self) -> Option<Start> {
        self.pending_ack
    }

    pub fn take_pending_ack(&mut self) -> Option<Start> {
        self.pending_ack.take()
    }

    pub fn cancel(&mut self, channel: u8) -> bool {
        let matches = self.active.map(|start| start.channel) == Some(channel)
            || self.pending_ack.map(|start| start.channel) == Some(channel);
        if matches {
            self.active = None;
            self.pending_ack = None;
        }
        matches
    }

    pub fn finish(&mut self) {
        self.active = None;
    }

    pub fn clear(&mut self) {
        self.active = None;
        self.pending_ack = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseError {
    Syntax,
    Range,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UploadSession {
    channel: Option<u8>,
    next_index: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SequenceError;

impl UploadSession {
    pub const fn new() -> Self {
        Self {
            channel: None,
            next_index: 0,
        }
    }

    pub fn accept(&mut self, chunk: DataChunk) -> Result<(), SequenceError> {
        if chunk.start == 0 {
            self.channel = Some(chunk.channel);
            self.next_index = 0;
        }
        if self.channel != Some(chunk.channel) || chunk.start != self.next_index {
            return Err(SequenceError);
        }
        self.next_index = self.next_index.saturating_add(u16::from(chunk.len));
        Ok(())
    }

    pub fn is_complete_for(&self, start: Start) -> bool {
        self.channel == Some(start.channel) && self.next_index >= start.count
    }

    /// Invalidate the session so a new `START` requires a fresh, contiguous
    /// upload from index 0. Called when a run consumes the buffer; without
    /// this, a partial re-upload that errors mid-sequence could still leave
    /// a startable mixture of old and new points.
    pub fn invalidate(&mut self) {
        self.channel = None;
        self.next_index = 0;
    }
}

fn unsigned(text: &[u8]) -> Option<u16> {
    if text.is_empty() || text.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    text.iter().try_fold(0u16, |value, byte| {
        value.checked_mul(10)?.checked_add(u16::from(*byte - b'0'))
    })
}

fn channel_and_payload<'a>(command: &'a [u8], operation: &[u8]) -> Option<(u8, &'a [u8])> {
    let rest = command.strip_prefix(b"SOUR:WAVE:CH")?;
    let channel = rest.first()?.checked_sub(b'0')?;
    if !matches!(channel, 4 | 5) {
        return None;
    }
    Some((channel - 1, rest.get(1..)?.strip_prefix(operation)?))
}

pub fn parse_data(command: &[u8]) -> Result<Option<DataChunk>, ParseError> {
    if !command.starts_with(b"SOUR:WAVE:CH") || !command.windows(10).any(|w| w == b":ARB:DATA ") {
        return Ok(None);
    }
    let (channel, payload) =
        channel_and_payload(command, b":ARB:DATA ").ok_or(ParseError::Syntax)?;
    let mut tokens = payload.split(|byte| *byte == b',');
    let start = unsigned(tokens.next().ok_or(ParseError::Syntax)?).ok_or(ParseError::Syntax)?;
    if usize::from(start) >= MAX_POINTS {
        return Err(ParseError::Range);
    }
    let mut chunk = DataChunk {
        channel,
        start,
        points: [Point::default(); MAX_CHUNK_POINTS],
        len: 0,
    };
    while let Some(voltage) = tokens.next() {
        let dwell = tokens.next().ok_or(ParseError::Syntax)?;
        if usize::from(chunk.len) >= MAX_CHUNK_POINTS {
            return Err(ParseError::Range);
        }
        let point = Point {
            centivolts: unsigned(voltage).ok_or(ParseError::Syntax)?,
            dwell: unsigned(dwell)
                .filter(|value| *value != 0)
                .ok_or(ParseError::Range)?,
        };
        if point.centivolts > CH5_MAX_VOLTAGE_MV / 10
            || usize::from(start) + usize::from(chunk.len) >= MAX_POINTS
        {
            return Err(ParseError::Range);
        }
        chunk.points[usize::from(chunk.len)] = point;
        chunk.len += 1;
    }
    if chunk.len == 0 {
        return Err(ParseError::Syntax);
    }
    Ok(Some(chunk))
}

pub fn parse_start(command: &[u8]) -> Result<Option<Start>, ParseError> {
    if !command.starts_with(b"SOUR:WAVE:CH") || !command.windows(11).any(|w| w == b":ARB:START ") {
        return Ok(None);
    }
    let (channel, payload) =
        channel_and_payload(command, b":ARB:START ").ok_or(ParseError::Syntax)?;
    let mut tokens = payload.split(|byte| *byte == b',');
    let count = unsigned(tokens.next().ok_or(ParseError::Syntax)?)
        .filter(|value| *value != 0 && usize::from(*value) <= MAX_POINTS)
        .ok_or(ParseError::Range)?;
    let multiplier = tokens.next().ok_or(ParseError::Syntax)?;
    // Legacy integer multipliers are milliseconds. `0.5` is the compatible
    // extension that exposes one 2 kHz scheduler tick per dwell unit.
    let multiplier_ticks = if multiplier == b"0.5" {
        1
    } else {
        unsigned(multiplier)
            .filter(|value| *value != 0)
            .map(|value| u32::from(value) * 2)
            .ok_or(ParseError::Range)?
    };
    let repetitions =
        unsigned(tokens.next().ok_or(ParseError::Syntax)?).ok_or(ParseError::Syntax)?;
    if tokens.next().is_some() {
        return Err(ParseError::Syntax);
    }
    Ok(Some(Start {
        channel,
        count,
        multiplier_ticks,
        repetitions,
    }))
}

pub struct Buffer {
    points: [Point; MAX_POINTS],
    written: [u8; MAX_POINTS / 8],
}

impl Buffer {
    pub const fn new() -> Self {
        Self {
            points: [Point {
                centivolts: 0,
                dwell: 0,
            }; MAX_POINTS],
            written: [0; MAX_POINTS / 8],
        }
    }

    pub fn write(&mut self, chunk: DataChunk) {
        if chunk.start == 0 {
            self.written.fill(0);
        }
        for offset in 0..usize::from(chunk.len) {
            let index = usize::from(chunk.start) + offset;
            self.points[index] = chunk.points[offset];
            self.written[index / 8] |= 1 << (index % 8);
        }
    }

    pub fn validate(&self, start: Start) -> Option<(u16, u16, u16)> {
        let (minimum, maximum) = if start.channel == 3 {
            (50, 500)
        } else {
            (80, 2_200)
        };
        let points = &self.points[..usize::from(start.count)];
        if points.iter().enumerate().any(|(index, point)| {
            self.written[index / 8] & (1 << (index % 8)) == 0
                || !(minimum..=maximum).contains(&point.centivolts)
                || point.dwell == 0
                || u64::from(point.dwell)
                    .checked_mul(u64::from(start.multiplier_ticks))
                    .is_none()
        }) {
            return None;
        }
        Some((
            points[0].centivolts * 10,
            points.iter().map(|point| point.centivolts).min()? * 10,
            points.iter().map(|point| point.centivolts).max()? * 10,
        ))
    }

    pub fn point(&self, index: u16) -> Point {
        self.points[usize::from(index)]
    }

    fn period_ticks(&self, start: Start) -> u64 {
        self.points[..usize::from(start.count)]
            .iter()
            .map(|point| u64::from(point.dwell) * u64::from(start.multiplier_ticks))
            .sum()
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tick {
    Sample(u16),
    Finished,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SchedulerStatus {
    pub index: u16,
    pub cycles: u32,
    pub late_updates: u32,
    pub skipped_cycles: u32,
}

pub struct Scheduler {
    index: u16,
    cycles: u32,
    remaining: u64,
    period_ticks: u64,
    last_tick: u16,
    active: bool,
    late_updates: u32,
    skipped_cycles: u32,
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            index: 0,
            cycles: 0,
            remaining: 0,
            period_ticks: 0,
            last_tick: 0,
            active: false,
            late_updates: 0,
            skipped_cycles: 0,
        }
    }

    pub fn stop(&mut self) {
        self.active = false;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn status(&self) -> SchedulerStatus {
        SchedulerStatus {
            index: self.index,
            cycles: self.cycles,
            late_updates: self.late_updates,
            skipped_cycles: self.skipped_cycles,
        }
    }

    pub fn tick(&mut self, now: u16, start: Start, buffer: &Buffer) -> Option<Tick> {
        if !self.active {
            self.active = true;
            self.index = 0;
            self.cycles = 0;
            self.remaining = u64::from(buffer.point(0).dwell) * u64::from(start.multiplier_ticks);
            self.period_ticks = buffer.period_ticks(start);
            self.last_tick = now;
            self.late_updates = 0;
            self.skipped_cycles = 0;
            return None;
        }
        let elapsed = u64::from(now.wrapping_sub(self.last_tick));
        self.last_tick = now;
        if elapsed < self.remaining {
            self.remaining -= elapsed;
            return None;
        }
        let mut overrun = elapsed - self.remaining;
        if overrun != 0 {
            self.late_updates = self.late_updates.saturating_add(1);
        }
        loop {
            self.index += 1;
            if self.index >= start.count {
                self.index = 0;
                self.cycles = self.cycles.saturating_add(1);
                if start.repetitions != 0 && self.cycles >= u32::from(start.repetitions) {
                    self.active = false;
                    return Some(Tick::Finished);
                }
            }
            let dwell =
                u64::from(buffer.point(self.index).dwell) * u64::from(start.multiplier_ticks);
            if overrun < dwell {
                self.remaining = dwell - overrun;
                return Some(Tick::Sample(buffer.point(self.index).centivolts * 10));
            }
            overrun -= dwell;
            if self.period_ticks != 0 && overrun >= self.period_ticks {
                let skipped_cycles = overrun / self.period_ticks;
                if start.repetitions != 0
                    && u64::from(self.cycles) + skipped_cycles >= u64::from(start.repetitions)
                {
                    self.active = false;
                    return Some(Tick::Finished);
                }
                self.cycles = self.cycles.saturating_add(skipped_cycles as u32);
                self.skipped_cycles = self.skipped_cycles.saturating_add(skipped_cycles as u32);
                overrun %= self.period_ticks;
            }
        }
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_start(channel: u8) -> Start {
        Start {
            channel,
            count: 3,
            multiplier_ticks: 1,
            repetitions: 1,
        }
    }

    #[test]
    fn runtime_stop_cancels_active_start_and_deferred_ack_together() {
        let start = runtime_start(3);
        let mut runtime = RuntimeState::new();
        runtime.arm(start);

        assert!(runtime.cancel(start.channel));
        assert_eq!(runtime.directive(), RuntimeDirective::Shutdown);
        assert_eq!(runtime.pending_ack(), None);
    }

    #[test]
    fn runtime_stop_for_another_channel_preserves_the_session() {
        let start = runtime_start(4);
        let mut runtime = RuntimeState::new();
        runtime.arm(start);

        assert!(!runtime.cancel(3));
        assert_eq!(runtime.directive(), RuntimeDirective::Run(start));
        assert_eq!(runtime.take_pending_ack(), Some(start));
        assert_eq!(runtime.take_pending_ack(), None);
        assert_eq!(runtime.directive(), RuntimeDirective::Run(start));
    }

    #[test]
    fn parses_desktop_chunks_and_legacy_or_2khz_start() {
        let chunk = parse_data(b"SOUR:WAVE:CH4:ARB:DATA 8,50,1,500,2")
            .unwrap()
            .unwrap();
        assert_eq!(chunk.channel, 3);
        assert_eq!(chunk.start, 8);
        assert_eq!(
            chunk.points[1],
            Point {
                centivolts: 500,
                dwell: 2
            }
        );
        assert_eq!(
            parse_start(b"SOUR:WAVE:CH4:ARB:START 10,1,0")
                .unwrap()
                .unwrap()
                .multiplier_ticks,
            2
        );
        assert_eq!(
            parse_start(b"SOUR:WAVE:CH5:ARB:START 2,0.5,3")
                .unwrap()
                .unwrap()
                .multiplier_ticks,
            1
        );
    }

    #[test]
    fn buffer_requires_every_point_and_scheduler_preserves_absolute_deadlines() {
        let chunk = parse_data(b"SOUR:WAVE:CH4:ARB:DATA 0,50,1,100,1,150,1")
            .unwrap()
            .unwrap();
        let start = parse_start(b"SOUR:WAVE:CH4:ARB:START 3,0.5,1")
            .unwrap()
            .unwrap();
        let mut buffer = Buffer::new();
        buffer.write(chunk);
        assert_eq!(buffer.validate(start), Some((500, 500, 1_500)));
        let mut scheduler = Scheduler::new();
        assert_eq!(scheduler.tick(0, start, &buffer), None);
        assert_eq!(scheduler.tick(2, start, &buffer), Some(Tick::Sample(1_500)));
        assert_eq!(scheduler.tick(3, start, &buffer), Some(Tick::Finished));
    }

    #[test]
    fn consumed_session_requires_a_fresh_upload_before_the_next_start() {
        let start = Start {
            channel: 3,
            count: 4,
            multiplier_ticks: 2,
            repetitions: 1,
        };
        let chunk = DataChunk {
            channel: 3,
            start: 0,
            len: 4,
            points: [Point {
                centivolts: 100,
                dwell: 1,
            }; MAX_CHUNK_POINTS],
        };
        let mut session = UploadSession::new();
        assert!(session.accept(chunk).is_ok());
        assert!(session.is_complete_for(start));

        session.invalidate();
        assert!(!session.is_complete_for(start));

        // A partial re-upload that does not restart at index 0 must not
        // resurrect completeness from the stale buffer.
        let tail = DataChunk {
            channel: 3,
            start: 2,
            len: 2,
            points: chunk.points,
        };
        assert!(session.accept(tail).is_err());
        assert!(!session.is_complete_for(start));

        assert!(session.accept(chunk).is_ok());
        assert!(session.is_complete_for(start));
    }

    #[test]
    fn upload_session_requires_contiguous_same_channel_chunks() {
        let first = parse_data(b"SOUR:WAVE:CH4:ARB:DATA 0,50,1,100,1")
            .unwrap()
            .unwrap();
        let second = parse_data(b"SOUR:WAVE:CH4:ARB:DATA 2,150,1")
            .unwrap()
            .unwrap();
        let wrong = parse_data(b"SOUR:WAVE:CH5:ARB:DATA 3,80,1")
            .unwrap()
            .unwrap();
        let start = parse_start(b"SOUR:WAVE:CH4:ARB:START 3,0.5,0")
            .unwrap()
            .unwrap();
        let mut session = UploadSession::new();
        assert!(session.accept(first).is_ok());
        assert!(!session.is_complete_for(start));
        assert!(session.accept(second).is_ok());
        assert!(session.is_complete_for(start));
        assert!(session.accept(wrong).is_err());
    }

    #[test]
    fn late_service_skips_cycles_and_finite_repetition_finishes() {
        let chunk = parse_data(b"SOUR:WAVE:CH4:ARB:DATA 0,50,1")
            .unwrap()
            .unwrap();
        let infinite = parse_start(b"SOUR:WAVE:CH4:ARB:START 1,0.5,0")
            .unwrap()
            .unwrap();
        let finite = parse_start(b"SOUR:WAVE:CH4:ARB:START 1,0.5,3")
            .unwrap()
            .unwrap();
        let mut buffer = Buffer::new();
        buffer.write(chunk);

        let mut scheduler = Scheduler::new();
        assert_eq!(scheduler.tick(0, infinite, &buffer), None);
        assert_eq!(
            scheduler.tick(u16::MAX, infinite, &buffer),
            Some(Tick::Sample(500))
        );

        let mut scheduler = Scheduler::new();
        assert_eq!(scheduler.tick(0, finite, &buffer), None);
        assert_eq!(scheduler.tick(10, finite, &buffer), Some(Tick::Finished));
    }

    #[test]
    fn validation_applies_the_selected_channels_physical_range() {
        let chunk = parse_data(b"SOUR:WAVE:CH4:ARB:DATA 0,600,1")
            .unwrap()
            .unwrap();
        let ch4 = parse_start(b"SOUR:WAVE:CH4:ARB:START 1,1,0")
            .unwrap()
            .unwrap();
        let mut buffer = Buffer::new();
        buffer.write(chunk);
        assert!(buffer.validate(ch4).is_none());
    }
}
