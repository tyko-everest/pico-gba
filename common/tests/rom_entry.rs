// The Thumb entry of the ROMs the firmware has been run with, for the ones that are around
use common::rom::thumb_entry;

#[test]
fn known_roms() {
    for (name, entry) in [("Google-Dino-Advance.gba", 0xdc), ("nobot.gba", 0x108)] {
        let path = format!("{}/../firmware/src/{name}", env!("CARGO_MANIFEST_DIR"));
        if let Ok(rom) = std::fs::read(&path) {
            assert_eq!(thumb_entry(&rom), Some(entry), "{name}");
        }
    }
}

#[test]
fn emerald_crt0() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../pokeemerald/pokeemerald_modern.gba"
    );
    let Ok(rom) = std::fs::read(path) else { return };
    let entry = common::rom::find_entry(|at| {
        u32::from_le_bytes(rom[at as usize..at as usize + 4].try_into().unwrap())
    });
    // AgbMain and IntrMain of that build
    assert_eq!(
        entry,
        Some(common::rom::Entry {
            thumb: 0x0800_03AD,
            irq_handler: 0x0800_0250
        })
    );
}

#[test]
fn find_entry_agrees_with_thumb_entry() {
    for name in ["Google-Dino-Advance.gba", "nobot.gba"] {
        let path = format!("{}/../firmware/src/{name}", env!("CARGO_MANIFEST_DIR"));
        let Ok(rom) = std::fs::read(&path) else {
            continue;
        };
        let entry = common::rom::find_entry(|at| {
            u32::from_le_bytes(rom[at as usize..at as usize + 4].try_into().unwrap())
        });
        let expect = common::rom::thumb_entry(&rom).unwrap() as u32 + 0x0800_0000 + 1;
        assert_eq!(entry.map(|e| e.thumb), Some(expect), "{name}");
    }
}
