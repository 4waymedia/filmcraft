//! Print the music cues and suggested cut points of a WAV file (developer aid for
//! [`filmcraft_audio_dsp::cues`]).
//!
//! ```text
//! cargo run -p filmcraft-audio-dsp --release --example cues -- <song.wav> [calm|cinematic|energetic|trailer] [density 0..100]
//! ```
//!
//! Reads 16/24/32-bit PCM and 32-bit float WAV. For MP3 or other formats, convert first, for
//! example `ffmpeg -i song.mp3 -ac 2 -ar 44100 song.wav`.

use filmcraft_audio_dsp::cues::{self, CutParams, Pacing};

/// Planar channels and the sample rate of a WAV file.
fn read_wav(bytes: &[u8]) -> Result<(Vec<Vec<f32>>, u32), String> {
    let u16_at = |o: usize| bytes.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| bytes.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a WAV file".into());
    }
    let (mut fmt, mut data) = (None, None);
    let mut o = 12usize;
    while let (Some(id), Some(size)) = (bytes.get(o..o + 4), u32_at(o + 4)) {
        let body = o + 8;
        let end = body.saturating_add(size as usize).min(bytes.len());
        match id {
            b"fmt " => fmt = Some((u16_at(body), u16_at(body + 2), u32_at(body + 4), u16_at(body + 14), u16_at(body + 24))),
            b"data" => data = bytes.get(body..end),
            _ => {}
        }
        o = end + (size as usize & 1);
    }
    let Some((Some(tag), Some(ch), Some(sr), Some(bits), sub)) = fmt else { return Err("no fmt chunk".into()) };
    let data = data.ok_or("no data chunk")?;
    let ch = ch.max(1) as usize;
    let bps = (bits as usize).div_ceil(8);
    if sr == 0 || bps == 0 {
        return Err("bad format".into());
    }
    let float = tag == 3 || (tag == 0xFFFE && sub == Some(3));
    let mut out = vec![Vec::with_capacity(data.len() / (bps * ch)); ch];
    for (i, s) in data.chunks_exact(bps).enumerate() {
        let v = match (bps, float) {
            (2, _) => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
            (3, _) => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
            (4, true) => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
            (4, false) => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
            _ => return Err(format!("{bits}-bit samples are not supported")),
        };
        if let Some(c) = out.get_mut(i % ch) {
            c.push(v);
        }
    }
    Ok((out, sr))
}

fn clock(samples: i64, sr: u32) -> String {
    let s = samples as f64 / sr.max(1) as f64;
    format!("{}:{:05.2}", (s / 60.0).floor() as i64, s % 60.0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().collect();
    let json = args.iter().any(|a| a == "--json");
    args.retain(|a| a != "--json");
    let Some(path) = args.get(1) else {
        eprintln!("usage: cues <song.wav> [calm|cinematic|energetic|trailer] [density 0..100] [--json]");
        std::process::exit(2);
    };
    let pacing = args.get(2).map(|p| Pacing::from_id(p).ok_or(format!("unknown pacing {p}"))).transpose()?.unwrap_or_default();
    let density = args.get(3).map(|d| d.parse::<f64>()).transpose()?.unwrap_or(50.0);
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read '{path}': {e} (the example reads WAV files; convert MP3 with ffmpeg first)"))?;
    let (audio, sr) = read_wav(&bytes).map_err(|e| format!("'{path}': {e}"))?;
    let refs: Vec<&[f32]> = audio.iter().map(Vec::as_slice).collect();
    let t0 = std::time::Instant::now();
    let c = cues::analyze(&refs, sr)?;
    let took = t0.elapsed();
    if json {
        let secs = |b: usize| c.beats.get(b).map(|&s| s as f64 / sr as f64).unwrap_or(c.len as f64 / sr as f64);
        let list = |v: Vec<String>| format!("[{}]", v.join(","));
        let pts = cues::cut_points(&c, CutParams { pacing, density, ..CutParams::default() });
        println!(
            "{{\"bpm\":{:.3},\"meter\":{},\"beats\":{},\"downbeats\":{},\"sections\":{},\"hits\":{},\"cuts\":{}}}",
            c.bpm,
            c.meter,
            list((0..c.beats.len()).map(|b| format!("{:.4}", secs(b))).collect()),
            list(c.downbeats.iter().map(|&b| format!("{:.4}", secs(b))).collect()),
            list(c.sections.iter().map(|s| format!("{:.4}", secs(s.start))).collect()),
            list(c.hits.iter().map(|h| format!("[{:.4},{}]", secs(h.beat), h.drop)).collect()),
            list(pts.iter().map(|p| format!("{:.4}", p.sample as f64 / sr as f64)).collect()),
        );
        return Ok(());
    }
    println!("{path}: {:.1} BPM, {}/4, {} beats, {} bars ({} ms)", c.bpm, c.meter, c.beats.len(), c.downbeats.len(), took.as_millis());
    println!("\nSections");
    for (k, s) in c.sections.iter().enumerate() {
        println!(
            "  {}  {}  bar {:>3}  energy {:.2}  novelty {:.2}",
            (b'A' + (k % 26) as u8) as char,
            clock(c.beats[s.start], sr),
            c.bar_of(s.start),
            s.energy,
            s.novelty
        );
    }
    println!("\nHits");
    for h in &c.hits {
        println!("  {}  bar {:>3}  strength {:.2}{}", clock(c.beats[h.beat], sr), c.bar_of(h.beat), h.strength, if h.drop { "  DROP" } else { "" });
    }
    println!("\nQuiet");
    for q in &c.quiet {
        let end = c.beats.get(q.end).copied().unwrap_or(c.len);
        println!("  {} – {}", clock(c.beats[q.start], sr), clock(end, sr));
    }
    let pts = cues::cut_points(&c, CutParams { pacing, density, ..CutParams::default() });
    println!("\nCut points ({}, density {density}): {}", pacing.id(), pts.len());
    let mut prev = c.beats.first().copied().unwrap_or(0);
    for p in &pts {
        let why: Vec<&str> = p.kinds.iter().map(|k| k.id()).collect();
        println!(
            "  {}  bar {:>3}  shot {:>5.2} s  score {:.2}  {}",
            clock(p.sample, sr),
            c.bar_of(p.beat),
            (p.sample - prev) as f64 / sr as f64,
            p.score,
            why.join(" + ")
        );
        prev = p.sample;
    }
    Ok(())
}
