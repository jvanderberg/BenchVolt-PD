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
    sectors: [[u8; 8]; 5],
    buffer: [u8; 8],
    opcode: u8,
    erase_mask: u8,
    programs: u8,
}

impl NvmBus {
    fn new(sectors: [[u8; 8]; 5]) -> Self {
        Self {
            sectors,
            buffer: [0; 8],
            opcode: 0,
            erase_mask: 0,
            programs: 0,
        }
    }
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
            FTP_CTRL1 => {
                self.opcode = values[0] & 7;
                if self.opcode == 2 {
                    self.erase_mask = values[0] >> 3;
                }
            }
            FTP_BUFFER if values.len() == 8 => self.buffer.copy_from_slice(values),
            FTP_BUFFER => self.buffer.fill(values[0]),
            FTP_CTRL0 if values[0] & 0x10 != 0 => match self.opcode {
                0 => self.buffer = self.sectors[usize::from(values[0] & 7)],
                5 => {
                    for (index, sector) in self.sectors.iter_mut().enumerate() {
                        if self.erase_mask & (1 << index) != 0 {
                            sector.fill(0xff);
                        }
                    }
                }
                6 => {
                    self.sectors[usize::from(values[0] & 7)] = self.buffer;
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

const NVM_TEST_SECTORS: [[u8; 8]; 5] = [
    [0x00, 0x00, 0xb0, 0xaa, 0x00, 0x45, 0x00, 0x02],
    [0x00, 0x40, 0x9d, 0x1c, 0xf0, 0x01, 0x00, 0xdf],
    [0x32, 0x40, 0x0f, 0x01, 0x32, 0x0a, 0xfc, 0xf1],
    [0x00, 0x19, 0x54, 0xaa, 0x57, 0x35, 0x5f, 0x00],
    [0x00, 0x64, 0x90, 0x21, 0x43, 0x00, 0x40, 0xfb],
];

#[test]
fn factory_restore_rewrites_both_sectors_to_canonical_and_is_idempotent() {
    let mut bus = NvmBus::new(NVM_TEST_SECTORS);
    assert_eq!(restore_canonical_nvm(&mut bus), Ok(NvmUpdate::Updated));
    assert_eq!(bus.sectors[3], CANONICAL_NVM_SECTOR3);
    assert_eq!(bus.sectors[4], CANONICAL_NVM_SECTOR4);
    let programs = bus.programs;
    assert_eq!(
        restore_canonical_nvm(&mut bus),
        Ok(NvmUpdate::AlreadyConfigured)
    );
    assert_eq!(bus.programs, programs);
}

#[test]
fn request_source_current_nvm_update_is_verified_and_idempotent() {
    let original = NVM_TEST_SECTORS[4];
    let mut bus = NvmBus::new(NVM_TEST_SECTORS);

    assert_eq!(
        configure_request_source_current(&mut bus),
        Ok(NvmUpdate::Updated)
    );
    assert_eq!(bus.sectors[4][6], 0x50);
    assert_eq!(&bus.sectors[4][..6], &original[..6]);
    assert_eq!(bus.sectors[4][7], original[7]);
    assert_eq!(bus.sectors[3], NVM_TEST_SECTORS[3]);
    assert_eq!(bus.programs, 1);
    assert_eq!(
        configure_request_source_current(&mut bus),
        Ok(NvmUpdate::AlreadyConfigured)
    );
    assert_eq!(bus.programs, 1);

    bus.sectors[4] = [0xff; 8];
    assert_eq!(
        configure_request_source_current(&mut bus),
        Ok(NvmUpdate::Updated)
    );
    assert_eq!(bus.sectors[4], CANONICAL_NVM_SECTOR4);
}

#[test]
fn nvm_sink_voltage_update_is_verified_and_idempotent() {
    let mut bus = NvmBus::new(NVM_TEST_SECTORS);

    // Canonical sector 4 stores 20 V (400 units): 0x64 << 2 | 0b00.
    assert_eq!(
        configure_nvm_sink_voltage(&mut bus, 20_000),
        Ok(NvmUpdate::AlreadyConfigured)
    );
    assert_eq!(bus.programs, 0);

    // 9 V = 180 units = 0b00_101101_00: high byte 45, low bits 00.
    assert_eq!(
        configure_nvm_sink_voltage(&mut bus, 9_000),
        Ok(NvmUpdate::Updated)
    );
    assert_eq!(bus.sectors[4][1], 45);
    assert_eq!(bus.sectors[4][0] & 0xc0, 0);
    assert_eq!(&bus.sectors[4][2..], &NVM_TEST_SECTORS[4][2..]);
    assert_eq!(bus.sectors[3], NVM_TEST_SECTORS[3]);
    assert_eq!(bus.programs, 1);
    assert_eq!(
        configure_nvm_sink_voltage(&mut bus, 9_000),
        Ok(NvmUpdate::AlreadyConfigured)
    );
    assert_eq!(bus.programs, 1);

    assert_eq!(
        configure_nvm_sink_voltage(&mut bus, 4_000),
        Err(PdError::NoSuitablePdo)
    );
    assert_eq!(
        configure_nvm_sink_voltage(&mut bus, 9_025),
        Err(PdError::NoSuitablePdo)
    );
}

#[test]
fn usb_comm_capable_nvm_update_is_verified_and_idempotent() {
    let original = NVM_TEST_SECTORS[3];
    let mut bus = NvmBus::new(NVM_TEST_SECTORS);

    assert_eq!(configure_usb_comm_capable(&mut bus), Ok(NvmUpdate::Updated));
    assert_eq!(bus.sectors[3][2], 0x55);
    assert_eq!(&bus.sectors[3][..2], &original[..2]);
    assert_eq!(&bus.sectors[3][3..], &original[3..]);
    assert_eq!(bus.sectors[4], NVM_TEST_SECTORS[4]);
    assert_eq!(bus.programs, 1);
    assert_eq!(
        configure_usb_comm_capable(&mut bus),
        Ok(NvmUpdate::AlreadyConfigured)
    );
    assert_eq!(bus.programs, 1);

    bus.sectors[3] = [0xff; 8];
    assert_eq!(configure_usb_comm_capable(&mut bus), Ok(NvmUpdate::Updated));
    assert_eq!(bus.sectors[3], CANONICAL_NVM_SECTOR3);
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
        Operation::Read(
            SINK_PDO1,
            vec![
                0x2c, 0x91, 0x01, 0x00, 0x2c, 0xd1, 0x02, 0x00, 0xc8, 0x40, 0x06, 0x00,
            ],
        ),
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
            sink_pdos: [
                0x2c, 0x91, 0x01, 0x00, 0x2c, 0xd1, 0x02, 0x00, 0xc8, 0x40, 0x06, 0x00,
            ],
            active_rdo: [0xc8, 0x58, 0x02, 0x30],
        })
    );
    assert!(bus.0.is_empty());
}

#[test]
fn legacy_boot_request_replays_the_original_ram_pdos_then_soft_resets() {
    let legacy_pdos = [
        0xc8, 0x20, 0x03, 0x00, // 10 V, 2 A
        0x2c, 0x21, 0x03, 0x00, // 10 V, 3 A
        0xf4, 0x41, 0x06, 0x00, // 20 V, 5 A
    ];
    let mut operations = VecDeque::new();
    for (offset, value) in legacy_pdos.into_iter().enumerate() {
        operations.push_back(Operation::Write(SINK_PDO1 + offset as u8, vec![value]));
    }
    operations.push_back(Operation::Write(TX_HEADER, vec![0x0d]));
    operations.push_back(Operation::Write(COMMAND_CTRL, vec![SEND_COMMAND]));
    let mut bus = ScriptBus(operations);

    assert_eq!(request_legacy_boot_contract(&mut bus), Ok(()));
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
