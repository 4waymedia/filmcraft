//! Tests of [`crate::music_cuts`]: Suggest Cuts on generated music (120 BPM, 24 bars, 48 s).

use std::sync::Arc;

use filmcraft_audio_dsp::remix as dsp;
use filmcraft_project::{ClipId, Label, Marker, MarkerKind};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use super::*;
use crate::music_cuts::TAG;

const SR: u32 = 48_000;

/// A session with generated music on A1 at `at` seconds, the clip selected.
fn session(at: f64) -> (Session, ClipId) {
    let (m, _) = dsp::test_music(SR, 120.0, 24, 3);
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"name": "cuts", "audio": 2, "video": 1})).unwrap();
    let inter: Vec<f32> = (0..m[0].len()).flat_map(|i| [m[0][i], m[1][i]]).collect();
    let bytes: Arc<[u8]> = crate::previews::write_wav_f32(&inter, SR).into();
    let item = crate::commands::import_bytes(&mut s, "/music.wav", bytes, None).unwrap();
    let r = s.execute("timeline.place", json!({"item": item.0, "audioTrack": "A1", "seconds": at})).unwrap();
    let c = ClipId(r["clips"][0].as_u64().unwrap());
    s.execute("timeline.select", json!({"clips": [c.0]})).unwrap();
    (s, c)
}

fn markers(s: &Session) -> Vec<Marker> {
    s.active_sequence().unwrap().markers.clone()
}

fn ours(s: &Session) -> Vec<Marker> {
    markers(s).into_iter().filter(|m| m.comment.starts_with(TAG)).collect()
}

#[test]
fn markers_for_cuts_and_sections_with_undo() {
    let (mut s, c) = session(0.0);
    let r = s.execute("audio.suggestCuts", json!({})).unwrap();
    assert_eq!(r["clip"], c.0);
    assert!((r["bpm"].as_f64().unwrap() - 120.0).abs() < 1.0, "{}", r["bpm"]);
    assert_eq!(r["meter"], 4);
    let cuts = r["cuts"].as_array().unwrap();
    assert!(cuts.len() >= 8, "{} cuts", cuts.len());
    let sections = r["sections"].as_array().unwrap();
    assert!(sections.len() >= 3, "{} sections", sections.len());
    // one unnamed green Comment marker per cut, at the reported times
    let m = ours(&s);
    let cut_markers: Vec<&Marker> = m.iter().filter(|m| m.name.is_empty()).collect();
    assert_eq!(cut_markers.len(), cuts.len());
    for (mk, cut) in cut_markers.iter().zip(cuts) {
        assert_eq!(mk.start.0, cut["time"].as_i64().unwrap());
        assert_eq!((mk.kind, mk.color, mk.duration), (MarkerKind::Comment, Label::Green, Tick::ZERO));
    }
    // cuts land on beats: a multiple of half a second (120 BPM) within 20 ms
    for mk in &cut_markers {
        let sec = mk.start.0 as f64 / Tick::from_seconds_f64(1.0).0 as f64;
        let off = (sec * 2.0 - (sec * 2.0).round()).abs() / 2.0;
        assert!(off < 0.02, "cut at {sec:.3} s is {off:.3} s off the beat");
    }
    // range markers per section, tiling the clip
    let sec_markers: Vec<&Marker> = m.iter().filter(|m| m.name.starts_with("Section ")).collect();
    assert_eq!(sec_markers.len(), sections.len());
    assert_eq!(sec_markers[0].start, Tick::ZERO);
    assert!(sec_markers.iter().all(|m| m.color == Label::Blue && m.duration > Tick::ZERO));
    assert!(sec_markers.windows(2).all(|w| w[0].start + w[0].duration == w[1].start));
    assert_eq!(r["markersAdded"].as_u64().unwrap() as usize, m.len());
    // one undo step
    s.execute("edit.undo", json!({})).unwrap();
    assert!(ours(&s).is_empty());
}

#[test]
fn automate_to_sequence_sees_the_cut_markers() {
    let (mut s, _) = session(0.0);
    s.execute("audio.suggestCuts", json!({})).unwrap();
    // the filter Automate to Sequence uses for `placement: "unnumberedMarkers"`
    let unnumbered = markers(&s).iter().filter(|m| m.name.is_empty() && m.kind == MarkerKind::Comment).count();
    let cuts = ours(&s).iter().filter(|m| m.name.is_empty()).count();
    assert_eq!(unnumbered, cuts);
}

