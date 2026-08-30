use boot_shared::crc::crc32;
use boot_shared::image::{
    build_app_descriptor, build_slot_descriptor, parse_app_descriptor, parse_slot_descriptor,
    vectors_valid, VectorCheck,
};
use boot_shared::layout;
use boot_shared::metadata;
use boot_shared::{
    core_boot_decision, should_stay_in_updater, trampoline_decision, BootTarget, CoreBoot,
};

fn sample(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + 3) & 0xff) as u8).collect()
}

#[test]
fn crc_matches_reference_implementation() {
    // Vectors generated from flash_firmware.py::stm32_crc32.
    assert_eq!(crc32(b"123456789"), 0x5f65_8665);
    assert_eq!(crc32(&sample(1000)), 0x6680_d3a6);
    assert_eq!(crc32(&[]), 0xFFFF_FFFF);
}

#[test]
fn app_descriptor_round_trip() {
    let payload = sample(300);
    let crc = crc32(&payload);
    let desc = build_app_descriptor(payload.len() as u32, crc);
    let parsed = parse_app_descriptor(&desc).expect("descriptor parses");
    assert_eq!(parsed.size, 300);
    assert_eq!(parsed.crc, crc);
    assert_eq!(parsed.layout_version, layout::LAYOUT_VERSION);
}

#[test]
fn app_descriptor_rejects_bad_magic_and_oversize() {
    let mut desc = build_app_descriptor(1024, 0);
    desc[0] = 0;
    assert!(parse_app_descriptor(&desc).is_none());

    let desc = build_app_descriptor(layout::APP_MAX_SIZE + 1, 0);
    assert!(parse_app_descriptor(&desc).is_none());

    let desc = build_app_descriptor(0, 0);
    assert!(parse_app_descriptor(&desc).is_none());
}

#[test]
fn slot_descriptor_round_trip() {
    let payload = sample(100);
    let crc = crc32(&payload);
    let desc = build_slot_descriptor(100, crc);
    let parsed = parse_slot_descriptor(&desc).expect("descriptor parses");
    assert_eq!(parsed.size, 100);
    assert_eq!(parsed.crc, crc);
}

#[test]
fn vector_checks_match_stock_bootloader_semantics() {
    let good = VectorCheck {
        initial_sp: 0x2000_4000,
        reset_vector: 0x0800_5001,
    };
    assert!(vectors_valid(good, layout::APP_BASE, 0x1000));
    assert!(!vectors_valid(
        VectorCheck { initial_sp: 0x2000_4001, reset_vector: 0x0800_5001 },
        layout::APP_BASE,
        0x1000
    ));
    assert!(!vectors_valid(
        VectorCheck { initial_sp: 0x2000_0000, reset_vector: 0x0800_5000 },
        layout::APP_BASE,
        0x1000
    ));
    assert!(!vectors_valid(
        VectorCheck { initial_sp: 0x2000_0000, reset_vector: 0x0800_4FFF },
        layout::APP_BASE,
        0x1000
    ));
    assert!(!vectors_valid(
        VectorCheck { initial_sp: 0x2000_0000, reset_vector: 0x0800_6001 },
        layout::APP_BASE,
        0x1000
    ));
}

fn records(page: &[(bool, bool)]) -> Vec<u32> {
    let mut words = vec![metadata::ERASED; metadata::WORDS];
    let mut index = metadata::OFF_RECORDS;
    for (attempt, healthy) in page {
        let _ = attempt;
        assert!(index + 1 < metadata::WORDS, "test page overflow");
        words[index] = metadata::ATTEMPT_WORD;
        words[index + 1] = if *healthy { metadata::HEALTH_WORD } else { metadata::ERASED };
        index += 2;
    }
    words
}

#[test]
fn record_scan_counts_streaks() {
    let words = records(&[(true, true), (true, false), (true, false)]);
    match metadata::scan(&words) {
        metadata::RecordScan::Valid { healthy, unhealthy } => {
            assert_eq!(healthy, 1);
            assert_eq!(unhealthy, 2);
        }
        other => panic!("unexpected scan: {other:?}"),
    }
    assert!(should_stay_in_updater(&metadata::scan(&words)) == false);

    let words = records(&[(true, false), (true, false), (true, false)]);
    assert!(should_stay_in_updater(&metadata::scan(&words)));

    assert_eq!(metadata::scan(&[metadata::ERASED; metadata::WORDS]), metadata::RecordScan::Empty);
    assert!(!should_stay_in_updater(&metadata::scan(&[metadata::ERASED; metadata::WORDS])));
}

