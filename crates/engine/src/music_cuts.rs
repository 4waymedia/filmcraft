//! Suggest cuts from music (beyond Premiere): analyse a music clip and mark where picture cuts
//! land on the music ([`filmcraft_audio_dsp::cues`]).
//!
//! | command | what it does |
//! |---|---|
//! | `audio.suggestCuts {clip?, pacing="cinematic", density=0..100=50, sections=true, hits=true, apply="markers", replace=true}` | analyses the music clip (default: the selected audio clip) and, with `apply: "markers"`, adds sequence markers in one undo step ("Suggest Cuts"); `apply: "none"` only reports |
//! | `audio.suggestCuts.calm` / `.cinematic` / `.energetic` / `.trailer` | Markers ▸ Suggest Cuts from Music ▸ Calm / Cinematic / Energetic / Trailer: the same on the selected audio clip with that pacing |
//!
//! `pacing` is `calm`, `cinematic`, `energetic` or `trailer` (shot ranges in
//! [`filmcraft_audio_dsp::cues::Pacing`]). `density` moves the preferred shot length from the
//! longest (0) to the shortest (100) of the range.
//!
//! Markers written (sequence time, over the clip):
//!
//! | cue | marker | name | colour |
//! |---|---|---|---|
//! | cut point | point, Comment | **empty** | Green |
//! | section | range, Comment, to the next section | `Section B · 120 BPM` | Blue |
//! | hit / drop | point, Comment | `Hit` / `Drop` | Rose / Magenta |
//!
//! Cut markers are unnamed Comment markers on purpose: **Clip ▸ Automate to Sequence** with
//! `placement: "unnumberedMarkers"` places the selected Project items exactly there, so a bin of
//! shots becomes a cut on the music. Every marker's comment starts with [`TAG`]; with `replace`
//! (default) markers carrying the tag inside the clip's range are removed first, so running the
//! command again doesn't pile up markers, and other markers are never touched.
//!
//! The beat analysis is the Remix analysis (shared cache, [`crate::remix::analysis`]). Remixed,
//! rate-changed and reversed clips are refused: their timeline doesn't follow the source audio.

use filmcraft_audio_dsp::cues::{self, CueKind, CutParams, Pacing};
use filmcraft_project::{ClipId, ItemKind, Label, Marker, MarkerId, MarkerKind, TrackKind};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, has_seq};
use crate::{EngineError, Result, Session};

/// First characters of the comment of every marker this command writes.
pub const TAG: &str = "[music] ";

const CMD: &str = "audio.suggestCuts";

/// Longest stretch of music analysed (the decoded audio is held in memory).
pub const MAX_SECONDS: i64 = 600;

fn check_clip(s: &Session, c: ClipId) -> std::result::Result<(), String> {
    let seq = s.active_sequence().ok_or("no sequence is open")?;
    let (t, it) = seq.find_item(c).ok_or("no such clip")?;
    if seq.track(t).is_none_or(|t| t.kind != TrackKind::Audio) {
        return Err("Suggest Cuts works on audio clips".into());
    }
    let item = s.project.item(it.item).ok_or("the clip has no media")?;
    if !matches!(item.kind, ItemKind::Media(_)) || !item.has_audio() {
        return Err("the clip has no audio media".into());
    }
    if filmcraft_render::remix::is_remixed(it) {
        return Err("the clip is remixed; revert the remix or suggest cuts on the original".into());
    }
    if it.reverse || !(it.speed.is_finite() && (it.speed - 1.0).abs() < 1e-9) {
        return Err("Suggest Cuts needs the clip at 100 % speed, not reversed".into());
    }
    if it.duration <= Tick::ZERO {
        return Err("the clip is empty".into());
    }
    if it.duration > Tick::from_seconds_f64(MAX_SECONDS as f64) {
        return Err(format!("Suggest Cuts analyses at most {} minutes of music; trim or split the clip", MAX_SECONDS / 60));
    }
    Ok(())
}

fn target_clip(s: &Session, p: &Value) -> Result<ClipId> {
    let c = match p.get("clip") {
        None | Some(Value::Null) => crate::remix::selected_audio(s).ok_or_else(|| bad(CMD, "select an audio clip or pass `clip`"))?,
        Some(v) => ClipId(v.as_u64().ok_or_else(|| bad(CMD, "`clip` must be a clip id"))?),
    };
    check_clip(s, c).map_err(|e| bad(CMD, e))?;
    Ok(c)
}

