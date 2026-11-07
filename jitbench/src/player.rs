// Playing the sound as it is made: raw samples go down a pipe to pw-play (PipeWire). The pipe is
// kept small, a write blocks when the player is behind and that keeps the game at the speed of
// the sound without falling far ahead of it (which would delay the buttons).
use crate::machine::Machine;
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};

// about 30 ms of sound in the pipe, and as much in the player
const PIPE_BYTES: i32 = 4096;
const PLAYER_LATENCY: &str = "30ms";

// Start the player and stream the machine's sound to it (the sound has to be on). None when there
// is no pw-play.
pub fn start(m: &mut Machine) -> Option<Child> {
    let rate = common::audio::SAMPLE_RATE.to_string();
    let mut child = Command::new("pw-play")
        // raw samples, without --raw it wants a sound file with a header
        .args([
            "--raw",
            "--rate",
            &rate,
            "--channels",
            "2",
            "--format",
            "s16",
        ])
        .args(["--latency", PLAYER_LATENCY, "-"])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| eprintln!("cannot start pw-play: {e}"))
        .ok()?;
    let stdin = child.stdin.take().unwrap();
    // a smaller pipe is only a hint, the default works too, with more delay
    unsafe { libc::fcntl(stdin.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_BYTES) };
    m.sys.audio_out = Some(Box::new(stdin));
    Some(child)
}

// Let the player finish what it has
pub fn finish(m: &mut Machine, child: Option<Child>) {
    m.sys.audio_out = None;
    if let Some(mut c) = child {
        let _ = c.wait();
    }
}

// Whether the sound still goes somewhere (the player can go away)
pub fn playing(m: &Machine) -> bool {
    m.sys.audio_out.is_some()
}
