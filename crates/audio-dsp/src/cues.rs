//! Music cues for cutting picture to music (beyond Premiere): meter and downbeats, phrases,
//! sections, per-beat energy, hits and drops, quiet passages, and scored cut points for four
//! pacing profiles.
//!
//! ```text
//! remix::analyze ─► beats, tempo, beat self-similarity S(i, j)
//! per beat       ─► RMS (dB), low band < 150 Hz RMS (dB), onset strength (spectral flux at the attack)
//! meter, phase   ─► harmonic change 1 − S(i−1, i) (+ ¼ onset accent) averaged per bar phase,
//!                   for 4 and 3 beats per bar; the phase that stands out most marks the downbeats
//! sections       ─► structure features (Serrà et al. 2012, simplified): how much each bar repeats
//!                   the bar k bars earlier (one bar of S along the diagonal), for k = 1 … 32; a
//!                   downbeat where that repeat pattern (plus ½ × the energy) differs between the
//!                   2 bars before and after is a boundary; peaks ≥ 30 % of the largest, ≥ 4 bars apart
//! phrases        ─► every 4 bars from each section start
//! hits           ─► an attack (onset ≥ ¾ × the median) ≥ 6 dB louder, full band or < 150 Hz, than
//!                   the loudest of the 4 beats before;
//!                   a hit is a drop when the next bar is ≥ 6 dB louder than the 2 bars before
//! quiet          ─► beats ≥ 20 dB below the median beat level
//! cut points     ─► dynamic programme over beat boundaries: Σ cue score − Σ shot-length penalty,
//!                   shots within the pacing profile's range (bars, and a shortest time)
//! ```
//!
//! **Pacing profiles** (the same names and ranges as AssetCraft's storyboard generator, so a cut
//! planned in either app reads the same):
//!
//! | profile | shot length (bars) | shortest shot | cuts on |
//! |---|---|---|---|
//! | Calm | 2 … 8 | 3.0 s | sections, phrases, downbeats, quiet edges |
//! | Cinematic | 1 … 4 | 1.5 s | sections, drops, phrases, downbeats, hits (beats rarely) |
//! | Energetic | ½ … 2 | 0.6 s | everything, beats included |
//! | Trailer | ½ … 4 | 0.9 s | sections, drops, hits; shots shorten as the cut goes on |
//!
//! **Density** (0 … 100, default 50) moves the preferred shot length from the longest (0) to the
//! shortest (100) of the range; louder passages prefer shorter shots.
//!
//! Positions in [`Cues::beats`] are **attacks** (where a picture cut should land), in samples of
//! the analysed signal. Everything is deterministic. Inputs are hostile: a [`remix::Analysis`]
//! passed in from a cache is validated, and non-finite samples count as silence.

use crate::remix::{self, Analysis, RemixError};

/// Bump when the cues computed from the same audio change (cache keys).
pub const ANALYZER_VERSION: u32 = 1;

/// Why cues cannot be computed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CueError {
    /// The beat analysis failed (no beat, too short).
    Remix(RemixError),
    /// The analysis doesn't describe this audio (stale cache, corrupt data).
    Inconsistent(&'static str),
}

impl std::fmt::Display for CueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CueError::Remix(e) => e.fmt(f),
            CueError::Inconsistent(what) => write!(f, "the music analysis doesn't match the audio ({what})"),
        }
    }
}

impl std::error::Error for CueError {}

impl From<RemixError> for CueError {
    fn from(e: RemixError) -> Self {
        CueError::Remix(e)
    }
}

/// A stretch of music with one character, `[start, end)` in beat indices.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Section {
    pub start: usize,
    pub end: usize,
    /// Novelty of the boundary that starts it (0 for the first section), 0 … 1 of the largest.
    pub novelty: f32,
    /// Mean [`Cues::energy`] of its beats.
    pub energy: f32,
}

/// An accented beat.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub beat: usize,
    /// 0 … 1.
    pub strength: f32,
    /// The music gets markedly louder from here.
    pub drop: bool,
}

/// Quiet beats `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quiet {
    pub start: usize,
    pub end: usize,
}

/// Everything the editor needs to cut to a piece of music.
#[derive(Clone, Debug, PartialEq)]
pub struct Cues {
    pub sample_rate: u32,
    /// Signal length (samples).
    pub len: i64,
    /// Beat attacks (samples), increasing.
    pub beats: Vec<i64>,
    pub bpm: f64,
    /// Beats per bar (3 or 4).
    pub meter: usize,
    /// Beat indices of bar starts, increasing.
    pub downbeats: Vec<usize>,
    /// Beat indices of phrase starts (every 4 bars from each section start), increasing.
    pub phrases: Vec<usize>,
    /// Sections covering every beat, in order.
    pub sections: Vec<Section>,
    /// Per beat, 0 … 1 over the track (RMS 10th … 90th percentile, with onset strength).
    pub energy: Vec<f32>,
    /// Per beat, RMS level (dBFS).
    pub level_db: Vec<f32>,
    /// Per beat, onset strength (units of the envelope's standard deviation).
    pub onset: Vec<f32>,
    pub hits: Vec<Hit>,
    pub quiet: Vec<Quiet>,
}

