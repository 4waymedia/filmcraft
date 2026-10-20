//! Build a trailer from music (beyond Premiere; assisted editing, #1934): a song plus a set of
//! shots becomes a new sequence cut to the music.
//!
//! | command | what it does |
//! |---|---|
//! | `sequence.buildTrailer {music?, items?, seconds?, pacing="trailer", density=0..100=50, name?, dryRun=false}` | plans and (unless `dryRun`) builds a new sequence in one undo step ("Build Trailer") |
//! | `sequence.buildTrailer.30` / `.60` / `.90` / `.full` | Sequence ▸ Build Trailer from Music ▸ 30 / 60 / 90 Seconds / Full Song, on the Project selection |
//!
//! - **Music:** `music` (an item id), or the selected Project item that has audio and no video.
//!   At most [`crate::music_cuts::MAX_SECONDS`] of it is analysed.
//! - **Shots:** `items`, or the other selected Project items (video clips and stills), used in
//!   order and reused in turn when there are more shots than items.
//! - **Length:** with `seconds`, the music ends at the section start, phrase start or downbeat
//!   nearest the target (sections preferred), so the cut ends on a musical boundary rather than
//!   mid-phrase; without it, the whole song.
//! - **Cuts:** [`filmcraft_audio_dsp::cues::cut_points`] for the pacing and density; each shot
//!   fills the span between two cuts, snapped to the sequence's frames, so shots butt with no
//!   gaps. A video clip contributes a window of the shot's length, taken from a different part of
//!   the clip each time it is reused (middle first); a clip shorter than its shot is used whole
//!   and the shot is reported as `short` (the gap stays visible on the timeline).
//! - **Result:** music on A1, shots on V1 (picture only), blue section range markers and hit /
//!   drop markers as in [`crate::music_cuts`].
//!
//! The story arc (Hauge's six stages for 90 s and longer) and shot ranking by motion come next;
//! this command already returns everything they build on.

use filmcraft_audio_dsp::cues::{self, CutParams, Pacing};
use filmcraft_media::MediaKind;
use filmcraft_project::{ItemId, ItemKind, Label, Marker, MarkerId, MarkerKind, SequenceSettings};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, default_seq_settings_for, has_project_selection, place_item};
use crate::music_cuts::{MAX_SECONDS, TAG};
use crate::{EngineError, Result, Session};

const CMD: &str = "sequence.buildTrailer";
/// The analysis rate (the cut maths is done in samples, then turned into ticks).
const SR: u32 = 48_000;
/// Most shots one build places (a guard against a pathological plan).
const MAX_SHOTS: usize = 2_000;

/// One planned shot.
#[derive(Clone, Debug, PartialEq)]
pub struct Shot {
    pub item: ItemId,
    /// Media time of the first frame used.
    pub source_in: Tick,
    /// Sequence position and length.
    pub at: Tick,
    pub len: Tick,
    /// The item is shorter than the shot: it is used whole and the rest of the shot stays empty.
    pub short: bool,
}

/// What a build will do.
#[derive(Clone, Debug)]
pub struct Plan {
    pub music: ItemId,
    pub music_len: Tick,
    pub settings: SequenceSettings,
    pub shots: Vec<Shot>,
    pub cues: cues::Cues,
    pub pacing: Pacing,
}

fn is_still(it: &filmcraft_project::ProjectItem) -> bool {
    it.as_media().is_some_and(|m| matches!(m.info.kind, MediaKind::Still))
}

fn has_video(it: &filmcraft_project::ProjectItem) -> bool {
    it.as_media().is_some_and(|m| m.info.video.is_some())
}

fn is_music(it: &filmcraft_project::ProjectItem) -> bool {
    matches!(it.kind, ItemKind::Media(_)) && it.has_audio() && !has_video(it)
}

fn ids_p(p: &Value, k: &str) -> Result<Option<Vec<ItemId>>> {
    match p.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            a.iter().map(|v| v.as_u64().map(ItemId).ok_or_else(|| bad(CMD, format!("`{k}` must be a list of item ids")))).collect::<Result<Vec<_>>>().map(Some)
        }
        Some(_) => Err(bad(CMD, format!("`{k}` must be a list of item ids"))),
    }
}