fn bool_param(p: &Value, k: &str, default: bool) -> Result<bool> {
    match p.get(k) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_bool().ok_or_else(|| bad(CMD, format!("`{k}` must be true or false"))),
    }
}

fn params(p: &Value) -> Result<(CutParams, bool, bool)> {
    let pacing = match p.get("pacing") {
        None | Some(Value::Null) => Pacing::default(),
        Some(v) => v.as_str().and_then(Pacing::from_id).ok_or_else(|| bad(CMD, "`pacing` must be calm, cinematic, energetic or trailer"))?,
    };
    let density = match p.get("density") {
        None | Some(Value::Null) => 50.0,
        Some(v) => v.as_f64().filter(|d| d.is_finite() && (0.0..=100.0).contains(d)).ok_or_else(|| bad(CMD, "`density` must be a number from 0 to 100"))?,
    };
    let apply = match p.get("apply").and_then(Value::as_str) {
        None if p.get("apply").is_none_or(Value::is_null) => true,
        Some("markers") => true,
        Some("none") => false,
        _ => return Err(bad(CMD, "`apply` must be \"markers\" or \"none\"")),
    };
    let cp = CutParams { pacing, density, sections: bool_param(p, "sections", true)?, hits: bool_param(p, "hits", true)? };
    Ok((cp, apply, bool_param(p, "replace", true)?))
}

fn section_name(k: usize) -> String {
    // A … Z, then AA, AB … (more than 26 sections is unusual but must not panic)
    let mut n = k;
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (n % 26) as u8) as char);
        if n < 26 {
            break;
        }
        n = n / 26 - 1;
    }
    s
}

fn suggest(s: &mut Session, p: &Value) -> Result<Value> {
    let (cp, apply, replace) = params(p)?;
    let c = target_clip(s, p)?;
    let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let (_, it) = seq.find_item(c).ok_or_else(|| bad(CMD, "no such clip"))?;
    let (item, source_in, start, dur) = (it.item, it.source_in, it.start, it.duration);
    let sr = crate::remix::analysis_rate(s);
    let channels = crate::remix::read_audio(s, item, source_in, dur)?;
    let refs: Vec<&[f32]> = channels.iter().map(Vec::as_slice).collect();
    let cue = match crate::remix::analysis(s, item, source_in, dur) {
        Ok(a) => cues::from_analysis(&a, &refs).or_else(|_| cues::analyze(&refs, sr)),
        Err(_) => cues::analyze(&refs, sr),
    }
    .map_err(|e| EngineError::Other(format!("cannot suggest cuts: {e}")))?;
    let points = cues::cut_points(&cue, cp);

    let srl = sr as i64;
    let at = |sample: i64| start + Tick::from_units(sample.clamp(0, cue.len), srl);
    let end = start + dur;
    let beat_at = |b: usize| cue.beats.get(b).copied().unwrap_or(cue.len);
    let bpm = cue.bpm;

    // the markers, in sequence time
    let mut new: Vec<(Tick, Tick, String, String, Label)> = Vec::new();
    let mut report_sections = Vec::new();
    for (k, sec) in cue.sections.iter().enumerate() {
        let t0 = if k == 0 { start } else { at(beat_at(sec.start)) };
        let t1 = if sec.end >= cue.beats.len() { end } else { at(beat_at(sec.end)) };
        let name = format!("Section {} · {:.0} BPM", section_name(k), bpm);
        report_sections.push(json!({"name": name, "start": t0.0, "duration": (t1 - t0).0, "bar": cue.bar_of(sec.start), "energy": sec.energy}));
        if cp.sections && t1 > t0 {
            new.push((t0, t1 - t0, name, format!("{TAG}section · energy {:.2}", sec.energy), Label::Blue));
        }
    }
    let mut report_hits = Vec::new();
    for h in &cue.hits {
        let t = at(beat_at(h.beat));
        let bar = cue.bar_of(h.beat);
        report_hits.push(json!({"time": t.0, "bar": bar, "strength": h.strength, "drop": h.drop}));
        if cp.hits {
            let (name, color) = if h.drop { ("Drop", Label::Magenta) } else { ("Hit", Label::Rose) };
            new.push((t, Tick::ZERO, name.to_string(), format!("{TAG}{} · bar {bar}", name.to_lowercase()), color));
        }
    }
    let mut report_cuts = Vec::new();
    for pt in &points {
        let t = at(pt.sample);
        let bar = cue.bar_of(pt.beat);
        let why: Vec<&str> = pt.kinds.iter().map(|k| k.id()).collect();
        report_cuts.push(json!({"time": t.0, "bar": bar, "score": pt.score, "reasons": why}));
        let main = pt.kinds.first().copied().unwrap_or(CueKind::Beat).id();
        new.push((
            t,
            Tick::ZERO,
            String::new(),
            format!("{TAG}cut · bar {bar} · {} · score {:.2}", if why.is_empty() { main.to_string() } else { why.join(" + ") }, pt.score),
            Label::Green,
        ));
    }

    let mut added = 0usize;
    let mut removed = 0usize;
    if apply {
        let (a, r) = s.edit_sequence("Suggest Cuts", |q, ctx, _| {
            let before = q.markers.len();
            if replace {
                q.markers.retain(|m| !(m.comment.starts_with(TAG) && m.start >= start && m.start < end));
            }
            let removed = before - q.markers.len();
            for (t, d, name, comment, color) in new {
                q.markers.push(Marker { id: MarkerId(ctx.alloc()), start: t, duration: d, name, comment, kind: MarkerKind::Comment, color });
                added += 1;
            }
            q.markers.sort_by_key(|m| m.start);
            Ok((added, removed))
        })?;
        added = a;
        removed = r;
    }
    Ok(json!({
        "clip": c.0,
        "pacing": cp.pacing.id(),
        "bpm": bpm,
        "meter": cue.meter,
        "sections": report_sections,
        "hits": report_hits,
        "cuts": report_cuts,
        "applied": apply,
        "markersAdded": added,
        "markersRemoved": removed,
    }))
}