impl Cues {
    /// 1-based bar number of a beat (beats before the first downbeat are bar 0, the pickup).
    pub fn bar_of(&self, beat: usize) -> usize {
        let first = self.downbeats.first().copied().unwrap_or(0);
        if beat < first { 0 } else { (beat - first) / self.meter.max(1) + 1 }
    }

    /// Section index of a beat.
    pub fn section_of(&self, beat: usize) -> usize {
        self.sections.iter().position(|s| beat >= s.start && beat < s.end).unwrap_or(self.sections.len().saturating_sub(1))
    }
}

/// Analyse planar audio (channels summed to mono).
pub fn analyze(channels: &[&[f32]], sample_rate: u32) -> Result<Cues, CueError> {
    let a = remix::analyze(channels, sample_rate)?;
    from_analysis(&a, channels)
}

fn mono(channels: &[&[f32]]) -> Vec<f32> {
    let len = channels.iter().map(|c| c.len()).max().unwrap_or(0);
    let k = 1.0 / channels.len().max(1) as f32;
    (0..len)
        .map(|i| {
            let v = channels.iter().map(|c| c.get(i).copied().unwrap_or(0.0)).filter(|v| v.is_finite()).sum::<f32>() * k;
            if v.is_finite() { v } else { 0.0 }
        })
        .collect()
}

fn validate(a: &Analysis, len: usize) -> Result<(), CueError> {
    let m = a.beats.len();
    if a.sample_rate == 0 {
        return Err(CueError::Inconsistent("sample rate"));
    }
    if a.len != len as i64 {
        return Err(CueError::Inconsistent("length"));
    }
    if m < 4 {
        return Err(CueError::Remix(RemixError::NoBeat));
    }
    if m.checked_mul(m) != Some(a.sim.len()) {
        return Err(CueError::Inconsistent("similarity size"));
    }
    if !(a.period.is_finite() && a.period > 0.0 && a.bpm.is_finite() && a.bpm > 0.0) {
        return Err(CueError::Inconsistent("tempo"));
    }
    if a.beats.first().is_some_and(|&b| b < 0) || a.beats.windows(2).any(|w| w[1] <= w[0]) || a.beats.last().is_some_and(|&b| b >= a.len) {
        return Err(CueError::Inconsistent("beat positions"));
    }
    Ok(())
}

fn median(v: &[f32]) -> f32 {
    let mut s: Vec<f32> = v.iter().copied().filter(|x| x.is_finite()).collect();
    s.sort_by(f32::total_cmp);
    s.get(s.len() / 2).copied().unwrap_or(0.0)
}

fn percentile(v: &[f32], p: f64) -> f32 {
    let mut s: Vec<f32> = v.iter().copied().filter(|x| x.is_finite()).collect();
    s.sort_by(f32::total_cmp);
    let i = ((s.len().saturating_sub(1)) as f64 * p).round() as usize;
    s.get(i).copied().unwrap_or(0.0)
}

fn db(mean_sq: f64) -> f32 {
    (10.0 * (mean_sq + 1e-12).log10()) as f32
}

fn mean(v: &[f32]) -> f32 {
    if v.is_empty() { 0.0 } else { v.iter().sum::<f32>() / v.len() as f32 }
}

