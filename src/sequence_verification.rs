use super::*;
use crate::timeline::{Project, Source};
use std::time::{Duration, Instant};

fn edited_project() -> Project {
    let path = media::test_fixture();
    let source = Arc::new(Source {
        metadata: probe_video(&path).unwrap(),
        path,
        has_audio: true,
    });
    let mut p = Project::default();
    let first = p.add_video(source.clone());
    p.trim(first, false, 1., 2.).unwrap();
    let second = p.add_video(source);
    p.trim(second, false, 0., 1.).unwrap();
    let sound = p.detach(first).unwrap();
    p.trim(sound, true, 0.2, 0.8).unwrap();
    p.move_audio(sound, 0.2).unwrap();
    p.gain(sound, 0.5, false).unwrap();
    p.gain(second, 1., true).unwrap();
    p
}
fn export(project: Project, output: PathBuf, profile: ExportProfile) -> Result<PathBuf, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    media::export::execute_project_export(
        project,
        output,
        profile,
        |_| {},
        move |r| {
            let _ = tx.send(r);
        },
    );
    rx.recv_timeout(Duration::from_secs(60)).unwrap()
}
fn output(mut cmd: std::process::Command) -> std::process::Output {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn frame_hashes(path: &std::path::Path) -> Vec<String> {
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-v", "error", "-i"])
        .arg(path)
        .args(["-an", "-f", "framemd5", "-"]);
    String::from_utf8(output(cmd).stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect()
}
#[test]
#[ignore = "requires FFmpeg and bundled playback runtime"]
fn timeline_export_preserves_order_audio_placement_gain_mute_and_size_cap() {
    let project = edited_project();
    let path = PathBuf::from("target/media-verification/timeline-lossless.mkv");
    export(project.clone(), path.clone(), ExportProfile::LosslessEncode).unwrap();
    let metadata = probe_video(&path).unwrap();
    assert!((metadata.duration_secs - 2.).abs() < 0.05);
    let original = frame_hashes(&project.video[0].source.path);
    let rendered = frame_hashes(&path);
    let expected: Vec<_> = original[30..60]
        .iter()
        .chain(original[0..30].iter())
        .cloned()
        .collect();
    assert_eq!(
        rendered, expected,
        "Reordered clips must preserve decoded frames in lossless export"
    );
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-vn", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"]);
    let samples: Vec<f32> = output(cmd)
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let rms = |start: f64, end: f64| {
        let slice = &samples[(start * 48000.) as usize..(end * 48000.) as usize];
        (slice.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / slice.len() as f64).sqrt()
    };
    assert!(
        rms(0.01, 0.15) < 0.00001,
        "Detached audio must start at its edited position"
    );
    let level = rms(0.3, 0.6);
    assert!(
        level > 0.015 && level < 0.05,
        "Clip gain must reach export, got {level}"
    );
    assert!(rms(1.2, 1.6) < 0.00001, "Muted audio must remain silent");
    let capped = PathBuf::from("target/media-verification/timeline-capped.mp4");
    export(
        project.clone(),
        capped.clone(),
        ExportProfile::TargetSize {
            target_mb: 0.3,
            codec: ExportCodec::Libx264,
            fps: 30.,
            preset: "fast".into(),
        },
    )
    .unwrap();
    assert!(std::fs::metadata(capped).unwrap().len() <= (0.3 * 1024. * 1024.) as u64);
    let source = project.video[0].source.path.clone();
    let before = std::fs::read(&source).unwrap();
    assert!(export(project, source.clone(), ExportProfile::LosslessEncode).is_err());
    assert_eq!(std::fs::read(source).unwrap(), before);
}
#[test]
#[ignore = "requires FFmpeg and bundled playback runtime"]
fn timeline_playback_seeks_across_cuts_and_reloads_without_recreating_controller() {
    let project = edited_project();
    let plan = project.preview().unwrap();
    let (frames, rx) = std::sync::mpsc::channel();
    let (errors, erx) = std::sync::mpsc::channel();
    let player = PlayerController::timeline(
        plan,
        move |_, time, _| {
            let _ = frames.send(time);
        },
        |_, _| {},
        move |e| {
            let _ = errors.send(e);
        },
    );
    rx.recv_timeout(Duration::from_secs(10)).unwrap();
    player.seek(1.5);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    while Instant::now() < deadline {
        if let Ok(time) = rx.recv_timeout(Duration::from_millis(100))
            && (time - 1.5).abs() < 0.1
        {
            found = true;
            break;
        }
    }
    assert!(found, "Seeking across an edit must return a frame");
    let mut reordered = project.clone();
    let last = reordered.video[1].id;
    reordered.reorder(last, 0).unwrap();
    // Keep detached audio inside the free gap after reordering linked video.
    reordered.audio.iter_mut().for_each(|a| {
        if a.linked_video.is_some() {
            a.muted = true;
        }
    });
    player.reload(reordered.preview().unwrap(), 0.5);
    let deadline = Instant::now() + Duration::from_secs(10);
    found = false;
    while Instant::now() < deadline {
        if let Ok(time) = rx.recv_timeout(Duration::from_millis(100))
            && (time - 0.5).abs() < 0.1
        {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "Reloading edits must seek to the requested timeline position"
    );
    player.play();
    std::thread::sleep(Duration::from_millis(250));
    player.pause();
    player.stop();
    let errors: Vec<_> = erx.try_iter().collect();
    assert!(errors.is_empty(), "Playback errors: {errors:?}");
}
#[test]
fn model_edit_cost_is_bounded_and_sources_are_shared() {
    let source = Arc::new(Source {
        path: "fixture.mp4".into(),
        metadata: VideoMetadata {
            duration_secs: 10.,
            width: 640,
            height: 360,
            fps: 30.,
            keyframes: vec![],
        },
        has_audio: false,
    });
    let mut p = Project::default();
    for _ in 0..100 {
        p.add_video(source.clone());
    }
    let start = Instant::now();
    for i in 0..1000 {
        let id = p.video[i % 100].id;
        p.reorder(id, (i * 7) % 100).unwrap();
    }
    eprintln!(
        "1,000 reorder operations / 100 clips: {:?}; no decoding, file IO, or workers",
        start.elapsed()
    );
    assert!(Arc::ptr_eq(&p.video[0].source, &source));
    assert_eq!(p.duration(), 1000.);
}

#[test]
fn free_positions_layers_trim_and_simple_mode_preserve_edit_intent() {
    let source = Arc::new(Source {
        path: "fixture.mp4".into(),
        metadata: VideoMetadata {
            duration_secs: 10.,
            width: 640,
            height: 360,
            fps: 30.,
            keyframes: vec![],
        },
        has_audio: true,
    });
    let mut p = Project::default();
    let a = p.add_video(source.clone());
    p.trim(a, false, 2., 6.).unwrap();
    p.move_video(a, 1., 0).unwrap();
    let b = p.add_video(source);
    p.trim(b, false, 0., 2.).unwrap();
    p.move_video(b, 2., 1).unwrap();
    let visible: Vec<_> = p
        .visible_segments()
        .iter()
        .map(|(s, e, c)| (*s, *e, c.map(|c| c.id)))
        .collect();
    assert_eq!(
        visible,
        vec![
            (0., 1., None),
            (1., 2., Some(a)),
            (2., 4., Some(b)),
            (4., 5., Some(a))
        ]
    );
    assert!(p.preview().unwrap().needs_black);
    assert_eq!(
        p.preview().unwrap().extra_audio.len(),
        1,
        "Overlapping sound needs independent clocks"
    );
    p.trim_edge(a, false, false, 0.5).unwrap();
    assert_eq!(p.video[0].start, 1.5);
    assert_eq!(p.video[0].input, 2.5);
    assert_eq!(
        p.audio
            .iter()
            .find(|c| c.linked_video == Some(a))
            .unwrap()
            .start,
        1.5
    );
    let simple = p.selected_project(a, false);
    assert!(simple.simple_source().is_some());
    assert_eq!(simple.video[0].start, 0.);
    assert_eq!(simple.video[0].input, 2.5);
    let detached = p.detach(a).unwrap();
    p.move_audio_layer(detached, 7., 0).unwrap();
    assert_eq!(p.duration(), 10.5, "Detached sound can extend past video");
    let before = p.video[0].start;
    p.remove(b, false);
    assert_eq!(
        p.video[0].start, before,
        "Removing clips must preserve gaps"
    );
}

#[test]
#[ignore = "requires FFmpeg"]
fn waveform_analysis_is_real_bounded_and_cancellable() {
    let path = media::test_fixture();
    let cancel = Arc::new(AtomicBool::new(false));
    let peaks = media::waveform::peaks(&path, 3., &cancel).unwrap();
    assert!(!peaks.is_empty() && peaks.len() <= 2048);
    assert!(peaks.iter().all(|p| p.is_finite() && *p >= 0. && *p <= 1.));
    assert!(peaks.iter().any(|p| *p > 0.5));
    assert_eq!(media::waveform::range(&peaks, 3., 1., 2.).len(), 96);
    cancel.store(true, Ordering::Relaxed);
    assert!(media::waveform::peaks(&path, 3., &cancel).is_err());
}

#[test]
#[ignore = "requires FFmpeg and bundled playback runtime"]
fn layered_export_has_black_gaps_top_video_crop_and_mixed_audio() {
    let path = media::test_fixture();
    let source = Arc::new(Source {
        metadata: probe_video(&path).unwrap(),
        path,
        has_audio: true,
    });
    let mut p = Project::default();
    let a = p.add_video(source.clone());
    p.trim(a, false, 0., 2.).unwrap();
    p.move_video(a, 0.5, 0).unwrap();
    p.crop(
        a,
        crate::timeline::Crop {
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5,
        },
    )
    .unwrap();
    let b = p.add_video(source);
    p.trim(b, false, 1., 2.).unwrap();
    p.move_video(b, 1., 1).unwrap();
    p.crop(
        b,
        crate::timeline::Crop {
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5,
        },
    )
    .unwrap();
    let dest = PathBuf::from("target/media-verification/layers-crop-gaps.mkv");
    export(p.clone(), dest.clone(), ExportProfile::LosslessEncode).unwrap();
    let metadata = probe_video(&dest).unwrap();
    assert_eq!((metadata.width, metadata.height), (320, 180));
    assert!((metadata.duration_secs - 2.5).abs() < 0.06);
    let mut black = std::process::Command::new("ffmpeg");
    black
        .args(["-v", "error", "-ss", "0.2", "-i"])
        .arg(&dest)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
    assert!(
        output(black).stdout.iter().all(|v| *v < 3),
        "Leading empty space must export black"
    );
    let mut expected = std::process::Command::new("ffmpeg");
    expected
        .args(["-v", "error", "-ss", "1.2", "-i"])
        .arg(&p.video[1].source.path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "crop=320:180:160:90",
            "-an",
            "-f",
            "framemd5",
            "-",
        ]);
    let expected = String::from_utf8(output(expected).stdout).unwrap();
    let hash = expected
        .lines()
        .find(|l| !l.starts_with('#'))
        .unwrap()
        .rsplit(',')
        .next()
        .unwrap()
        .trim();
    assert_eq!(
        frame_hashes(&dest)[36],
        hash,
        "Upper layer pixels and crop must match source"
    );
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-v", "error", "-i"])
        .arg(&dest)
        .args(["-vn", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"]);
    let samples: Vec<f32> = output(cmd)
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let rms = |s: f64, e: f64| {
        let a = &samples[(s * 48000.) as usize..(e * 48000.) as usize];
        (a.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / a.len() as f64).sqrt()
    };
    assert!(rms(0.05, 0.3) < 0.00001);
    assert!(
        rms(1.2, 1.5) > rms(0.6, 0.9) * 1.5,
        "Both audio layers must mix"
    );
    let plan = p.preview().unwrap();
    assert_eq!(plan.extra_audio.len(), 1);
    let (tx, rx) = std::sync::mpsc::channel();
    let (etx, erx) = std::sync::mpsc::channel();
    let player = PlayerController::timeline(
        plan,
        move |frame, t, _| {
            let _ = tx.send((frame.width(), frame.height(), t));
        },
        |_, _| {},
        move |e| {
            let _ = etx.send(e);
        },
    );
    rx.recv_timeout(Duration::from_secs(10)).unwrap();
    player.seek(1.2);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    while Instant::now() < deadline {
        if let Ok((w, h, t)) = rx.recv_timeout(Duration::from_millis(100))
            && (t - 1.2).abs() < 0.1
        {
            assert_eq!((w, h), (320, 180));
            found = true;
            break;
        }
    }
    assert!(found);
    player.play();
    std::thread::sleep(Duration::from_millis(150));
    player.stop();
    assert!(erx.try_iter().collect::<Vec<_>>().is_empty());
}