/// The music and the shots: from the parameters, else from the Project selection.
fn sources(s: &Session, p: &Value) -> Result<(ItemId, Vec<ItemId>)> {
    let music = match p.get("music") {
        None | Some(Value::Null) => s
            .state
            .project_selection
            .iter()
            .copied()
            .find(|i| s.project.item(*i).is_some_and(is_music))
            .ok_or_else(|| bad(CMD, "select a music file (audio only) and the clips to use in the Project panel"))?,
        Some(v) => ItemId(v.as_u64().ok_or_else(|| bad(CMD, "`music` must be an item id"))?),
    };
    let m = s.project.item(music).ok_or_else(|| bad(CMD, "no such music item"))?;
    if !matches!(m.kind, ItemKind::Media(_)) || !m.has_audio() {
        return Err(bad(CMD, "the music item has no audio"));
    }
    let items = match ids_p(p, "items")? {
        Some(v) => v,
        None => s.state.project_selection.iter().copied().filter(|i| *i != music).collect(),
    };
    let mut shots = Vec::new();
    for id in items {
        let it = s.project.item(id).ok_or_else(|| bad(CMD, format!("no such item {}", id.0)))?;
        if id != music && (has_video(it) || is_still(it)) && !shots.contains(&id) {
            shots.push(id);
        }
    }
    if shots.is_empty() {
        return Err(bad(CMD, "select some video clips or stills to cut to the music"));
    }
    Ok((music, shots))
}

/// Where the music ends for a `target` length (samples): the boundary nearest the target, with
/// section starts preferred over phrase starts over downbeats; the song's end if it is nearer.
pub fn ending(c: &cues::Cues, target: i64) -> i64 {
    let sr = c.sample_rate.max(1) as f64;
    let mut cands: Vec<(i64, f64)> = vec![(c.len, 0.0)];
    for s in c.sections.iter().skip(1) {
        cands.push((c.beats.get(s.start).copied().unwrap_or(c.len), 0.0));
    }
    for &ph in &c.phrases {
        cands.push((c.beats.get(ph).copied().unwrap_or(c.len), 0.75));
    }
    for &d in &c.downbeats {
        cands.push((c.beats.get(d).copied().unwrap_or(c.len), 1.5));
    }
    cands
        .into_iter()
        .filter(|(t, _)| *t * 2 >= target && *t > 0)
        .map(|(t, pen)| (t, (t - target).abs() as f64 / sr + pen))
        .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
        .map(|(t, _)| t)
        .unwrap_or(c.len)
}

/// Where in a clip of `dur` a window of `len` starts on its `use_n`-th use: the middle first, then
/// spread out.
fn window(dur: Tick, len: Tick, use_n: usize) -> Tick {
    const SPOTS: [f64; 6] = [0.5, 0.25, 0.75, 0.1, 0.9, 0.4];
    let spare = dur - len;
    if spare <= Tick::ZERO {
        return Tick::ZERO;
    }
    Tick((spare.0 as f64 * SPOTS[use_n % SPOTS.len()]) as i64)
}

fn plan(s: &Session, p: &Value) -> Result<Plan> {
    let pacing = match p.get("pacing") {
        None | Some(Value::Null) => Pacing::Trailer,
        Some(v) => v.as_str().and_then(Pacing::from_id).ok_or_else(|| bad(CMD, "`pacing` must be calm, cinematic, energetic or trailer"))?,
    };
    let density = match p.get("density") {
        None | Some(Value::Null) => 50.0,
        Some(v) => v.as_f64().filter(|d| d.is_finite() && (0.0..=100.0).contains(d)).ok_or_else(|| bad(CMD, "`density` must be a number from 0 to 100"))?,
    };
    let seconds = match p.get("seconds") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .filter(|x| x.is_finite() && *x >= 5.0 && *x <= MAX_SECONDS as f64)
                .ok_or_else(|| bad(CMD, format!("`seconds` must be from 5 to {MAX_SECONDS}")))?,
        ),
    };
    let (music, items) = sources(s, p)?;
    let mi = s.project.item(music).ok_or_else(|| bad(CMD, "no such music item"))?;
    let full = mi.duration();
    if full <= Tick::ZERO {
        return Err(bad(CMD, "the music is empty"));
    }
    if full > Tick::from_seconds_f64(MAX_SECONDS as f64) {
        return Err(bad(CMD, format!("the music is longer than {} minutes; trim it first", MAX_SECONDS / 60)));
    }
    // the sequence follows the first video clip (else the defaults)
    let settings = items
        .iter()
        .filter_map(|i| s.project.item(*i))
        .find_map(|i| i.as_media().filter(|m| m.info.video.is_some()).map(default_seq_settings_for))
        .unwrap_or_default();
    let rate: FrameRate = settings.frame_rate;
    if rate.frame_duration() <= Tick::ZERO {
        return Err(bad(CMD, "the clips have no usable frame rate"));
    }

    let channels = crate::remix::read_audio_at(s, music, Tick::ZERO, full, SR)?;
    let refs: Vec<&[f32]> = channels.iter().map(Vec::as_slice).collect();
    let cue = cues::analyze(&refs, SR).map_err(|e| EngineError::Other(format!("cannot cut to this music: {e}")))?;
    let end_smp = match seconds {
        Some(sec) => ending(&cue, (sec * SR as f64) as i64),
        None => cue.len,
    };
    let to_tick = |smp: i64| rate.snap(Tick::from_units(smp, SR as i64));
    let music_len = to_tick(end_smp).max(rate.frame_duration());

    // shot boundaries: 0, the cuts inside the music, the end; frame-snapped, strictly increasing
    let mut bounds = vec![Tick::ZERO];
    for pt in cues::cut_points(&cue, CutParams { pacing, density, sections: true, hits: true }) {
        let t = to_tick(pt.sample);
        if t > *bounds.last().unwrap_or(&Tick::ZERO) && t + rate.frame_duration() < music_len {
            bounds.push(t);
        }
    }
    bounds.push(music_len);
    if bounds.len() > MAX_SHOTS + 1 {
        return Err(bad(CMD, "too many shots for one build; use a calmer pacing or a lower density"));
    }

    let mut uses = vec![0usize; items.len()];
    let mut shots = Vec::with_capacity(bounds.len());
    for (k, w) in bounds.windows(2).enumerate() {
        let (at, len) = (w[0], w[1] - w[0]);
        let n = k % items.len();
        let id = items[n];
        let it = s.project.item(id).ok_or_else(|| bad(CMD, "no such item"))?;
        let shot = if is_still(it) {
            Shot { item: id, source_in: Tick::ZERO, at, len, short: false }
        } else {
            let dur = it.duration();
            if dur >= len {
                Shot { item: id, source_in: rate.snap(window(dur, len, uses[n])), at, len, short: false }
            } else {
                Shot { item: id, source_in: Tick::ZERO, at, len: dur.max(rate.frame_duration()), short: true }
            }
        };
        uses[n] += 1;
        shots.push(shot);
    }
    Ok(Plan { music, music_len, settings, shots, cues: cue, pacing })
}