/// Cues for audio already analysed by [`remix::analyze`] (for example from the Remix cache). The
/// analysis must come from the same `channels`.
pub fn from_analysis(a: &Analysis, channels: &[&[f32]]) -> Result<Cues, CueError> {
    let x = mono(channels);
    validate(a, x.len())?;
    let sr = a.sample_rate;
    let m = a.beats.len();
    let len = x.len();
    let sim = |i: usize, j: usize| -> f32 {
        let v = a.sim.get(i * m + j).copied().unwrap_or(0.0);
        if v.is_finite() { v } else { 0.0 }
    };
    // span of beat i: [beats[i], beats[i + 1]) (the last runs to the end)
    let span = |i: usize| -> (usize, usize) {
        let s = a.beats.get(i).copied().unwrap_or(0).clamp(0, len as i64) as usize;
        let e = a.beats.get(i + 1).copied().unwrap_or(len as i64).clamp(0, len as i64) as usize;
        (s, e.max(s))
    };

    // levels: full band and low band (two one-pole low-passes at 150 Hz)
    let alpha = 1.0 - (-std::f64::consts::TAU * 150.0 / sr as f64).exp();
    let mut low = Vec::with_capacity(len);
    let (mut y1, mut y2) = (0f64, 0f64);
    for &v in &x {
        y1 += alpha * (v as f64 - y1);
        y2 += alpha * (y1 - y2);
        low.push(y2);
    }
    let mut level_db = Vec::with_capacity(m);
    let mut low_db = Vec::with_capacity(m);
    for i in 0..m {
        let (s, e) = span(i);
        let n = (e - s).max(1) as f64;
        let sq: f64 = x.get(s..e).unwrap_or(&[]).iter().map(|v| (*v as f64) * (*v as f64)).sum();
        let lq: f64 = low.get(s..e).unwrap_or(&[]).iter().map(|v| v * v).sum();
        level_db.push(db(sq / n));
        low_db.push(db(lq / n));
    }

    // onset strength at each beat's attack
    let (env, n, hop) = remix::onset_envelope(&x, sr);
    let lead = remix::beat_lead(sr);
    let onset: Vec<f32> = a
        .beats
        .iter()
        .map(|&b| {
            let pos = b.saturating_add(lead).saturating_add(3 * hop as i64 / 4).saturating_sub(n as i64);
            let t = (pos.max(0) as usize) / hop.max(1);
            env.get(t.saturating_sub(2)..(t + 3).min(env.len())).unwrap_or(&[]).iter().copied().filter(|v| v.is_finite()).fold(0.0, f32::max)
        })
        .collect();
    let onset_med = median(&onset).max(1e-6);

    // energy 0 … 1
    let (p10, p90) = (percentile(&level_db, 0.1), percentile(&level_db, 0.9));
    let o90 = percentile(&onset, 0.9).max(1e-6);
    let energy: Vec<f32> = (0..m)
        .map(|i| {
            let r = if p90 - p10 > 1e-3 { ((level_db[i] - p10) / (p90 - p10)).clamp(0.0, 1.0) } else { 0.5 };
            (0.7 * r + 0.3 * (onset[i] / o90).clamp(0.0, 1.0)).clamp(0.0, 1.0)
        })
        .collect();

    // meter and downbeat phase
    let change: Vec<f32> = (0..m).map(|i| if i == 0 { 0.0 } else { 1.0 - sim(i - 1, i) }).collect();
    let phase_scores = |meter: usize| -> Vec<f32> {
        (0..meter)
            .map(|p| {
                let v: Vec<f32> = (1..m).filter(|i| i % meter == p).map(|i| change[i] + 0.25 * onset[i] / onset_med).collect();
                mean(&v)
            })
            .collect()
    };
    let contrast = |s: &[f32]| -> (f32, usize) {
        let (best, bp) = s.iter().enumerate().fold((f32::MIN, 0), |acc, (p, &v)| if v > acc.0 { (v, p) } else { acc });
        let others: Vec<f32> = s.iter().enumerate().filter(|(p, _)| *p != bp).map(|(_, v)| *v).collect();
        (best - mean(&others), bp)
    };
    let (c4, p4) = contrast(&phase_scores(4));
    let (c3, p3) = contrast(&phase_scores(3));
    let (meter, phase) = if m >= 6 && c3 > 1.25 * c4.max(0.0) && c3 > 0.0 { (3, p3) } else { (4, p4) };
    let meter = if m < 2 * meter { 4 } else { meter };
    let downbeats: Vec<usize> = (0..m).filter(|i| i % meter == phase % meter).collect();

    // sections: structure features (which earlier bars each bar repeats, at bar-aligned lags)
    // and loudness, compared over the 2 bars before and after each downbeat
    let max_lag = (32 * meter).min(m.saturating_sub(1));
    let lags: Vec<usize> = (1..).map(|k| k * meter).take_while(|&l| l <= max_lag).collect();
    // one bar of sequence similarity: beats i… against beats i−l…
    let rep = |i: usize, l: usize| -> Option<f32> {
        if i < l || i + meter > m {
            return None;
        }
        Some(mean(&(0..meter).map(|t| sim(i + t, i + t - l)).collect::<Vec<_>>()))
    };
    let span2 = 2 * meter;
    let novelty = |i: usize| -> f32 {
        let (mut d, mut n) = (0f32, 0usize);
        for &l in &lags {
            let before: Vec<f32> = (i.saturating_sub(span2)..i).step_by(meter).filter_map(|t| rep(t, l)).collect();
            let after: Vec<f32> = (i..(i + span2).min(m)).step_by(meter).filter_map(|t| rep(t, l)).collect();
            if before.len() * meter == span2 && after.len() * meter == span2 {
                d += (mean(&after) - mean(&before)).powi(2);
                n += 1;
            }
        }
        let structure = if n > 0 { (d / n as f32).sqrt() } else { 0.0 };
        let e_before = mean(energy.get(i.saturating_sub(span2)..i).unwrap_or(&[]));
        let e_after = mean(energy.get(i..(i + span2).min(m)).unwrap_or(&[]));
        structure + 0.5 * (e_after - e_before).abs()
    };
    let edge = 2 * meter;
    let gap = 4 * meter;
    let cands: Vec<(usize, f32)> = downbeats.iter().filter(|&&i| i >= edge && i + edge <= m).map(|&i| (i, novelty(i))).collect();
    let nmax = cands.iter().map(|c| c.1).fold(0.0, f32::max);
    let mut order = cands.clone();
    order.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut bounds: Vec<(usize, f32)> = Vec::new();
    if nmax > 0.02 {
        for (i, v) in order {
            if v < 0.3 * nmax {
                break;
            }
            if bounds.iter().all(|(b, _)| b.abs_diff(i) >= gap) {
                bounds.push((i, v / nmax));
            }
        }
    }
    bounds.sort_by_key(|b| b.0);
    let mut sections = Vec::with_capacity(bounds.len() + 1);
    let mut start = 0usize;
    let mut nov = 0f32;
    for &(b, v) in bounds.iter().chain(std::iter::once(&(m, 0.0))) {
        if b > start {
            sections.push(Section { start, end: b, novelty: nov, energy: mean(energy.get(start..b).unwrap_or(&[])) });
        }
        start = b;
        nov = v;
    }

    // phrases: every 4 bars from each section's first downbeat
    let mut phrases = Vec::new();
    for s in &sections {
        let Some(&first) = downbeats.iter().find(|&&d| d >= s.start && d < s.end) else { continue };
        let mut p = first;
        while p < s.end {
            phrases.push(p);
            p += 4 * meter;
        }
    }

    // hits and drops: an attack ≥ 6 dB louder (full band or low band) than the loudest of the
    // 4 beats before
    let mut hits = Vec::new();
    for i in 4..m {
        let from = i - 4;
        let peak = |v: &[f32]| v.get(from..i).unwrap_or(&[]).iter().copied().fold(f32::MIN, f32::max);
        let jump = (level_db[i] - peak(&level_db)).max(low_db[i] - peak(&low_db));
        if onset[i] < 0.75 * onset_med || jump < 6.0 {
            continue;
        }
        let before = level_db.get(i.saturating_sub(2 * meter)..i).unwrap_or(&[]);
        let after = level_db.get(i..(i + meter).min(m)).unwrap_or(&[]);
        let lin = |v: &[f32]| mean(&v.iter().map(|d| 10f32.powf(d / 10.0)).collect::<Vec<_>>());
        let drop = !before.is_empty() && db(lin(after) as f64) - db(lin(before) as f64) >= 6.0;
        hits.push(Hit { beat: i, strength: ((jump - 6.0) / 18.0 + 0.5).clamp(0.0, 1.0), drop });
    }

    // quiet runs
    let med_db = median(&level_db);
    let mut quiet = Vec::new();
    let mut run: Option<usize> = None;
    for i in 0..=m {
        let q = i < m && level_db[i] < med_db - 20.0;
        match (q, run) {
            (true, None) => run = Some(i),
            (false, Some(s)) => {
                quiet.push(Quiet { start: s, end: i });
                run = None;
            }
            _ => {}
        }
    }

    let beats = a.beats.iter().map(|&b| b.saturating_add(lead).min(a.len - 1)).collect();
    Ok(Cues { sample_rate: sr, len: a.len, beats, bpm: a.bpm, meter, downbeats, phrases, sections, energy, level_db, onset, hits, quiet })
}