fn can_suggest(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)
}

/// The menu items: Markers ▸ Suggest Cuts from Music ▸ <pacing> on the selected audio clip.
fn can_suggest_selected(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)?;
    let c = crate::remix::selected_audio(s).ok_or("select a music clip on an audio track")?;
    check_clip(s, c)
}

fn suggest_with(s: &mut Session, p: &Value, pacing: Pacing) -> Result<Value> {
    let mut q = if p.is_object() { p.clone() } else { json!({}) };
    q["pacing"] = json!(pacing.id());
    suggest(s, &q)
}

pub fn commands() -> Vec<CommandSpec> {
    const MENU: &[&str] = &["Markers", "Suggest Cuts from Music"];
    const PARAMS: &str = r#"{"clip":id?,"density":0..100=50,"sections":bool=true,"hits":bool=true,"apply":"markers"|"none"="markers","replace":bool=true}"#;
    vec![
        CommandSpec {
            id: CMD,
            label: "Suggest Cuts from Music",
            menu: &[],
            shortcut: None,
            params: r#"{"clip":id?,"pacing":"calm"|"cinematic"|"energetic"|"trailer"="cinematic","density":0..100=50,"sections":bool=true,"hits":bool=true,"apply":"markers"|"none"="markers","replace":bool=true}"#,
            enabled: can_suggest,
            run: suggest,
            journal: true,
        },
        CommandSpec {
            id: "audio.suggestCuts.calm",
            label: "Calm",
            menu: MENU,
            shortcut: None,
            params: PARAMS,
            enabled: can_suggest_selected,
            run: |s, p| suggest_with(s, p, Pacing::Calm),
            journal: true,
        },
        CommandSpec {
            id: "audio.suggestCuts.cinematic",
            label: "Cinematic",
            menu: MENU,
            shortcut: None,
            params: PARAMS,
            enabled: can_suggest_selected,
            run: |s, p| suggest_with(s, p, Pacing::Cinematic),
            journal: true,
        },
        CommandSpec {
            id: "audio.suggestCuts.energetic",
            label: "Energetic",
            menu: MENU,
            shortcut: None,
            params: PARAMS,
            enabled: can_suggest_selected,
            run: |s, p| suggest_with(s, p, Pacing::Energetic),
            journal: true,
        },
        CommandSpec {
            id: "audio.suggestCuts.trailer",
            label: "Trailer",
            menu: MENU,
            shortcut: None,
            params: PARAMS,
            enabled: can_suggest_selected,
            run: |s, p| suggest_with(s, p, Pacing::Trailer),
            journal: true,
        },
    ]
}