fn report(pl: &Plan, sequence: Option<u64>) -> Value {
    let secs = |t: Tick| t.seconds();
    json!({
        "sequence": sequence,
        "music": pl.music.0,
        "pacing": pl.pacing.id(),
        "bpm": pl.cues.bpm,
        "seconds": secs(pl.music_len),
        "shots": pl.shots.iter().map(|sh| json!({"item": sh.item.0, "at": sh.at.0, "duration": sh.len.0, "sourceIn": sh.source_in.0, "short": sh.short})).collect::<Vec<_>>(),
        "short": pl.shots.iter().filter(|sh| sh.short).count(),
        "sections": pl.cues.sections.len(),
        "hits": pl.cues.hits.len(),
    })
}

fn build(s: &mut Session, p: &Value) -> Result<Value> {
    let dry = match p.get("dryRun") {
        None | Some(Value::Null) => false,
        Some(v) => v.as_bool().ok_or_else(|| bad(CMD, "`dryRun` must be true or false"))?,
    };
    let name = match p.get("name") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str().filter(|n| !n.trim().is_empty() && n.chars().count() <= 200).ok_or_else(|| bad(CMD, "`name` must be a non-empty text"))?.to_string(),
        ),
    };
    let pl = plan(s, p)?;
    if dry {
        return Ok(report(&pl, None));
    }
    let music_name = s.project.item(pl.music).map(|i| i.name.clone()).unwrap_or_default();
    let name = name.unwrap_or_else(|| format!("Trailer - {music_name}"));
    let seq = s.edit_group("Build Trailer", |s| {
        let st = &pl.settings;
        let r = s.execute(
            "file.newSequence",
            json!({"name": name, "width": st.width, "height": st.height, "fps": st.frame_rate.as_f64(), "sampleRate": st.sample_rate, "video": 1, "audio": 1}),
        )?;
        let seq = r["sequence"].as_u64().ok_or_else(|| EngineError::Other("the new sequence has no id".into()))?;
        let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
        let (v1, a1) = (
            q.video_tracks.first().map(|t| t.id).ok_or_else(|| EngineError::Other("the new sequence has no video track".into()))?,
            q.audio_tracks.first().map(|t| t.id).ok_or_else(|| EngineError::Other("the new sequence has no audio track".into()))?,
        );
        place_item(s, pl.music, TimeRange::new(Tick::ZERO, pl.music_len), Tick::ZERO, None, Some(a1), false, CMD, None)?;
        for sh in &pl.shots {
            place_item(s, sh.item, TimeRange::new(sh.source_in, sh.len), sh.at, Some(v1), None, false, CMD, None)?;
        }
        // sections and hits as markers
        let sr = SR as i64;
        let c = &pl.cues;
        let end = pl.music_len;
        let rate = pl.settings.frame_rate;
        let at = |b: usize| rate.snap(Tick::from_units(c.beats.get(b).copied().unwrap_or(c.len), sr));
        let mut marks: Vec<(Tick, Tick, String, String, Label)> = Vec::new();
        for (k, sec) in c.sections.iter().enumerate() {
            let t0 = if k == 0 { Tick::ZERO } else { at(sec.start) };
            let t1 = if sec.end >= c.beats.len() { end } else { at(sec.end).min(end) };
            if t0 < end && t1 > t0 {
                marks.push((
                    t0,
                    t1 - t0,
                    format!("Section {} · {:.0} BPM", (b'A' + (k % 26) as u8) as char, c.bpm),
                    format!("{TAG}section · energy {:.2}", sec.energy),
                    Label::Blue,
                ));
            }
        }
        for h in &c.hits {
            let t = at(h.beat);
            if t < end {
                let (n, col) = if h.drop { ("Drop", Label::Magenta) } else { ("Hit", Label::Rose) };
                marks.push((t, Tick::ZERO, n.to_string(), format!("{TAG}{} · bar {}", n.to_lowercase(), c.bar_of(h.beat)), col));
            }
        }
        s.edit_sequence("Build Trailer", |q, ctx, _| {
            for (t, d, n, cm, col) in marks {
                q.markers.push(Marker { id: MarkerId(ctx.alloc()), start: t, duration: d, name: n, comment: cm, kind: MarkerKind::Comment, color: col });
            }
            q.markers.sort_by_key(|m| m.start);
            Ok(())
        })?;
        Ok(seq)
    })?;
    Ok(report(&pl, Some(seq)))
}