#[test]
fn dry_run_replace_and_other_markers() {
    let (mut s, c) = session(10.0);
    let undo_before = s.history.undo.len();
    let r = s.execute("audio.suggestCuts", json!({"clip": c.0, "apply": "none"})).unwrap();
    assert_eq!(r["applied"], false);
    assert!(markers(&s).is_empty() && s.history.undo.len() == undo_before, "a dry run changes nothing");
    // the clip starts at 10 s (snapped to a frame): everything is offset
    let ten = s.active_sequence().unwrap().find_item(c).unwrap().1.start.0;
    assert!(ten >= Tick::from_seconds_f64(10.0).0);
    assert!(r["cuts"].as_array().unwrap().iter().all(|c| c["time"].as_i64().unwrap() > ten));
    assert_eq!(r["sections"][0]["start"].as_i64().unwrap(), ten);
    // a user marker, then suggest twice: no pile-up, the user marker stays
    s.execute("markers.add", json!({"seconds": 20.0, "name": "mine"})).unwrap();
    let a = s.execute("audio.suggestCuts", json!({})).unwrap();
    let n = ours(&s).len();
    let b = s.execute("audio.suggestCuts", json!({})).unwrap();
    assert_eq!(ours(&s).len(), n);
    assert_eq!(b["markersRemoved"].as_u64().unwrap() as usize, n);
    assert!(markers(&s).iter().any(|m| m.name == "mine"));
    assert_eq!(a["cuts"], b["cuts"], "deterministic");
    // replace: false adds a second set
    s.execute("audio.suggestCuts", json!({"replace": false})).unwrap();
    assert_eq!(ours(&s).len(), 2 * n);
}

#[test]
fn pacing_density_and_switches() {
    let (mut s, _) = session(0.0);
    let count = |s: &mut Session, p: Value| s.execute("audio.suggestCuts", p).unwrap()["cuts"].as_array().unwrap().len();
    let calm = count(&mut s, json!({"pacing": "calm", "apply": "none"}));
    let energetic = count(&mut s, json!({"pacing": "energetic", "apply": "none"}));
    assert!(calm < energetic, "calm {calm} < energetic {energetic}");
    let sparse = count(&mut s, json!({"density": 0, "apply": "none"}));
    let dense = count(&mut s, json!({"density": 100, "apply": "none"}));
    assert!(sparse < dense, "density 0 → {sparse}, 100 → {dense}");
    s.execute("audio.suggestCuts", json!({"sections": false})).unwrap();
    assert!(ours(&s).iter().all(|m| !m.name.starts_with("Section ")));
}

#[test]
fn hostile_parameters_are_refused() {
    let (mut s, c) = session(0.0);
    for p in [
        json!({"pacing": "bogus"}),
        json!({"pacing": 3}),
        json!({"density": "x"}),
        json!({"density": -1}),
        json!({"density": 101}),
        json!({"density": 1e308}),
        json!({"apply": "explode"}),
        json!({"apply": 1}),
        json!({"sections": "yes"}),
        json!({"replace": 0}),
        json!({"clip": "one"}),
        json!({"clip": -5}),
        json!({"clip": 999_999_999u64}),
        json!({"clip": u64::MAX}),
    ] {
        assert!(s.execute("audio.suggestCuts", p.clone()).is_err(), "accepted {p}");
    }
    assert!(ours(&s).is_empty());
    // nothing selected and no clip given
    s.execute("edit.deselectAll", json!({})).unwrap();
    let e = s.execute("audio.suggestCuts", json!({})).unwrap_err().to_string();
    assert!(e.contains("audio clip"), "{e}");
    // a rate-changed clip
    s.execute("timeline.select", json!({"clips": [c.0]})).unwrap();
    s.execute("clip.speedDuration", json!({"clips": [c.0], "speed": 50.0})).unwrap();
    let e = s.execute("audio.suggestCuts", json!({"clip": c.0})).unwrap_err().to_string();
    assert!(e.contains("100 %"), "{e}");
    // no sequence
    let mut empty = Session::default();
    assert!(empty.execute("audio.suggestCuts", json!({})).is_err());
}

#[test]
fn silence_has_no_beat() {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"name": "q", "audio": 1, "video": 1})).unwrap();
    let z = vec![0f32; SR as usize * 20 * 2]; // stereo, interleaved
    let bytes: Arc<[u8]> = crate::previews::write_wav_f32(&z, SR).into();
    let item = crate::commands::import_bytes(&mut s, "/quiet.wav", bytes, None).unwrap();
    let r = s.execute("timeline.place", json!({"item": item.0, "audioTrack": "A1", "seconds": 0.0})).unwrap();
    let c = r["clips"][0].as_u64().unwrap();
    let e = s.execute("audio.suggestCuts", json!({"clip": c})).unwrap_err().to_string();
    assert!(e.contains("no beat") || e.contains("cannot suggest"), "{e}");
}