// ------------------------------------------------------------------------------------- cut points

/// How a cut is paced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pacing {
    Calm,
    #[default]
    Cinematic,
    Energetic,
    Trailer,
}

impl Pacing {
    pub const ALL: [Pacing; 4] = [Pacing::Calm, Pacing::Cinematic, Pacing::Energetic, Pacing::Trailer];

    /// Stable id (`calm`, `cinematic`, `energetic`, `trailer`).
    pub fn id(self) -> &'static str {
        match self {
            Pacing::Calm => "calm",
            Pacing::Cinematic => "cinematic",
            Pacing::Energetic => "energetic",
            Pacing::Trailer => "trailer",
        }
    }

    pub fn from_id(s: &str) -> Option<Pacing> {
        Pacing::ALL.into_iter().find(|p| p.id().eq_ignore_ascii_case(s))
    }

    /// Shot length range in bars.
    pub fn shot_bars(self) -> (f64, f64) {
        match self {
            Pacing::Calm => (2.0, 8.0),
            Pacing::Cinematic => (1.0, 4.0),
            Pacing::Energetic => (0.5, 2.0),
            Pacing::Trailer => (0.5, 4.0),
        }
    }

    /// Shortest shot (seconds).
    pub fn shortest_s(self) -> f64 {
        match self {
            Pacing::Calm => 3.0,
            Pacing::Cinematic => 1.5,
            Pacing::Energetic => 0.6,
            Pacing::Trailer => 0.9,
        }
    }

    /// Weight of each cue kind.
    pub fn weight(self, k: CueKind) -> f32 {
        use CueKind::*;
        let w = match self {
            Pacing::Calm => [1.0, 0.6, 0.8, 0.6, 0.2, 0.0, 0.6],
            Pacing::Cinematic => [1.0, 0.9, 0.7, 0.6, 0.5, 0.1, 0.5],
            Pacing::Energetic => [0.8, 0.9, 0.5, 0.6, 0.7, 0.3, 0.3],
            Pacing::Trailer => [1.0, 1.0, 0.7, 0.6, 0.8, 0.1, 0.6],
        };
        w[match k {
            Section => 0,
            Drop => 1,
            Phrase => 2,
            Downbeat => 3,
            Hit => 4,
            Beat => 5,
            QuietEdge => 6,
        }]
    }
}

