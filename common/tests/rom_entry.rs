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
