//! Tests of [`crate::trailer`]: Build Trailer from generated music (120 BPM, 24 bars, 48 s) and
//! colour mattes.

use std::sync::Arc;

use filmcraft_audio_dsp::remix as dsp;
use filmcraft_project::{ItemId, Label};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use super::*;

/// A project with music and `n` colour mattes of `secs` seconds; everything selected.
fn project(n: usize, secs: f64) -> (Session, ItemId, Vec<ItemId>) {
    let (m, _) = dsp::test_music(48_000, 120.0, 24, 3);
    let mut s = Session::default();
    let inter: Vec<f32> = (0..m[0].len()).flat_map(|i| [m[0][i], m[1][i]]).collect();
    let bytes: Arc<[u8]> = crate::previews::write_wav_f32(&inter, 48_000).into();
    let music = crate::commands::import_bytes(&mut s, "/song.wav", bytes, None).unwrap();
    let mut shots = Vec::new();
    for k in 0..n {
        let r = s.execute("file.newColorMatte", json!({"color": format!("#{:02x}4080", 40 * k), "seconds": secs})).unwrap();
        shots.push(ItemId(r["item"].as_u64().unwrap()));
    }
    let mut sel = vec![music];
    sel.extend(&shots);
    s.state.project_selection = sel;
    (s, music, shots)
}

fn shots(r: &Value) -> &Vec<Value> {
    r["shots"].as_array().unwrap()
}

#[test]
fn builds_a_sequence_cut_to_the_music_in_one_undo_step() {
    let (mut s, music, mattes) = project(4, 10.0);
    let undo = s.history.undo.len();
    let r = s.execute("sequence.buildTrailer", json!({})).unwrap();
    assert!(r["sequence"].as_u64().is_some());
    assert!((r["bpm"].as_f64().unwrap() - 120.0).abs() < 1.0);
    let seq_item = ItemId(r["sequence"].as_u64().unwrap());
    let seq_name = s.project.item(seq_item).unwrap().name.clone();
    assert_eq!(seq_name, "Trailer - song.wav");
    let q = s.active_sequence().unwrap();
    // music on A1, shots on V1, back to back from 0 to the music's end
    let a1 = &q.audio_tracks[0].items;
    assert_eq!(a1.len(), 1);
    assert_eq!(a1[0].item, music);
    let v1 = &q.video_tracks[0].items;
    assert_eq!(v1.len(), shots(&r).len());
    assert!(v1.len() >= 8, "{} shots", v1.len());
    assert_eq!(v1[0].start, Tick::ZERO);
    for w in v1.windows(2) {
        assert_eq!(w[0].start + w[0].duration, w[1].start, "shots butt");
    }
    assert_eq!(v1.last().map(|c| c.start + c.duration), Some(a1[0].duration), "picture ends with the music");
    // every shot starts on a frame, every matte is used in turn
    let fd = q.settings.frame_rate;
    assert!(v1.iter().all(|c| fd.snap(c.start) == c.start));
    for (k, c) in v1.iter().enumerate() {
        assert_eq!(c.item, mattes[k % mattes.len()]);
    }
    // sections as blue range markers
    assert!(q.markers.iter().any(|m| m.color == Label::Blue && m.duration > Tick::ZERO));
    // one undo step takes the whole build back
    assert_eq!(s.history.undo.len(), undo + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.item(seq_item).is_none(), "undo removes the trailer sequence");
}

#[test]
fn a_target_length_ends_on_a_musical_boundary() {
    let (mut s, _, _) = project(3, 10.0);
    let r = s.execute("sequence.buildTrailer", json!({"seconds": 30, "dryRun": true})).unwrap();
    let sec = r["seconds"].as_f64().unwrap();
    // 120 BPM in 4/4: bars every 2 s, so the end is within a bar of 30 s, on a bar line
    assert!((sec - 30.0).abs() <= 2.05, "{sec}");
    assert!(((sec / 2.0) - (sec / 2.0).round()).abs() < 0.05, "{sec} s is on a bar line");
    // the dry run changed nothing
    assert!(s.active_sequence().is_none());
}

#[test]
fn reused_clips_show_different_parts_and_short_clips_are_reported() {
    let (mut s, _, _) = project(1, 20.0);
    let r = s.execute("sequence.buildTrailer", json!({"dryRun": true})).unwrap();
    let ins: Vec<i64> = shots(&r).iter().map(|x| x["sourceIn"].as_i64().unwrap()).collect();
    assert!(ins.windows(2).any(|w| w[0] != w[1]), "one clip reused from different spots: {ins:?}");
    let (mut s, _, _) = project(2, 0.5);
    let r = s.execute("sequence.buildTrailer", json!({"pacing": "calm", "dryRun": true})).unwrap();
    assert!(r["short"].as_u64().unwrap() > 0, "half-second mattes are shorter than calm shots");
}

#[test]
fn menu_items_follow_the_selection() {
    let (mut s, _, _) = project(3, 10.0);
    for id in ["sequence.buildTrailer.30", "sequence.buildTrailer.60", "sequence.buildTrailer.90", "sequence.buildTrailer.full"] {
        let spec = crate::commands::find(id).unwrap();
        assert_eq!(spec.menu, &["Sequence", "Build Trailer from Music"]);
        assert!((spec.enabled)(&s).is_ok(), "{id}");
    }
    let r = s.execute("sequence.buildTrailer.30", json!({})).unwrap();
    assert!((r["seconds"].as_f64().unwrap() - 30.0).abs() <= 2.05);
    // only music selected: disabled with a reason
    let music = s.state.project_selection[0];
    s.state.project_selection = vec![music];
    let why = (crate::commands::find("sequence.buildTrailer.60").unwrap().enabled)(&s).unwrap_err();
    assert!(why.contains("video clips"), "{why}");
}

#[test]
fn hostile_parameters_are_refused() {
    let (mut s, music, mattes) = project(2, 10.0);
    for p in [
        json!({"music": "x"}),
        json!({"music": 999_999}),
        json!({"music": mattes[0].0}),
        json!({"items": "all"}),
        json!({"items": [999_999]}),
        json!({"items": []}),
        json!({"items": [music.0]}),
        json!({"seconds": 1}),
        json!({"seconds": 1e9}),
        json!({"seconds": "60"}),
        json!({"pacing": "wild"}),
        json!({"density": 150}),
        json!({"name": ""}),
        json!({"name": 5}),
        json!({"dryRun": "yes"}),
    ] {
        assert!(s.execute("sequence.buildTrailer", p.clone()).is_err(), "accepted {p}");
    }
    assert!(s.active_sequence().is_none(), "nothing was built");
    // nothing selected
    s.state.project_selection.clear();
    assert!(s.execute("sequence.buildTrailer", json!({})).is_err());
}
