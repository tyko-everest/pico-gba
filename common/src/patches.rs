// Changes to a game's code that are made as it is translated, for the games that are known. The
// device has no audio output, and the software mixer of the Game Boy Advance sound driver is the
// biggest steady user of the ROM (every channel streams its samples out of it) and of the CPU, so
// it is not run. The music sequencer in front of it has to keep running though: the title screen
// leaves for the copyright screen when the music player says nothing is playing.
//
// The patches are keyed by the game code in the ROM header and the address of the code, and are
// applied to the words the translator reads.

pub const GAME_CODE_OFFSET: u32 = 0xAC;

// Pokémon Emerald, the pokeemerald_modern build. SoundMain runs the sequencer and then jumps
// to the mixer through a literal (SoundMainRAM_Buffer, the copy of the mixer in IWRAM). The
// literal is changed to the end of the mixer, which restores the registers and returns, so the
// frame SoundMain built is cleaned up the normal way.
const EMERALD_MIXER_LITERAL: u32 = 0x0800_2808;
const EMERALD_MIXER_END: u32 = 0x0800_2BBA;

pub fn word(game_code: [u8; 4], addr: u32, original: u32) -> u32 {
    match (&game_code, addr) {
        (b"BPEE", EMERALD_MIXER_LITERAL) => EMERALD_MIXER_END | 1,
        _ => original,
    }
}
