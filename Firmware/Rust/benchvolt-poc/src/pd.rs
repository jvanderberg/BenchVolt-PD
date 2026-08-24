pub const STUSB4500_ADDRESS: u8 = 0x28;
pub const STUSB4500_I2C_HALF_CYCLE_US: u32 = 2;
const DEVICE_ID: u8 = 0x2f;
const PORT_STATUS: u8 = 0x0e;
const MONITORING_STATUS: u8 = 0x10;
const CC_STATUS: u8 = 0x11;
const CC_HW_FAULT_STATUS: u8 = 0x13;
const TYPEC_STATUS: u8 = 0x15;
const PRT_STATUS: u8 = 0x16;
const COMMAND_CTRL: u8 = 0x1a;
const RESET_CTRL: u8 = 0x23;
const VBUS_CTRL: u8 = 0x27;
const PE_FSM: u8 = 0x29;
const RX_BYTE_COUNT: u8 = 0x30;
const RX_HEADER: u8 = 0x31;
const RX_DATA: u8 = 0x33;
const TX_HEADER: u8 = 0x51;
const SINK_PDO_COUNT: u8 = 0x70;
const SINK_PDO1: u8 = 0x85;
const SINK_PDO3: u8 = 0x8d;
const ACTIVE_RDO: u8 = 0x91;
const FTP_PASSWORD: u8 = 0x95;
const FTP_CTRL0: u8 = 0x96;
const FTP_CTRL1: u8 = 0x97;
const FTP_BUFFER: u8 = 0x53;
const CANONICAL_NVM_SECTOR4: [u8; 8] = [0x00, 0x64, 0x90, 0x21, 0x43, 0x00, 0x50, 0xfb];
const DEVICE_IDS: [u8; 2] = [0x25, 0x21];
const PE_SINK_READY: u8 = 0x18;
const PD_MESSAGE_RECEIVED: u8 = 0x04;
const GET_SOURCE_CAPABILITIES: [u8; 2] = [0x07, 0x00];
const SEND_COMMAND: u8 = 0x26;
const OPERATION_TIMEOUT_MS: u16 = 500;
const PASSIVE_DISCOVERY_MS: u16 = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PdError {
    Bus,
    WrongDevice,
    Detached,
    Timeout,
    MalformedCapabilities,
    NoSuitablePdo,
    ContractMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedPdo {
    pub source_position: u8,
    pub millivolts: u16,
    pub milliamps: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub source: FixedPdo,
    pub requested_milliamps: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rdo {
    pub source_position: u8,
    pub operating_milliamps: u16,
    pub maximum_milliamps: u16,
    pub capability_mismatch: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Contract {
    pub source_position: u8,
    pub millivolts: u16,
    pub operating_milliamps: u16,
    pub maximum_milliamps: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BusError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NvmUpdate {
    AlreadyConfigured,
    Updated,
}

pub trait PdBus {
    fn read(&mut self, register: u8, values: &mut [u8]) -> Result<(), BusError>;
    fn write(&mut self, register: u8, values: &[u8]) -> Result<(), BusError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostics {
    pub device_id: u8,
    pub port_status: u8,
    pub monitoring_status: u8,
    pub cc_status: u8,
    pub cc_hw_fault_status: u8,
    pub typec_status: u8,
    pub reset_ctrl: u8,
    pub vbus_ctrl: u8,
    pub pe_fsm: u8,
    pub sink_pdo_count: u8,
    pub active_rdo: [u8; 4],
}

pub fn read_diagnostics(bus: &mut impl PdBus) -> Result<Diagnostics, BusError> {
    fn read_byte(bus: &mut impl PdBus, register: u8) -> Result<u8, BusError> {
        let mut value = [0];
        bus.read(register, &mut value)?;
        Ok(value[0])
    }

    let device_id = read_byte(bus, DEVICE_ID)?;
    let port_status = read_byte(bus, PORT_STATUS)?;
    let monitoring_status = read_byte(bus, MONITORING_STATUS)?;
    let cc_status = read_byte(bus, CC_STATUS)?;
    let cc_hw_fault_status = read_byte(bus, CC_HW_FAULT_STATUS)?;
    let typec_status = read_byte(bus, TYPEC_STATUS)?;
    let reset_ctrl = read_byte(bus, RESET_CTRL)?;
    let vbus_ctrl = read_byte(bus, VBUS_CTRL)?;
    let pe_fsm = read_byte(bus, PE_FSM)?;
    let sink_pdo_count = read_byte(bus, SINK_PDO_COUNT)?;
    let mut active_rdo = [0; 4];
    bus.read(ACTIVE_RDO, &mut active_rdo)?;
    Ok(Diagnostics {
        device_id,
        port_status,
        monitoring_status,
        cc_status,
        cc_hw_fault_status,
        typec_status,
        reset_ctrl,
        vbus_ctrl,
        pe_fsm,
        sink_pdo_count,
        active_rdo,
    })
}

fn ftp_wait(bus: &mut impl PdBus) -> Result<(), PdError> {
    for _ in 0..255 {
        let mut status = [0];
        bus.read(FTP_CTRL0, &mut status).map_err(|_| PdError::Bus)?;
        if status[0] & 0x10 == 0 {
            return Ok(());
        }
    }
    Err(PdError::Timeout)
}

fn ftp_exit(bus: &mut impl PdBus) -> Result<(), PdError> {
    bus.write(FTP_CTRL0, &[0x40]).map_err(|_| PdError::Bus)?;
    bus.write(FTP_CTRL1, &[0]).map_err(|_| PdError::Bus)?;
    bus.write(FTP_PASSWORD, &[0]).map_err(|_| PdError::Bus)
}

fn read_nvm_sector4(bus: &mut impl PdBus) -> Result<[u8; 8], PdError> {
    bus.write(FTP_PASSWORD, &[0x47]).map_err(|_| PdError::Bus)?;
    bus.write(FTP_CTRL0, &[0]).map_err(|_| PdError::Bus)?;
    bus.write(FTP_CTRL0, &[0xc0]).map_err(|_| PdError::Bus)?;
    bus.write(FTP_CTRL1, &[0]).map_err(|_| PdError::Bus)?;
    bus.write(FTP_CTRL0, &[0xd4]).map_err(|_| PdError::Bus)?;
    ftp_wait(bus)?;
    let mut sector = [0; 8];
    bus.read(FTP_BUFFER, &mut sector).map_err(|_| PdError::Bus)?;
    bus.write(FTP_CTRL0, &[0]).map_err(|_| PdError::Bus)?;
    ftp_exit(bus)?;
    Ok(sector)
}

/// Persist the STUSB4500 mode that requests all current advertised by a
/// matching source PDO. Only sector 4 is erased, the other seven bytes are
/// preserved, and the programmed bit is read back before success is returned.
pub fn configure_request_source_current(bus: &mut impl PdBus) -> Result<NvmUpdate, PdError> {
    let mut sector = read_nvm_sector4(bus)?;
    let erased = sector == [0xff; 8];
    if erased {
        sector = CANONICAL_NVM_SECTOR4;
    }
    if !erased && sector[6] & 0x10 != 0 {
        return Ok(NvmUpdate::AlreadyConfigured);
    }
    if !erased {
        sector[6] |= 0x10;
    }

    bus.write(FTP_PASSWORD, &[0x47]).map_err(|_| PdError::Bus)?;
    bus.write(FTP_BUFFER, &[0]).map_err(|_| PdError::Bus)?;
    bus.write(FTP_CTRL0, &[0]).map_err(|_| PdError::Bus)?;
    bus.write(FTP_CTRL0, &[0xc0]).map_err(|_| PdError::Bus)?;
    for opcode in [0x82, 0x07, 0x05] {
        bus.write(FTP_CTRL1, &[opcode]).map_err(|_| PdError::Bus)?;
        bus.write(FTP_CTRL0, &[0xd0]).map_err(|_| PdError::Bus)?;
        ftp_wait(bus)?;
    }
    bus.write(FTP_BUFFER, &sector).map_err(|_| PdError::Bus)?;
    for (opcode, control) in [(0x01, 0xd0), (0x06, 0xd4)] {
        bus.write(FTP_CTRL0, &[0xc0]).map_err(|_| PdError::Bus)?;
        bus.write(FTP_CTRL1, &[opcode]).map_err(|_| PdError::Bus)?;
        bus.write(FTP_CTRL0, &[control]).map_err(|_| PdError::Bus)?;
        ftp_wait(bus)?;
    }
    ftp_exit(bus)?;

    if read_nvm_sector4(bus)? != sector {
        return Err(PdError::ContractMismatch);
    }
    Ok(NvmUpdate::Updated)
}

pub fn decode_fixed_pdo(raw: u32, source_position: u8) -> Option<FixedPdo> {
    if raw >> 30 != 0 || !(1..=7).contains(&source_position) {
        return None;
    }
    let millivolts = ((raw >> 10) & 0x03ff) as u16 * 50;
    let milliamps = (raw & 0x03ff) as u16 * 10;
    (millivolts != 0 && milliamps != 0).then_some(FixedPdo {
        source_position,
        millivolts,
        milliamps,
    })
}

pub fn encode_sink_fixed_pdo(millivolts: u16, milliamps: u16) -> Option<u32> {
    if millivolts == 0
        || millivolts > 20_000
        || millivolts / 50 * 50 != millivolts
        || milliamps == 0
        || milliamps > 5_000
        || milliamps / 10 * 10 != milliamps
    {
        return None;
    }
    Some((u32::from(millivolts / 50) << 10) | u32::from(milliamps / 10))
}

pub fn select_highest_power_fixed(
    raw_pdos: &[u32],
    max_voltage_mv: u16,
    current_cap_ma: u16,
) -> Option<Selection> {
    raw_pdos
        .iter()
        .copied()
        .take(7)
        .enumerate()
        .filter_map(|(index, raw)| decode_fixed_pdo(raw, index as u8 + 1))
        .filter(|pdo| pdo.millivolts <= max_voltage_mv)
        .filter_map(|source| {
            let requested_milliamps = source.milliamps.min(current_cap_ma).min(5_000);
            (requested_milliamps >= 10).then_some(Selection {
                source,
                requested_milliamps,
            })
        })
        .max_by_key(|selection| {
            (
                u32::from(selection.source.millivolts) * u32::from(selection.requested_milliamps),
                selection.source.millivolts,
                selection.requested_milliamps,
            )
        })
}

pub fn decode_source_capabilities(
    header: [u8; 2],
    byte_count: u8,
    data: &[u8],
) -> Result<([u32; 7], usize), PdError> {
    let header = u16::from_le_bytes(header);
    let count = usize::from((header >> 12) & 0x07);
    if header & 0x1f != 1
        || count == 0
        || byte_count as usize != count * 4
        || data.len() != count * 4
    {
        return Err(PdError::MalformedCapabilities);
    }
    let mut pdos = [0u32; 7];
    for (destination, bytes) in pdos.iter_mut().zip(data.chunks_exact(4)) {
        *destination = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    Ok((pdos, count))
}

pub fn decode_rdo(bytes: [u8; 4]) -> Result<Rdo, PdError> {
    let raw = u32::from_le_bytes(bytes);
    let source_position = ((raw >> 28) & 0x07) as u8;
    if !(1..=7).contains(&source_position) {
        return Err(PdError::ContractMismatch);
    }
    Ok(Rdo {
        source_position,
        operating_milliamps: ((raw >> 10) & 0x03ff) as u16 * 10,
        maximum_milliamps: (raw & 0x03ff) as u16 * 10,
        capability_mismatch: raw & (1 << 26) != 0,
    })
}

pub fn match_passive_contract(
    sink_pdos: &[u32],
    rdo_bytes: [u8; 4],
    measured_vbus_mv: u16,
) -> Result<Contract, PdError> {
    let rdo = decode_rdo(rdo_bytes)?;
    if rdo.capability_mismatch
        || rdo.operating_milliamps == 0
        || rdo.maximum_milliamps == 0
        || rdo.operating_milliamps > 5_000
        || rdo.maximum_milliamps > 5_000
        || rdo.maximum_milliamps < rdo.operating_milliamps
        || measured_vbus_mv == 0
    {
        return Err(PdError::ContractMismatch);
    }

    // The RDO contains current and source-object position, but no voltage.
    // A non-mismatch autonomous contract must match one enabled local fixed
    // sink PDO, so use independently measured VBUS to identify that voltage.
    let matched = sink_pdos
        .iter()
        .copied()
        .take(3)
        .enumerate()
        .filter_map(|(index, raw)| decode_fixed_pdo(raw, index as u8 + 1))
        .filter(|pdo| pdo.millivolts <= 20_000)
        .filter(|pdo| {
            let measured = u32::from(measured_vbus_mv) * 100;
            let nominal = u32::from(pdo.millivolts);
            measured >= nominal * 80 && measured <= nominal * 120
        })
        .min_by_key(|pdo| pdo.millivolts.abs_diff(measured_vbus_mv))
        .ok_or(PdError::ContractMismatch)?;

    // Normally the operating current comes from the matched sink PDO. With
    // STUSB4500 REQ_SRC_CURRENT enabled, both RDO current fields instead carry
    // the matched source PDO current, which may legitimately exceed the sink
    // PDO's minimum requirement.
    if rdo.operating_milliamps > matched.milliamps
        && rdo.operating_milliamps != rdo.maximum_milliamps
    {
        return Err(PdError::ContractMismatch);
    }

    Ok(Contract {
        source_position: rdo.source_position,
        millivolts: matched.millivolts,
        operating_milliamps: rdo.operating_milliamps,
        maximum_milliamps: rdo.maximum_milliamps,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Discover,
    WaitReady { deadline: u16 },
    WaitCapabilities { deadline: u16 },
    WaitContract { deadline: u16, selection: Selection },
    Ready(Contract),
    Failed(PdError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PdEvent {
    Negotiated(Contract),
    Lost(PdError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceEvent {
    NegotiationStarted,
    Pd(PdEvent),
}

pub struct Service {
    negotiator: Negotiator,
    cadence_ms: u16,
    active: bool,
    started_pending: bool,
    command_pending: bool,
}

impl Service {
    pub const fn new(current_cap_ma: u16) -> Self {
        Self {
            negotiator: Negotiator::new(current_cap_ma),
            cadence_ms: 0,
            active: false,
            started_pending: false,
            command_pending: false,
        }
    }

    pub fn request_negotiation(&mut self, current_cap_ma: u16) {
        self.negotiator.restart(current_cap_ma);
        self.cadence_ms = 20;
        self.active = true;
        self.started_pending = true;
        self.command_pending = true;
    }

    pub const fn command_pending(&self) -> bool {
        self.command_pending
    }

    pub fn take_command_completion(&mut self, event: PdEvent) -> Option<Result<(), PdError>> {
        if !self.command_pending {
            return None;
        }
        self.command_pending = false;
        Some(match event {
            PdEvent::Negotiated(_) => Ok(()),
            PdEvent::Lost(error) => Err(error),
        })
    }

    pub const fn current_cap_ma(&self) -> u16 {
        self.negotiator.current_cap_ma()
    }

    pub fn tick<B: PdBus>(
        &mut self,
        elapsed_ms: u16,
        now: u16,
        outputs_off: bool,
        requested_current_cap_ma: u16,
        measured_vbus_mv: Option<u16>,
        bus: &mut B,
    ) -> [Option<ServiceEvent>; 2] {
        let mut events = [None; 2];
        if !self.active {
            self.cadence_ms = self.cadence_ms.saturating_add(elapsed_ms);
            if self.cadence_ms < PASSIVE_DISCOVERY_MS {
                return events;
            }
            self.cadence_ms = 0;
            if outputs_off {
                if let Some(measured_vbus_mv) = measured_vbus_mv {
                    match self.negotiator.import_passive(
                        bus,
                        measured_vbus_mv,
                        requested_current_cap_ma,
                    ) {
                        Ok(contract) => {
                            self.active = true;
                            events[1] = Some(ServiceEvent::Pd(PdEvent::Negotiated(contract)));
                        }
                        Err(error) => {
                            events[1] = Some(ServiceEvent::Pd(PdEvent::Lost(error)));
                        }
                    }
                }
            }
            return events;
        }
        if self.started_pending {
            self.started_pending = false;
            events[0] = Some(ServiceEvent::NegotiationStarted);
        }
        self.cadence_ms = self.cadence_ms.wrapping_add(elapsed_ms);
        if self.cadence_ms < 20 {
            return events;
        }
        self.cadence_ms = 0;
        let event = self.negotiator.step(bus, now);
        if matches!(event, Some(PdEvent::Lost(_))) {
            self.active = false;
        }
        events[1] = event.map(ServiceEvent::Pd);
        events
    }
}

pub struct Negotiator {
    state: State,
    current_cap_ma: u16,
}

impl Negotiator {
    pub const fn new(current_cap_ma: u16) -> Self {
        Self {
            state: State::Discover,
            current_cap_ma,
        }
    }

    pub const fn contract(&self) -> Option<Contract> {
        match self.state {
            State::Ready(contract) => Some(contract),
            _ => None,
        }
    }

    pub const fn current_cap_ma(&self) -> u16 {
        self.current_cap_ma
    }

    pub fn restart(&mut self, current_cap_ma: u16) {
        self.current_cap_ma = current_cap_ma;
        self.state = State::Discover;
    }

    pub const fn failed(&self) -> Option<PdError> {
        match self.state {
            State::Failed(error) => Some(error),
            _ => None,
        }
    }

    fn deadline_expired(now: u16, deadline: u16) -> bool {
        now.wrapping_sub(deadline) < 0x8000
    }

    fn fail(&mut self, error: PdError) -> Option<PdEvent> {
        self.state = State::Failed(error);
        Some(PdEvent::Lost(error))
    }

    fn read_byte(bus: &mut impl PdBus, register: u8) -> Result<u8, PdError> {
        let mut value = [0u8];
        bus.read(register, &mut value).map_err(|_| PdError::Bus)?;
        Ok(value[0])
    }

    fn send_get_source_capabilities(bus: &mut impl PdBus) -> Result<(), PdError> {
        bus.write(TX_HEADER, &GET_SOURCE_CAPABILITIES)
            .map_err(|_| PdError::Bus)?;
        bus.write(COMMAND_CTRL, &[SEND_COMMAND])
            .map_err(|_| PdError::Bus)
    }

    fn import_passive(
        &mut self,
        bus: &mut impl PdBus,
        measured_vbus_mv: u16,
        current_cap_ma: u16,
    ) -> Result<Contract, PdError> {
        let identity = Self::read_byte(bus, DEVICE_ID)?;
        if !DEVICE_IDS.contains(&identity) {
            return Err(PdError::WrongDevice);
        }
        if Self::read_byte(bus, PORT_STATUS)? & 1 == 0
            || Self::read_byte(bus, PE_FSM)? != PE_SINK_READY
        {
            return Err(PdError::Detached);
        }
        let count = usize::from(Self::read_byte(bus, SINK_PDO_COUNT)? & 0x07);
        if !(1..=3).contains(&count) {
            return Err(PdError::ContractMismatch);
        }
        let mut pdo_bytes = [0u8; 12];
        bus.read(SINK_PDO1, &mut pdo_bytes[..count * 4])
            .map_err(|_| PdError::Bus)?;
        let mut sink_pdos = [0u32; 3];
        for (pdo, bytes) in sink_pdos
            .iter_mut()
            .zip(pdo_bytes[..count * 4].chunks_exact(4))
        {
            *pdo = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        let mut rdo = [0u8; 4];
        bus.read(ACTIVE_RDO, &mut rdo).map_err(|_| PdError::Bus)?;
        let contract = match_passive_contract(&sink_pdos[..count], rdo, measured_vbus_mv)?;
        self.current_cap_ma = current_cap_ma;
        self.state = State::Ready(contract);
        Ok(contract)
    }

    fn step_result(&mut self, bus: &mut impl PdBus, now: u16) -> Result<Option<PdEvent>, PdError> {
        let result = match self.state {
            State::Discover => {
                let identity = Self::read_byte(bus, DEVICE_ID);
                match identity {
                    Ok(identity) if DEVICE_IDS.contains(&identity) => {
                        match Self::read_byte(bus, PORT_STATUS) {
                            Ok(status) if status & 1 != 0 => {
                                self.state = State::WaitReady {
                                    deadline: now.wrapping_add(OPERATION_TIMEOUT_MS),
                                };
                                return Ok(None);
                            }
                            Ok(_) => Err(PdError::Detached),
                            Err(error) => Err(error),
                        }
                    }
                    Ok(_) => Err(PdError::WrongDevice),
                    Err(error) => Err(error),
                }
            }
            State::WaitReady { deadline } => match Self::read_byte(bus, PE_FSM) {
                Ok(PE_SINK_READY) => Self::send_get_source_capabilities(bus).map(|()| {
                    self.state = State::WaitCapabilities {
                        deadline: now.wrapping_add(OPERATION_TIMEOUT_MS),
                    };
                }),
                Ok(_) if Self::deadline_expired(now, deadline) => Err(PdError::Timeout),
                Ok(_) => return Ok(None),
                Err(error) => Err(error),
            },
            State::WaitCapabilities { deadline } => match Self::read_byte(bus, PRT_STATUS) {
                Ok(status) if status & PD_MESSAGE_RECEIVED != 0 => {
                    let mut header = [0u8; 2];
                    let mut byte_count = [0u8];
                    bus.read(RX_HEADER, &mut header).map_err(|_| PdError::Bus)?;
                    if u16::from_le_bytes(header) & 0x1f != 1 {
                        return Ok(None);
                    }
                    bus.read(RX_BYTE_COUNT, &mut byte_count)
                        .map_err(|_| PdError::Bus)?;
                    let count = usize::from((u16::from_le_bytes(header) >> 12) & 0x07);
                    if count == 0 || count > 7 || byte_count[0] as usize != count * 4 {
                        Err(PdError::MalformedCapabilities)
                    } else {
                        let mut bytes = [0u8; 28];
                        bus.read(RX_DATA, &mut bytes[..count * 4])
                            .map_err(|_| PdError::Bus)?;
                        let (pdos, count) =
                            decode_source_capabilities(header, byte_count[0], &bytes[..count * 4])?;
                        let selection =
                            select_highest_power_fixed(&pdos[..count], 20_000, self.current_cap_ma)
                                .ok_or(PdError::NoSuitablePdo)?;
                        let sink_pdo = encode_sink_fixed_pdo(
                            selection.source.millivolts,
                            selection.requested_milliamps,
                        )
                        .ok_or(PdError::NoSuitablePdo)?;
                        bus.write(SINK_PDO3, &sink_pdo.to_le_bytes())
                            .map_err(|_| PdError::Bus)?;
                        // Re-advertising the source capabilities makes the STUSB4500
                        // re-run its autonomous match against the updated RAM PDO.
                        // A PD Soft Reset drops VBUS on the tested PPS source, resets
                        // the MCU, and loses the volatile PDO before it can be used.
                        Self::send_get_source_capabilities(bus)?;
                        self.state = State::WaitContract {
                            deadline: now.wrapping_add(OPERATION_TIMEOUT_MS),
                            selection,
                        };
                        return Ok(None);
                    }
                }
                Ok(_) if Self::deadline_expired(now, deadline) => Err(PdError::Timeout),
                Ok(_) => return Ok(None),
                Err(error) => Err(error),
            },
            State::WaitContract {
                deadline,
                selection,
            } => {
                let attached = Self::read_byte(bus, PORT_STATUS)? & 1 != 0;
                if !attached {
                    Err(PdError::Detached)
                } else if Self::read_byte(bus, PE_FSM)? != PE_SINK_READY {
                    if Self::deadline_expired(now, deadline) {
                        Err(PdError::Timeout)
                    } else {
                        return Ok(None);
                    }
                } else {
                    let mut bytes = [0u8; 4];
                    bus.read(ACTIVE_RDO, &mut bytes).map_err(|_| PdError::Bus)?;
                    let rdo = decode_rdo(bytes)?;
                    if rdo.source_position != selection.source.source_position
                        || rdo.capability_mismatch
                        || rdo.operating_milliamps != selection.requested_milliamps
                        || rdo.maximum_milliamps != selection.source.milliamps
                    {
                        if Self::deadline_expired(now, deadline) {
                            Err(PdError::ContractMismatch)
                        } else {
                            return Ok(None);
                        }
                    } else {
                        let contract = Contract {
                            source_position: rdo.source_position,
                            millivolts: selection.source.millivolts,
                            operating_milliamps: rdo.operating_milliamps,
                            maximum_milliamps: rdo.maximum_milliamps,
                        };
                        self.state = State::Ready(contract);
                        return Ok(Some(PdEvent::Negotiated(contract)));
                    }
                }
            }
            State::Ready(contract) => {
                let attached = Self::read_byte(bus, PORT_STATUS)? & 1 != 0;
                if !attached || Self::read_byte(bus, PE_FSM)? != PE_SINK_READY {
                    Err(PdError::Detached)
                } else {
                    let mut bytes = [0u8; 4];
                    bus.read(ACTIVE_RDO, &mut bytes).map_err(|_| PdError::Bus)?;
                    let rdo = decode_rdo(bytes)?;
                    if rdo.source_position != contract.source_position
                        || rdo.capability_mismatch
                        || rdo.operating_milliamps != contract.operating_milliamps
                        || rdo.maximum_milliamps != contract.maximum_milliamps
                    {
                        Err(PdError::ContractMismatch)
                    } else {
                        return Ok(None);
                    }
                }
            }
            State::Failed(_) => return Ok(None),
        };

        match result {
            Ok(()) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn step(&mut self, bus: &mut impl PdBus, now: u16) -> Option<PdEvent> {
        match self.step_result(bus, now) {
            Ok(event) => event,
            Err(error) => self.fail(error),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::{collections::VecDeque, vec, vec::Vec};

    #[test]
    fn stusb4500_i2c_clock_stays_within_fast_mode() {
        const FAST_MODE_MAX_HZ: u32 = 400_000;
        const CLOCK_HZ: u32 = 1_000_000 / (2 * STUSB4500_I2C_HALF_CYCLE_US);

        assert!(CLOCK_HZ <= FAST_MODE_MAX_HZ);
    }

    struct FailingBus;

    impl PdBus for FailingBus {
        fn read(&mut self, _: u8, _: &mut [u8]) -> Result<(), BusError> {
            Err(BusError)
        }

        fn write(&mut self, _: u8, _: &[u8]) -> Result<(), BusError> {
            Err(BusError)
        }
    }

    struct NvmBus {
        sector: [u8; 8],
        buffer: [u8; 8],
        opcode: u8,
        programs: u8,
    }

    impl PdBus for NvmBus {
        fn read(&mut self, register: u8, values: &mut [u8]) -> Result<(), BusError> {
            match register {
                FTP_CTRL0 => values[0] = 0,
                FTP_BUFFER => values.copy_from_slice(&self.buffer[..values.len()]),
                _ => return Err(BusError),
            }
            Ok(())
        }

        fn write(&mut self, register: u8, values: &[u8]) -> Result<(), BusError> {
            match register {
                FTP_PASSWORD => {}
                FTP_CTRL1 => self.opcode = values[0] & 7,
                FTP_BUFFER if values.len() == 8 => self.buffer.copy_from_slice(values),
                FTP_BUFFER => self.buffer.fill(values[0]),
                FTP_CTRL0 if values[0] & 0x10 != 0 => match self.opcode {
                    0 => self.buffer = self.sector,
                    5 => self.sector.fill(0xff),
                    6 => {
                        self.sector = self.buffer;
                        self.programs += 1;
                    }
                    _ => {}
                },
                FTP_CTRL0 => {}
                _ => return Err(BusError),
            }
            Ok(())
        }
    }

    #[test]
    fn request_source_current_nvm_update_is_verified_and_idempotent() {
        let original = [0x00, 0x64, 0x90, 0x21, 0x43, 0x00, 0x40, 0xfb];
        let mut bus = NvmBus {
            sector: original,
            buffer: [0; 8],
            opcode: 0,
            programs: 0,
        };

        assert_eq!(
            configure_request_source_current(&mut bus),
            Ok(NvmUpdate::Updated)
        );
        assert_eq!(bus.sector[6], 0x50);
        assert_eq!(&bus.sector[..6], &original[..6]);
        assert_eq!(bus.sector[7], original[7]);
        assert_eq!(bus.programs, 1);
        assert_eq!(
            configure_request_source_current(&mut bus),
            Ok(NvmUpdate::AlreadyConfigured)
        );
        assert_eq!(bus.programs, 1);

        bus.sector = [0xff; 8];
        assert_eq!(
            configure_request_source_current(&mut bus),
            Ok(NvmUpdate::Updated)
        );
        assert_eq!(bus.sector, CANONICAL_NVM_SECTOR4);
    }

    #[test]
    fn diagnostic_snapshot_is_read_only_and_preserves_raw_registers() {
        let mut bus = ScriptBus(VecDeque::from([
            Operation::Read(DEVICE_ID, vec![0x25]),
            Operation::Read(PORT_STATUS, vec![0x01]),
            Operation::Read(MONITORING_STATUS, vec![0x08]),
            Operation::Read(CC_STATUS, vec![0x11]),
            Operation::Read(CC_HW_FAULT_STATUS, vec![0x40]),
            Operation::Read(TYPEC_STATUS, vec![0x82]),
            Operation::Read(RESET_CTRL, vec![0x00]),
            Operation::Read(VBUS_CTRL, vec![0x02]),
            Operation::Read(PE_FSM, vec![PE_SINK_READY]),
            Operation::Read(SINK_PDO_COUNT, vec![0x03]),
            Operation::Read(ACTIVE_RDO, vec![0xc8, 0x58, 0x02, 0x30]),
        ]));

        assert_eq!(
            read_diagnostics(&mut bus),
            Ok(Diagnostics {
                device_id: 0x25,
                port_status: 0x01,
                monitoring_status: 0x08,
                cc_status: 0x11,
                cc_hw_fault_status: 0x40,
                typec_status: 0x82,
                reset_ctrl: 0x00,
                vbus_ctrl: 0x02,
                pe_fsm: PE_SINK_READY,
                sink_pdo_count: 0x03,
                active_rdo: [0xc8, 0x58, 0x02, 0x30],
            })
        );
        assert!(bus.0.is_empty());
    }

    #[test]
    fn service_never_transmits_until_a_deliberate_request() {
        let mut service = Service::new(2_000);
        let mut bus = FailingBus;
        for now in [20, 2_020, 4_020, 60_000] {
            assert_eq!(
                service.tick(2_000, now, true, 5_000, None, &mut bus),
                [None; 2]
            );
        }

        service.request_negotiation(1_500);
        assert!(service.command_pending());
        let events = service.tick(0, 60_001, true, 5_000, None, &mut bus);
        assert_eq!(
            events,
            [
                Some(ServiceEvent::NegotiationStarted),
                Some(ServiceEvent::Pd(PdEvent::Lost(PdError::Bus)))
            ]
        );
        assert_eq!(
            service.take_command_completion(PdEvent::Lost(PdError::Bus)),
            Some(Err(PdError::Bus))
        );
        assert_eq!(
            service.take_command_completion(PdEvent::Lost(PdError::Bus)),
            None
        );
        assert!(!service.command_pending());
        assert_eq!(service.current_cap_ma(), 1_500);
    }

    #[test]
    fn failed_pd_negotiation_never_retries_without_another_request() {
        let mut service = Service::new(2_000);
        let mut bus = FailingBus;
        service.request_negotiation(2_000);
        assert_eq!(
            service.tick(0, 0, true, 2_000, None, &mut bus)[1],
            Some(ServiceEvent::Pd(PdEvent::Lost(PdError::Bus)))
        );
        for now in [2_000, 4_000, 30_000] {
            assert_eq!(
                service.tick(2_000, now, true, 2_000, None, &mut bus),
                [None; 2]
            );
        }
    }

    fn fixed(millivolts: u16, milliamps: u16) -> u32 {
        encode_sink_fixed_pdo(millivolts, milliamps).unwrap()
    }

    #[test]
    fn fixed_pdo_codec_enforces_product_limits_and_resolution() {
        assert_eq!(
            decode_fixed_pdo(fixed(20_000, 5_000), 3)
                .unwrap()
                .millivolts,
            20_000
        );
        assert!(encode_sink_fixed_pdo(20_050, 1_000).is_none());
        assert!(encode_sink_fixed_pdo(9_001, 1_000).is_none());
        assert!(encode_sink_fixed_pdo(9_000, 1_001).is_none());
        assert!(decode_fixed_pdo((1 << 30) | fixed(9_000, 1_000), 1).is_none());
    }

    #[test]
    fn passive_contract_uses_measured_vbus_to_identify_the_local_sink_profile() {
        let sink_pdos = [
            fixed(5_000, 3_000),
            fixed(12_000, 2_000),
            fixed(20_000, 1_500),
        ];
        let rdo = ((4u32 << 28) | (1_500u32 / 10 << 10) | (2_250u32 / 10)).to_le_bytes();
        assert_eq!(
            match_passive_contract(&sink_pdos, rdo, 19_750),
            Ok(Contract {
                source_position: 4,
                millivolts: 20_000,
                operating_milliamps: 1_500,
                maximum_milliamps: 2_250,
            })
        );
        assert_eq!(
            match_passive_contract(&sink_pdos, rdo, 8_000),
            Err(PdError::ContractMismatch)
        );
        let mismatch = u32::from_le_bytes(rdo) | (1 << 26);
        assert_eq!(
            match_passive_contract(&sink_pdos, mismatch.to_le_bytes(), 20_000),
            Err(PdError::ContractMismatch)
        );
    }

    #[test]
    fn passive_contract_rejects_impossible_or_unsafe_rdo_currents() {
        let sink_pdos = [
            fixed(5_000, 3_000),
            fixed(12_000, 2_000),
            fixed(20_000, 1_500),
        ];
        let rdo = |operating_ma: u32, maximum_ma: u32| {
            ((3u32 << 28) | (operating_ma / 10 << 10) | (maximum_ma / 10)).to_le_bytes()
        };

        for invalid in [
            rdo(1_510, 2_250), // Operating current exceeds the matched sink PDO.
            rdo(1_500, 1_490), // Maximum current cannot be below operating current.
            rdo(5_010, 5_010), // USB PD fixed-supply current fields are capped at 5 A.
        ] {
            assert_eq!(
                match_passive_contract(&sink_pdos, invalid, 20_000),
                Err(PdError::ContractMismatch)
            );
        }

        // The RDO maximum reflects source capability and may exceed the local
        // sink PDO as long as the operating current stays within the sink limit.
        assert!(match_passive_contract(&sink_pdos, rdo(1_500, 5_000), 20_000).is_ok());

        // REQ_SRC_CURRENT deliberately books the matched source's full current,
        // so both RDO fields can exceed the sink PDO's minimum current.
        assert!(match_passive_contract(&sink_pdos, rdo(5_000, 5_000), 20_000).is_ok());
    }

    #[test]
    fn selection_chooses_highest_safe_power_and_caps_requested_current() {
        let pdos = [
            fixed(5_000, 3_000),
            fixed(9_000, 3_000),
            fixed(20_000, 2_000),
            (21_000u32 / 50 << 10) | (5_000u32 / 10),
        ];
        let selected = select_highest_power_fixed(&pdos, 20_000, 1_500).unwrap();
        assert_eq!(selected.source.source_position, 3);
        assert_eq!(selected.source.millivolts, 20_000);
        assert_eq!(selected.requested_milliamps, 1_500);
        assert!(select_highest_power_fixed(&pdos, 20_000, 0).is_none());
    }

    #[test]
    fn source_capability_and_rdo_parsers_reject_malformed_frames() {
        let header = [0x01, 0x30];
        let data = [
            fixed(5_000, 3_000),
            fixed(9_000, 3_000),
            fixed(20_000, 2_000),
        ]
        .map(u32::to_le_bytes)
        .concat();
        let (_, count) = decode_source_capabilities(header, 12, &data).unwrap();
        assert_eq!(count, 3);
        assert_eq!(
            decode_source_capabilities([0x02, 0x30], 12, &data),
            Err(PdError::MalformedCapabilities)
        );
        assert_eq!(
            decode_source_capabilities(header, 8, &data),
            Err(PdError::MalformedCapabilities)
        );

        let raw = (3u32 << 28) | (1_500u32 / 10 << 10) | (2_000u32 / 10);
        assert_eq!(
            decode_rdo(raw.to_le_bytes()).unwrap().operating_milliamps,
            1_500
        );
        assert_eq!(decode_rdo([0; 4]), Err(PdError::ContractMismatch));
    }

    enum Operation {
        Read(u8, Vec<u8>),
        Write(u8, Vec<u8>),
    }

    struct ScriptBus(VecDeque<Operation>);

    impl PdBus for ScriptBus {
        fn read(&mut self, register: u8, values: &mut [u8]) -> Result<(), BusError> {
            match self.0.pop_front() {
                Some(Operation::Read(expected, result))
                    if expected == register && result.len() == values.len() =>
                {
                    values.copy_from_slice(&result);
                    Ok(())
                }
                _ => Err(BusError),
            }
        }

        fn write(&mut self, register: u8, values: &[u8]) -> Result<(), BusError> {
            match self.0.pop_front() {
                Some(Operation::Write(expected, result))
                    if expected == register && result == values =>
                {
                    Ok(())
                }
                _ => Err(BusError),
            }
        }
    }

    #[test]
    fn service_imports_an_autonomous_contract_with_reads_only_after_settling() {
        let sink_pdos = [
            fixed(5_000, 3_000),
            fixed(12_000, 2_000),
            fixed(20_000, 1_500),
        ];
        let rdo = ((3u32 << 28) | (1_500u32 / 10 << 10) | (2_000u32 / 10)).to_le_bytes();
        let mut bus = ScriptBus(VecDeque::from(vec![
            Operation::Read(DEVICE_ID, vec![0x25]),
            Operation::Read(PORT_STATUS, vec![1]),
            Operation::Read(PE_FSM, vec![PE_SINK_READY]),
            Operation::Read(SINK_PDO_COUNT, vec![3]),
            Operation::Read(SINK_PDO1, sink_pdos.map(u32::to_le_bytes).concat()),
            Operation::Read(ACTIVE_RDO, rdo.to_vec()),
        ]));
        let mut service = Service::new(5_000);

        assert_eq!(
            service.tick(499, 499, true, 2_500, Some(19_800), &mut bus),
            [None; 2]
        );
        assert_eq!(bus.0.len(), 6);
        assert_eq!(
            service.tick(1, 500, false, 2_500, Some(19_800), &mut bus),
            [None; 2]
        );
        assert_eq!(bus.0.len(), 6);
        assert_eq!(
            service.tick(500, 1_000, true, 2_500, Some(19_800), &mut bus)[1],
            Some(ServiceEvent::Pd(PdEvent::Negotiated(Contract {
                source_position: 3,
                millivolts: 20_000,
                operating_milliamps: 1_500,
                maximum_milliamps: 2_000,
            })))
        );
        assert!(bus.0.is_empty());
        assert_eq!(service.current_cap_ma(), 2_500);
        assert_eq!(
            service.take_command_completion(PdEvent::Negotiated(Contract {
                source_position: 3,
                millivolts: 20_000,
                operating_milliamps: 1_500,
                maximum_milliamps: 2_000,
            })),
            None
        );
    }

    #[test]
    fn passive_detach_is_visible_to_the_boot_diagnostic_without_transmitting() {
        let mut bus = ScriptBus(VecDeque::from(vec![
            Operation::Read(DEVICE_ID, vec![0x25]),
            Operation::Read(PORT_STATUS, vec![0]),
        ]));
        let mut service = Service::new(5_000);

        assert_eq!(
            service.tick(500, 500, true, 5_000, Some(5_000), &mut bus),
            [
                None,
                Some(ServiceEvent::Pd(PdEvent::Lost(PdError::Detached)))
            ]
        );
        assert!(bus.0.is_empty());
    }

    #[test]
    fn negotiator_executes_bounded_active_sequence_and_verifies_rdo() {
        let source = [
            fixed(5_000, 3_000),
            fixed(9_000, 3_000),
            fixed(20_000, 2_000),
        ];
        let source_bytes = source.map(u32::to_le_bytes).concat();
        let requested = fixed(20_000, 1_500).to_le_bytes();
        let rdo = ((3u32 << 28) | (1_500u32 / 10 << 10) | (2_000u32 / 10)).to_le_bytes();
        let mut bus = ScriptBus(VecDeque::from(vec![
            Operation::Read(DEVICE_ID, vec![0x25]),
            Operation::Read(PORT_STATUS, vec![1]),
            Operation::Read(PE_FSM, vec![PE_SINK_READY]),
            Operation::Write(TX_HEADER, GET_SOURCE_CAPABILITIES.to_vec()),
            Operation::Write(COMMAND_CTRL, vec![SEND_COMMAND]),
            Operation::Read(PRT_STATUS, vec![PD_MESSAGE_RECEIVED]),
            Operation::Read(RX_HEADER, vec![0x01, 0x30]),
            Operation::Read(RX_BYTE_COUNT, vec![12]),
            Operation::Read(RX_DATA, source_bytes),
            Operation::Write(SINK_PDO3, requested.to_vec()),
            Operation::Write(TX_HEADER, GET_SOURCE_CAPABILITIES.to_vec()),
            Operation::Write(COMMAND_CTRL, vec![SEND_COMMAND]),
            Operation::Read(PORT_STATUS, vec![1]),
            Operation::Read(PE_FSM, vec![PE_SINK_READY]),
            Operation::Read(ACTIVE_RDO, rdo.to_vec()),
        ]));
        let mut negotiator = Negotiator::new(1_500);
        assert_eq!(negotiator.step(&mut bus, 0), None);
        assert_eq!(negotiator.step(&mut bus, 1), None);
        assert_eq!(negotiator.step(&mut bus, 2), None);
        assert_eq!(
            negotiator.step(&mut bus, 3),
            Some(PdEvent::Negotiated(Contract {
                source_position: 3,
                millivolts: 20_000,
                operating_milliamps: 1_500,
                maximum_milliamps: 2_000,
            }))
        );
        assert!(bus.0.is_empty());
    }

    #[test]
    fn unrelated_pd_messages_are_ignored_without_extending_the_deadline() {
        let mut negotiator = Negotiator {
            state: State::WaitCapabilities { deadline: 500 },
            current_cap_ma: 1_500,
        };
        let mut bus = ScriptBus(VecDeque::from(vec![
            Operation::Read(PRT_STATUS, vec![PD_MESSAGE_RECEIVED]),
            Operation::Read(RX_HEADER, vec![0x0f, 0x00]),
        ]));

        assert_eq!(negotiator.step(&mut bus, 100), None);
        assert_eq!(negotiator.state, State::WaitCapabilities { deadline: 500 });
        assert!(bus.0.is_empty());
    }

    #[test]
    fn ready_contract_rejects_any_rdo_identity_change() {
        let contract = Contract {
            source_position: 3,
            millivolts: 20_000,
            operating_milliamps: 1_500,
            maximum_milliamps: 2_000,
        };
        for changed_rdo in [
            (3u32 << 28) | (1_510u32 / 10 << 10) | (2_000u32 / 10),
            (3u32 << 28) | (1_500u32 / 10 << 10) | (1_990u32 / 10),
        ] {
            let mut negotiator = Negotiator {
                state: State::Ready(contract),
                current_cap_ma: 1_500,
            };
            let mut bus = ScriptBus(VecDeque::from(vec![
                Operation::Read(PORT_STATUS, vec![1]),
                Operation::Read(PE_FSM, vec![PE_SINK_READY]),
                Operation::Read(ACTIVE_RDO, changed_rdo.to_le_bytes().to_vec()),
            ]));

            assert_eq!(
                negotiator.step(&mut bus, 0),
                Some(PdEvent::Lost(PdError::ContractMismatch))
            );
        }
    }

    #[test]
    fn negotiation_times_out_without_an_unbounded_poll_loop() {
        let mut bus = ScriptBus(VecDeque::from(vec![
            Operation::Read(DEVICE_ID, vec![0x25]),
            Operation::Read(PORT_STATUS, vec![1]),
            Operation::Read(PE_FSM, vec![0]),
            Operation::Read(PE_FSM, vec![0]),
        ]));
        let mut negotiator = Negotiator::new(3_000);
        assert_eq!(negotiator.step(&mut bus, u16::MAX - 100), None);
        assert_eq!(negotiator.step(&mut bus, u16::MAX - 99), None);
        assert_eq!(
            negotiator.step(&mut bus, 400),
            Some(PdEvent::Lost(PdError::Timeout))
        );
        assert_eq!(negotiator.failed(), Some(PdError::Timeout));
        assert!(bus.0.is_empty());
    }

    #[test]
    fn every_bus_failure_becomes_a_terminal_reported_error() {
        let mut bus = ScriptBus(VecDeque::new());
        let mut negotiator = Negotiator::new(3_000);
        assert_eq!(
            negotiator.step(&mut bus, 0),
            Some(PdEvent::Lost(PdError::Bus))
        );
        assert_eq!(negotiator.step(&mut bus, 1), None);
        negotiator.restart(2_000);
        assert_eq!(negotiator.current_cap_ma(), 2_000);
    }
}
