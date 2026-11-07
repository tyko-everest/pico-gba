// A 16 bit stereo WAV file
pub fn encode(rate: u32, samples: &[i16]) -> Vec<u8> {
    let data = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    let mut put = |b: &[u8]| out.extend_from_slice(b);
    put(b"RIFF");
    put(&(36 + data).to_le_bytes());
    put(b"WAVEfmt ");
    put(&16u32.to_le_bytes());
    // PCM, 2 channels
    put(&1u16.to_le_bytes());
    put(&2u16.to_le_bytes());
    put(&rate.to_le_bytes());
    put(&(rate * 4).to_le_bytes());
    put(&4u16.to_le_bytes());
    put(&16u16.to_le_bytes());
    put(b"data");
    put(&data.to_le_bytes());
    for s in samples {
        put(&s.to_le_bytes());
    }
    out
}
