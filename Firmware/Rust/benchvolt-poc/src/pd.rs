pub const STUSB4500_ADDRESS: u8 = 0x28;
const DEVICE_ID: u8 = 0x2f;
const PORT_STATUS: u8 = 0x0e;
const PRT_STATUS: u8 = 0x16;
const COMMAND_CTRL: u8 = 0x1a;
const PE_FSM: u8 = 0x29;
const RX_BYTE_COUNT: u8 = 0x30;
const RX_HEADER: u8 = 0x31;
const RX_DATA: u8 = 0x33;
const TX_HEADER: u8 = 0x51;
const SINK_PDO_COUNT: u8 = 0x70;
const SINK_PDO1: u8 = 0x85;
const SINK_PDO3: u8 = 0x8d;
const ACTIVE_RDO: u8 = 0x91;
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

pub trait PdBus {
    fn read(&mut self, register: u8, values: &mut [u8]) -> Result<(), BusError>;
    fn write(&mut self, register: u8, values: &[u8]) -> Result<(), BusError>;
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
}

impl Service {
    pub const fn new(current_cap_ma: u16) -> Self {
        Self {
            negotiator: Negotiator::new(current_cap_ma),
            cadence_ms: 0,
            active: false,
            started_pending: false,
        }
    }

    pub fn request_negotiation(&mut self, current_cap_ma: u16) {
        self.negotiator.restart(current_cap_ma);
        self.cadence_ms = 20;
        self.active = true;
        self.started_pending = true;
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
                    if let Ok(contract) = self.negotiator.import_passive(
                        bus,
                        measured_vbus_mv,
                        requested_current_cap_ma,
                    ) {
                        self.active = true;
                        events[1] = Some(ServiceEvent::Pd(PdEvent::Negotiated(contract)));
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

    struct FailingBus;

    impl PdBus for FailingBus {
        fn read(&mut self, _: u8, _: &mut [u8]) -> Result<(), BusError> {
            Err(BusError)
        }

        fn write(&mut self, _: u8, _: &[u8]) -> Result<(), BusError> {
            Err(BusError)
        }
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
        assert_eq!(
            service.tick(0, 60_001, true, 5_000, None, &mut bus),
            [
                Some(ServiceEvent::NegotiationStarted),
                Some(ServiceEvent::Pd(PdEvent::Lost(PdError::Bus)))
            ]
        );
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