/// Why a beat is a good place to cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CueKind {
    Section,
    Drop,
    Phrase,
    Downbeat,
    Hit,
    Beat,
    QuietEdge,
}

impl CueKind {
    /// Stable id (`section`, `drop`, `phrase`, `downbeat`, `hit`, `beat`, `quietEdge`).
    pub fn id(self) -> &'static str {
        match self {
            CueKind::Section => "section",
            CueKind::Drop => "drop",
            CueKind::Phrase => "phrase",
            CueKind::Downbeat => "downbeat",
            CueKind::Hit => "hit",
            CueKind::Beat => "beat",
            CueKind::QuietEdge => "quietEdge",
        }
    }
}

/// What to optimise for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CutParams {
    pub pacing: Pacing,
    /// 0 … 100: preferred shot length from the longest to the shortest of the range.
    pub density: f64,
    /// Cut on section boundaries.
    pub sections: bool,
    /// Cut on hits and drops.
    pub hits: bool,
}

impl Default for CutParams {
    fn default() -> Self {
        CutParams { pacing: Pacing::Cinematic, density: 50.0, sections: true, hits: true }
    }
}

/// A suggested picture cut.
#[derive(Clone, Debug, PartialEq)]
pub struct CutPoint {
    pub beat: usize,
    /// Attack position (samples).
    pub sample: i64,
    pub score: f32,
    /// The cues at this beat, strongest reason first.
    pub kinds: Vec<CueKind>,
}

/// Cue kinds at each beat, with their strengths.
fn kinds_at(c: &Cues) -> Vec<Vec<(CueKind, f32)>> {
    let m = c.beats.len();
    let mut k: Vec<Vec<(CueKind, f32)>> = vec![Vec::new(); m];
    let mut add = |i: usize, kind: CueKind, s: f32| {
        if let Some(v) = k.get_mut(i) {
            v.push((kind, s));
        }
    };
    for s in c.sections.iter().skip(1) {
        add(s.start, CueKind::Section, 0.5 + 0.5 * s.novelty);
    }
    for h in &c.hits {
        add(h.beat, if h.drop { CueKind::Drop } else { CueKind::Hit }, 0.5 + 0.5 * h.strength);
    }
    for &p in &c.phrases {
        add(p, CueKind::Phrase, 1.0);
    }
    for &d in &c.downbeats {
        add(d, CueKind::Downbeat, 1.0);
    }
    for q in &c.quiet {
        add(q.start, CueKind::QuietEdge, 1.0);
        add(q.end, CueKind::QuietEdge, 1.0);
    }
    for i in 0..m {
        add(i, CueKind::Beat, 1.0);
    }
    k
}