#[test]
fn record_page_fills_and_reports_next_slot() {
    let words = records(&[]);
    assert_eq!(
        metadata::next_attempt_addr(&words),
        Some(layout::METADATA_ADDR + (metadata::OFF_RECORDS as u32) * 4)
    );
    assert!(!metadata::records_full(&words));

    // Fill the page completely: 511 record words + 1 trailing word.
    let mut words = vec![metadata::ERASED; metadata::WORDS];
    for i in metadata::OFF_RECORDS..metadata::WORDS {
        words[i] = metadata::ATTEMPT_WORD;
    }
    assert!(metadata::next_attempt_addr(&words).is_none());
    assert!(metadata::records_full(&words));

    // One free word is not enough for a pair.
    let mut words = vec![metadata::ERASED; metadata::WORDS];
    for i in metadata::OFF_RECORDS..metadata::WORDS - 1 {
        words[i] = metadata::ATTEMPT_WORD;
    }
    assert_eq!(metadata::next_attempt_addr(&words), None);
    assert!(metadata::records_full(&words));
}

#[test]
fn core_boot_decision_table() {
    use metadata::ERASED;
    let empty = metadata::scan(&[ERASED; metadata::WORDS]);
    let version = layout::LAYOUT_VERSION;

    // Fresh migration: erased metadata + valid app launches.
    assert_eq!(core_boot_decision(ERASED, ERASED, &empty, true), CoreBoot::LaunchApp);
    assert_eq!(core_boot_decision(version, ERASED, &empty, true), CoreBoot::LaunchApp);
    // Invalid app, updater request (even torn), or foreign layout → updater.
    assert_eq!(core_boot_decision(version, ERASED, &empty, false), CoreBoot::Updater);
    assert_eq!(
        core_boot_decision(version, metadata::REQUEST_MARK, &empty, true),
        CoreBoot::Updater
    );
    assert_eq!(core_boot_decision(version, 0x7FFF_0000, &empty, true), CoreBoot::Updater);
    assert_eq!(core_boot_decision(0xDEAD_BEEF, ERASED, &empty, true), CoreBoot::Updater);

    // Unhealthy streak at the limit → updater.
    let streak = records(&[(true, false), (true, false), (true, false)]);
    assert_eq!(
        core_boot_decision(version, ERASED, &metadata::scan(&streak), true),
        CoreBoot::Updater
    );
    let below = records(&[(true, true), (true, false), (true, false)]);
    assert_eq!(
        core_boot_decision(version, ERASED, &metadata::scan(&below), true),
        CoreBoot::LaunchApp
    );
}

#[test]
fn trampoline_decision_table() {
    use metadata::{ERASED, SLOT_B_MARK};

    // Interlock always reaches golden.
    assert_eq!(
        trampoline_decision(SLOT_B_MARK, true, true, true, true),
        BootTarget::Golden
    );
    assert_eq!(trampoline_decision(ERASED, true, false, true, false), BootTarget::SlotB);
    assert_eq!(trampoline_decision(ERASED, true, false, false, false), BootTarget::Halt);

    // Flag selects B only when B is valid.
    assert_eq!(
        trampoline_decision(SLOT_B_MARK, false, true, true, true),
        BootTarget::SlotB
    );
    assert_eq!(trampoline_decision(SLOT_B_MARK, false, true, false, true), BootTarget::Golden);

    // Torn/unknown flag defaults to golden.
    assert_eq!(trampoline_decision(0x1234_5678, false, true, true, true), BootTarget::Golden);

    // Both slots invalid (migration window) falls back to the legacy entry.
    assert_eq!(trampoline_decision(ERASED, false, false, false, true), BootTarget::Legacy);
    assert_eq!(trampoline_decision(ERASED, false, false, false, false), BootTarget::Halt);
}