fn with_seconds(s: &mut Session, p: &Value, seconds: Option<f64>) -> Result<Value> {
    let mut q = if p.is_object() { p.clone() } else { json!({}) };
    q["seconds"] = seconds.map(|x| json!(x)).unwrap_or(Value::Null);
    build(s, &q)
}

fn can_build_from_selection(s: &Session) -> std::result::Result<(), String> {
    has_project_selection(s)?;
    let sel = &s.state.project_selection;
    if !sel.iter().any(|i| s.project.item(*i).is_some_and(is_music)) {
        return Err("select a music file (audio only) in the Project panel".into());
    }
    if !sel.iter().any(|i| s.project.item(*i).is_some_and(|it| has_video(it) || is_still(it))) {
        return Err("also select the video clips or stills to use".into());
    }
    Ok(())
}

pub fn commands() -> Vec<CommandSpec> {
    const MENU: &[&str] = &["Sequence", "Build Trailer from Music"];
    const PARAMS: &str = r#"{"music":itemId?,"items":[itemId]?,"pacing":"calm"|"cinematic"|"energetic"|"trailer"="trailer","density":0..100=50,"name":str?,"dryRun":bool=false}"#;
    vec![
        CommandSpec {
            id: CMD,
            label: "Build Trailer from Music",
            menu: &[],
            shortcut: None,
            params: r#"{"music":itemId?,"items":[itemId]?,"seconds":5..600?,"pacing":"calm"|"cinematic"|"energetic"|"trailer"="trailer","density":0..100=50,"name":str?,"dryRun":bool=false}"#,
            enabled: |_| Ok(()),
            run: build,
            journal: true,
        },
        CommandSpec {
            id: "sequence.buildTrailer.30",
            label: "30 Seconds",
            menu: MENU,
            shortcut: None,
            params: PARAMS,
            enabled: can_build_from_selection,
            run: |s, p| with_seconds(s, p, Some(30.0)),
            journal: true,
        },
        CommandSpec {
            id: "sequence.buildTrailer.60",
            label: "60 Seconds",
            menu: MENU,
            shortcut: None,
            params: PARAMS,
            enabled: can_build_from_selection,
            run: |s, p| with_seconds(s, p, Some(60.0)),
            journal: true,
        },
        CommandSpec {
            id: "sequence.buildTrailer.90",
            label: "90 Seconds",
            menu: MENU,
            shortcut: None,
            params: PARAMS,
            enabled: can_build_from_selection,
            run: |s, p| with_seconds(s, p, Some(90.0)),
            journal: true,
        },
        CommandSpec {
            id: "sequence.buildTrailer.full",
            label: "Full Song",
            menu: MENU,
            shortcut: None,
            params: PARAMS,
            enabled: can_build_from_selection,
            run: |s, p| with_seconds(s, p, None),
            journal: true,
        },
    ]
}