/// Suggested cut points (beat boundaries where a new shot starts; the first shot starts at the
/// first beat and is not listed). Deterministic. Shots are within the profile's range except the
/// last, which runs to the end of the music.
pub fn cut_points(c: &Cues, p: CutParams) -> Vec<CutPoint> {
    let m = c.beats.len();
    if m < 2 || c.meter == 0 || !(c.bpm.is_finite() && c.bpm > 0.0) {
        return Vec::new();
    }
    let density = if p.density.is_finite() { p.density.clamp(0.0, 100.0) / 100.0 } else { 0.5 };
    let beat_s = 60.0 / c.bpm;
    let (blo, bhi) = p.pacing.shot_bars();
    let lo = ((blo * c.meter as f64).ceil().max((p.pacing.shortest_s() / beat_s).ceil())).max(1.0) as usize;
    let hi = ((bhi * c.meter as f64).round() as usize).max(lo);

    let kinds = kinds_at(c);
    let enabled = |k: CueKind| match k {
        CueKind::Section => p.sections,
        CueKind::Hit | CueKind::Drop => p.hits,
        _ => true,
    };
    let score: Vec<f32> = kinds.iter().map(|v| v.iter().filter(|(k, _)| enabled(*k)).map(|(k, s)| p.pacing.weight(*k) * s).sum()).collect();
    let pref = |i: usize| -> f64 {
        let e = c.energy.get(i).copied().unwrap_or(0.5) as f64;
        let mut t = density + 0.5 * (e - 0.5);
        if p.pacing == Pacing::Trailer {
            t += 0.4 * (i as f64 / m as f64 - 0.5);
        }
        let t = t.clamp(0.0, 1.0);
        ((hi as f64).ln() + t * ((lo as f64).ln() - (hi as f64).ln())).exp()
    };
    const LAMBDA: f64 = 1.0;
    let cost = |i: usize, g: usize| -> f64 {
        let r = (g as f64 / pref(i)).ln();
        LAMBDA * r * r
    };

    // f[j]: best total for a shot starting at beat j; node 0 is the start, node m the end
    let mut f = vec![f64::NEG_INFINITY; m + 1];
    let mut back = vec![usize::MAX; m + 1];
    f[0] = 0.0;
    for j in 1..m {
        if score[j] <= 0.0 || j < lo {
            continue;
        }
        for i in j.saturating_sub(hi)..=j.saturating_sub(lo) {
            if i >= j || !f[i].is_finite() {
                continue;
            }
            let v = f[i] + score[j] as f64 - cost(i, j - i);
            if v > f[j] {
                f[j] = v;
                back[j] = i;
            }
        }
    }
    // the last shot runs to the end: at least half the shortest shot, penalised past the longest
    for i in 0..m {
        if !f[i].is_finite() {
            continue;
        }
        let g = m - i;
        if i > 0 && g * 2 < lo {
            continue;
        }
        let over = if g > hi { 5.0 * (g - hi) as f64 / hi as f64 } else { 0.0 };
        let v = f[i] - cost(i, g) - over;
        if v > f[m] {
            f[m] = v;
            back[m] = i;
        }
    }
    let mut out = Vec::new();
    let mut j = back[m];
    let mut guard = 0;
    while j != usize::MAX && j != 0 && guard <= m {
        let mut ks: Vec<(CueKind, f32)> = kinds[j].iter().copied().filter(|(k, _)| enabled(*k) && p.pacing.weight(*k) > 0.0).collect();
        ks.sort_by(|a, b| (p.pacing.weight(b.0) * b.1).total_cmp(&(p.pacing.weight(a.0) * a.1)).then(a.0.cmp(&b.0)));
        out.push(CutPoint { beat: j, sample: c.beats[j], score: score[j], kinds: ks.into_iter().map(|k| k.0).collect() });
        j = back[j];
        guard += 1;
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remix::test_music;

    const SR: u32 = 22_050;

    fn cues_of(m: &[Vec<f32>]) -> Cues {
        let refs: Vec<&[f32]> = m.iter().map(Vec::as_slice).collect();
        analyze(&refs, SR).expect("cues")
    }

    #[test]
    fn downbeats_sit_on_bar_lines() {
        for bpm in [100.0, 120.0, 128.0] {
            let (m, truth) = test_music(SR, bpm, 24, 7);
            let c = cues_of(&m);
            assert_eq!(c.meter, 4, "{bpm} BPM");
            let bars: Vec<i64> = truth.iter().step_by(4).copied().collect();
            let tol = (0.03 * SR as f64) as i64;
            for &d in &c.downbeats {
                let s = c.beats[d];
                let err = bars.iter().map(|b| (b - s).abs()).min().unwrap();
                assert!(err <= tol, "{bpm} BPM: downbeat at {s} is {err} samples off a bar line");
            }
            assert!(c.downbeats.len() >= 22, "{} downbeats", c.downbeats.len());
            assert_eq!(c.bar_of(c.downbeats[3]), 4);
        }
    }

    /// Three-four test music: kick on 1, hats on 2 and 3, a new chord every bar.
    fn waltz(bpm: f64, bars: usize) -> Vec<Vec<f32>> {
        let beat = SR as f64 * 60.0 / bpm;
        let n = (bars as f64 * 3.0 * beat) as usize;
        let mut l = vec![0f32; n];
        let roots = [3, 8, 10, 0, 5, 10, 7, 0];
        let tau = std::f64::consts::TAU;
        let mut rng = 12345u32;
        for bar in 0..bars {
            let root = roots[bar % roots.len()];
            let b0 = (bar as f64 * 3.0 * beat) as usize;
            let b1 = (((bar + 1) as f64) * 3.0 * beat) as usize;
            for (i, s) in l.iter_mut().enumerate().take(b1.min(n)).skip(b0) {
                let t = (i - b0) as f64 / SR as f64;
                let mut v = 0.0;
                for nt in [root, root + 4, root + 7] {
                    v += (tau * 220.0 * 2f64.powf(nt as f64 / 12.0) * i as f64 / SR as f64).sin();
                }
                *s += (0.08 * (1.0 - (-t * 30.0).exp()) * v) as f32;
            }
            for q in 0..3 {
                let st = ((bar * 3 + q) as f64 * beat) as usize;
                for k in 0..((beat * 0.8) as usize).min(n.saturating_sub(st)) {
                    let t = k as f64 / SR as f64;
                    if q == 0 {
                        l[st + k] += (0.8 * (-t * 9.0).exp() * (tau * (50.0 + 90.0 * (-t * 25.0).exp()) * t).sin()) as f32;
                    } else if t < 0.05 {
                        rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        l[st + k] += 0.25 * (-t as f32 * 60.0).exp() * ((rng >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0);
                    }
                }
            }
        }
        vec![l.clone(), l]
    }

    #[test]
    fn three_four_is_found() {
        let c = cues_of(&waltz(120.0, 24));
        assert_eq!(c.meter, 3, "bpm {:.1}", c.bpm);
    }

    #[test]
    fn sections_at_form_changes() {
        // form A A B A B A, four bars each: changes at bars 8, 12, 16, 20 (not at bar 4: A → A)
        let (m, truth) = test_music(SR, 120.0, 24, 7);
        let c = cues_of(&m);
        let starts: Vec<i64> = c.sections.iter().skip(1).map(|s| c.beats[s.start]).collect();
        let tol = (0.3 * SR as f64) as i64; // within one beat (0.5 s)
        for bar in [8usize, 12, 16, 20] {
            let t = truth[bar * 4];
            assert!(starts.iter().any(|s| (s - t).abs() <= tol), "no section start near bar {bar}: {starts:?}");
        }
        let t4 = truth[16];
        assert!(starts.iter().all(|s| (s - t4).abs() > tol), "A → A at bar 4 is not a section: {starts:?}");
        // sections tile the beats
        assert_eq!(c.sections.first().map(|s| s.start), Some(0));
        assert_eq!(c.sections.last().map(|s| s.end), Some(c.beats.len()));
        assert!(c.sections.windows(2).all(|w| w[0].end == w[1].start));
        // phrases start every section
        assert!(c.sections.iter().all(|s| c.phrases.iter().any(|&p| p >= s.start && p < s.start + c.meter)));
    }

    /// 24 bars at 120 BPM with bars 10–11 nearly silent and a boom on bar 12's downbeat.
    fn with_break_and_boom() -> (Vec<Vec<f32>>, Vec<i64>) {
        let (mut m, truth) = test_music(SR, 120.0, 24, 7);
        let (q0, q1) = (truth[40] as usize, truth[48] as usize);
        let boom = truth[48] as usize;
        for ch in &mut m {
            for v in ch.iter_mut().take(q1).skip(q0) {
                *v *= 0.003;
            }
            for k in 0..(SR as usize / 2) {
                let t = k as f64 / SR as f64;
                if let Some(v) = ch.get_mut(boom + k) {
                    *v += (1.5 * (-t * 5.0).exp() * (std::f64::consts::TAU * 45.0 * t).sin()) as f32;
                }
            }
        }
        (m, truth)
    }

    #[test]
    fn hits_drops_and_quiet() {
        let (m, truth) = with_break_and_boom();
        let c = cues_of(&m);
        let tol = (0.05 * SR as f64) as i64;
        let near = |beat: usize, t: i64| (c.beats[beat] - t).abs() <= tol;
        let h = c.hits.iter().find(|h| near(h.beat, truth[48])).expect("hit on bar 12");
        assert!(h.drop, "the boom after the break is a drop: {h:?}");
        assert!(c.quiet.iter().any(|q| c.beats[q.start] <= truth[41] && c.beats[q.end.saturating_sub(1)] >= truth[46]), "quiet bars 10–11: {:?}", c.quiet);
        // the plain groove has no hits
        let (plain, _) = test_music(SR, 120.0, 24, 7);
        let pc = cues_of(&plain);
        assert!(pc.hits.is_empty());
    }

    #[test]
    fn cut_points_follow_the_profile() {
        let (m, _) = with_break_and_boom();
        let c = cues_of(&m);
        let mut counts = Vec::new();
        for pacing in Pacing::ALL {
            let pts = cut_points(&c, CutParams { pacing, ..CutParams::default() });
            let beat_s = 60.0 / c.bpm;
            let (blo, bhi) = pacing.shot_bars();
            let lo = ((blo * 4.0).ceil().max((pacing.shortest_s() / beat_s).ceil())).max(1.0) as usize;
            let hi = ((bhi * 4.0).round() as usize).max(lo);
            let mut prev = 0;
            for p in &pts {
                let g = p.beat - prev;
                assert!(g >= lo && g <= hi, "{pacing:?}: shot of {g} beats outside {lo}…{hi}");
                assert!(!p.kinds.is_empty() && p.score > 0.0);
                assert_eq!(p.sample, c.beats[p.beat]);
                prev = p.beat;
            }
            if pacing == Pacing::Calm {
                assert!(pts.iter().all(|p| p.kinds.iter().any(|k| *k != CueKind::Beat)), "Calm never cuts on a plain beat");
            }
            if pacing != Pacing::Calm {
                // the drop is used
                let drop = c.hits.iter().find(|h| h.drop).unwrap().beat;
                assert!(pts.iter().any(|p| p.beat == drop), "{pacing:?} cuts on the drop");
            }
            counts.push(pts.len());
            eprintln!("{pacing:?}: {} cuts", pts.len());
        }
        assert!(counts[0] < counts[1] && counts[1] < counts[2], "Calm < Cinematic < Energetic: {counts:?}");
        // density
        let few = cut_points(&c, CutParams { density: 0.0, ..CutParams::default() }).len();
        let many = cut_points(&c, CutParams { density: 100.0, ..CutParams::default() }).len();
        assert!(few < many, "density 0 → {few}, 100 → {many}");
        // deterministic
        assert_eq!(cut_points(&c, CutParams::default()), cut_points(&cues_of(&with_break_and_boom().0), CutParams::default()));
        // switches
        let no_hits = cut_points(&c, CutParams { hits: false, ..CutParams::default() });
        assert!(no_hits.iter().all(|p| !p.kinds.contains(&CueKind::Hit) && !p.kinds.contains(&CueKind::Drop)));
    }

    #[test]
    fn inconsistent_analysis_is_refused() {
        let (m, _) = test_music(SR, 120.0, 16, 7);
        let refs: Vec<&[f32]> = m.iter().map(Vec::as_slice).collect();
        let a = remix::analyze(&refs, SR).unwrap();
        let mut bad = a.clone();
        bad.sim.pop();
        assert!(matches!(from_analysis(&bad, &refs), Err(CueError::Inconsistent(_))));
        let mut bad = a.clone();
        bad.beats.swap(2, 3);
        assert!(matches!(from_analysis(&bad, &refs), Err(CueError::Inconsistent(_))));
        let mut bad = a.clone();
        bad.len += 1;
        assert!(matches!(from_analysis(&bad, &refs), Err(CueError::Inconsistent(_))));
        let mut bad = a.clone();
        bad.bpm = f64::NAN;
        assert!(from_analysis(&bad, &refs).is_err());
        let mut bad = a.clone();
        bad.sim.iter_mut().for_each(|v| *v = f32::NAN);
        assert!(from_analysis(&bad, &refs).is_ok(), "non-finite similarity counts as 0");
        assert!(from_analysis(&a, &refs[..1]).is_ok());
        let short = &refs[0][..refs[0].len() / 2];
        assert!(from_analysis(&a, &[short]).is_err());
    }

    #[test]
    fn hostile_input_never_panics() {
        let (m, _) = test_music(SR, 120.0, 12, 3);
        let base = m[0].clone();
        let mut rng = 99u32;
        let mut next = move || {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            rng
        };
        for round in 0..24 {
            let mut x = base.clone();
            match round % 4 {
                0 => x.truncate((next() as usize) % x.len()),
                1 => {
                    for _ in 0..200 {
                        let i = next() as usize % x.len();
                        x[i] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX][next() as usize % 4];
                    }
                }
                2 => x.iter_mut().for_each(|v| *v = (next() as f32 / u32::MAX as f32) * 2.0 - 1.0),
                _ => {
                    for _ in 0..50 {
                        let i = next() as usize % x.len();
                        x[i] = f32::from_bits(next());
                    }
                }
            }
            let r = std::panic::catch_unwind(|| {
                if let Ok(c) = analyze(&[&x], SR) {
                    for pacing in Pacing::ALL {
                        for density in [f64::NAN, -5.0, 0.0, 50.0, 1e9] {
                            let _ = cut_points(&c, CutParams { pacing, density, sections: true, hits: true });
                        }
                    }
                }
                // a truncated cue set
                if let Ok(mut c) = analyze(&[&base], SR) {
                    c.energy.truncate(1);
                    c.beats.truncate(c.beats.len() / 2);
                    c.meter = 0;
                    let _ = cut_points(&c, CutParams::default());
                    c.meter = 4;
                    let _ = cut_points(&c, CutParams::default());
                }
            });
            assert!(r.is_ok(), "panic in round {round}");
        }
        assert!(analyze(&[], SR).is_err());
        assert!(analyze(&[&[]], SR).is_err());
    }

    #[test]
    fn pacing_ids_round_trip() {
        for p in Pacing::ALL {
            assert_eq!(Pacing::from_id(p.id()), Some(p));
        }
        assert_eq!(Pacing::from_id("TRAILER"), Some(Pacing::Trailer));
        assert_eq!(Pacing::from_id("nope"), None);
    }
}
