use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum ExportCodec {
    HevcNvenc10Bit,
    Libx265_10Bit,
    H264Nvenc,
    Libx264,
}
#[derive(Debug, Clone)]
pub enum ExportProfile {
    LosslessCopy,
    LosslessEncode,
    TargetSize {
        target_mb: f64,
        codec: ExportCodec,
        fps: f64,
        preset: String,
    },
}
#[derive(Debug, Clone)]
pub struct ExportJob {
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub in_point_secs: f64,
    pub out_point_secs: f64,
    pub profile: ExportProfile,
}
pub fn calculate_target_bitrate_kbps(duration: f64, target_mb: f64) -> u64 {
    if !duration.is_finite() || duration <= 0.0 || !target_mb.is_finite() || target_mb <= 0.0 {
        return 0;
    }
    let available = target_mb * 8.0 * 1024.0 * 1024.0 * 0.96 / duration - 128_000.0;
    (available.max(0.0) / 1000.0).floor() as u64
}
fn validate_job(job: &ExportJob) -> Result<(), String> {
    if !job.in_point_secs.is_finite()
        || !job.out_point_secs.is_finite()
        || job.in_point_secs < 0.0
        || job.out_point_secs <= job.in_point_secs
    {
        return Err("Select a valid, non-empty trim range.".into());
    }
    let source = std::fs::canonicalize(&job.input_path).map_err(|e| e.to_string())?;
    if std::fs::canonicalize(&job.output_path).is_ok_and(|p| p == source) {
        return Err(
            "Choose a different destination; the source video cannot be overwritten.".into(),
        );
    }
    match &job.profile {
        ExportProfile::TargetSize { target_mb, fps, .. } => {
            if !fps.is_finite() || *fps <= 0.0 {
                return Err("The source frame rate is invalid.".into());
            }
            if calculate_target_bitrate_kbps(job.out_point_secs - job.in_point_secs, *target_mb)
                < 150
            {
                return Err("This size is too small for the selected duration. Increase the size or shorten the cut.".into());
            }
        }
        ExportProfile::LosslessEncode => {
            if !job
                .output_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
            {
                return Err("Exact lossless exports require an MKV destination.".into());
            }
        }
        _ => {}
    }
    Ok(())
}
fn command(job: &ExportJob, temporary: &PathBuf, bitrate: u64) -> Command {
    let duration = job.out_point_secs - job.in_point_secs;
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-nostats",
            "-progress",
            "pipe:1",
            "-n",
            "-ss",
        ])
        .arg(format!("{:.6}", job.in_point_secs))
        .arg("-i")
        .arg(&job.input_path)
        .arg("-t")
        .arg(format!("{duration:.6}"))
        .args(["-map", "0:V:0", "-map", "0:a:0?", "-sn", "-dn"]);
    encode_options(&mut command, job, bitrate, false);
    finish_command(command, temporary)
}
fn encode_options(command: &mut Command, job: &ExportJob, bitrate: u64, composed: bool) {
    match &job.profile {
        ExportProfile::LosslessCopy => {
            command.args(["-c", "copy", "-avoid_negative_ts", "disabled"]);
        }
        ExportProfile::LosslessEncode => {
            // Preserve the decoded pixel format; no resize, pad, or forced FPS.
            command.args([
                "-c:v",
                "ffv1",
                "-level",
                "3",
                "-coder",
                "1",
                "-context",
                "1",
                "-slicecrc",
                "1",
                "-fps_mode",
                "passthrough",
                "-c:a",
                "pcm_f64le",
            ]);
        }
        ExportProfile::TargetSize {
            codec, fps, preset, ..
        } => {
            let (encoder, format) = match codec {
                ExportCodec::HevcNvenc10Bit => ("hevc_nvenc", "p010le"),
                ExportCodec::Libx265_10Bit => ("libx265", "yuv420p10le"),
                ExportCodec::H264Nvenc => ("h264_nvenc", "yuv420p"),
                ExportCodec::Libx264 => ("libx264", "yuv420p"),
            };
            if !composed {
                command.args(["-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2"]);
            }
            command
                .args(["-c:v", encoder, "-pix_fmt", format, "-b:v"])
                .arg(format!("{bitrate}k"))
                .arg("-maxrate")
                .arg(format!("{}k", bitrate * 5 / 4))
                .arg("-bufsize")
                .arg(format!("{}k", bitrate * 2))
                .args(["-preset", preset, "-fps_mode", "cfr", "-r"])
                .arg(format!("{fps:.6}"))
                .args(["-c:a", "aac", "-b:a", "128k"]);
            if job
                .output_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
            {
                command.args(["-movflags", "+faststart"]);
            }
        }
    }
}
fn finish_command(mut command: Command, temporary: &PathBuf) -> Command {
    command
        .arg(temporary)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}
fn run_process<F>(
    mut command: Command,
    duration: f64,
    attempt: usize,
    cancel: &Arc<AtomicBool>,
    on_status: &Arc<F>,
) -> Result<(), String>
where
    F: Fn(String) + Send + Sync + 'static,
{
    if cancel.load(Ordering::Relaxed) {
        return Err("Export cancelled.".into());
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Could not start FFmpeg: {e}"))?;
    let stderr = child.stderr.take().unwrap();
    let errors = thread::spawn(move || {
        let mut tail = VecDeque::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tail.len() == 16 {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail.into_iter().collect::<Vec<_>>().join("\n")
    });
    let stdout = child.stdout.take().unwrap();
    let callback = on_status.clone();
    let progress = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(value) = line.strip_prefix("out_time_us=")
                && let Ok(micros) = value.parse::<f64>()
            {
                callback(format!(
                    "Exporting · {:.0}%{}",
                    (micros / 1_000_000.0 / duration * 100.0).clamp(0.0, 99.0),
                    if attempt > 0 {
                        format!(" · size adjustment {attempt}")
                    } else {
                        String::new()
                    }
                ));
            }
        }
    });
    let result = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            break Err("Export cancelled.".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    Ok(())
                } else {
                    Err("FFmpeg could not export this cut.".into())
                };
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error.to_string());
            }
        }
    };
    let _ = progress.join();
    let details = errors.join().unwrap_or_default();
    result.map_err(|error: String| {
        if details.is_empty() || cancel.load(Ordering::Relaxed) {
            error
        } else {
            format!("{error}\n{details}")
        }
    })
}
pub fn execute_export<F, C>(job: ExportJob, on_status: F, on_complete: C) -> Arc<AtomicBool>
where
    F: Fn(String) + Send + Sync + 'static,
    C: Fn(Result<PathBuf, String>) + Send + Sync + 'static,
{
    execute_impl(job, None, on_status, on_complete)
}
pub fn execute_project_export<F, C>(
    project: crate::timeline::Project,
    output_path: PathBuf,
    profile: ExportProfile,
    on_status: F,
    on_complete: C,
) -> Arc<AtomicBool>
where
    F: Fn(String) + Send + Sync + 'static,
    C: Fn(Result<PathBuf, String>) + Send + Sync + 'static,
{
    let first = project
        .video
        .first()
        .map(|c| c.source.path.clone())
        .unwrap_or_default();
    let job = ExportJob {
        input_path: first,
        output_path,
        in_point_secs: 0.,
        out_point_secs: project.duration(),
        profile,
    };
    execute_impl(job, Some(project), on_status, on_complete)
}
fn project_command(
    project: &crate::timeline::Project,
    job: &ExportJob,
    temporary: &PathBuf,
    bitrate: u64,
) -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-nostats",
        "-progress",
        "pipe:1",
        "-n",
        "-filter_complex_threads",
        "2",
    ]);
    let mut filters = Vec::new();
    let first = &project.video[0].source.metadata;
    let fps = if let ExportProfile::TargetSize { fps, .. } = job.profile {
        fps
    } else {
        first.fps
    };
    let first_clip = &project.video[0];
    let (_, _, w, h) = first_clip.crop.pixels(first.width, first.height);
    let segments = project.visible_segments();
    for (i, (start, end, clip)) in segments.iter().enumerate() {
        let duration = end - start;
        if let Some(c) = clip {
            cmd.args(["-threads", "2", "-ss"])
                .arg(format!("{:.9}", c.input + start - c.start))
                .arg("-t")
                .arg(format!("{duration:.9}"))
                .arg("-i")
                .arg(&c.source.path);
            let (x, y, cw, ch) = c
                .crop
                .pixels(c.source.metadata.width, c.source.metadata.height);
            filters.push(format!("[{i}:v:0]crop={cw}:{ch}:{x}:{y},scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps:.9},setpts=PTS-STARTPTS[v{i}]"));
        } else {
            cmd.args(["-f", "lavfi", "-i"]).arg(format!(
                "color=c=black:s={w}x{h}:r={fps:.9}:d={duration:.9}"
            ));
            filters.push(format!("[{i}:v:0]setpts=PTS-STARTPTS[v{i}]"));
        }
    }
    let inputs = (0..segments.len())
        .map(|i| format!("[v{i}]"))
        .collect::<String>();
    filters.push(format!("{inputs}concat=n={}:v=1:a=0[vout]", segments.len()));
    let mut audio_labels = Vec::new();
    for a in project
        .audio
        .iter()
        .filter(|a| !a.muted && a.start < project.duration())
    {
        let input = segments.len() + audio_labels.len();
        let label = format!("a{}", audio_labels.len());
        let duration = a.clip.duration().min(project.duration() - a.start);
        cmd.args(["-threads", "2"])
            .arg("-ss")
            .arg(format!("{:.9}", a.clip.input))
            .arg("-t")
            .arg(format!("{duration:.9}"))
            .arg("-i")
            .arg(&a.clip.source.path);
        let delay = (a.start * 48000.).round() as u64;
        filters.push(format!("[{input}:a:0]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,asettb=1/48000,asetpts=N/SR/TB,volume={:.6},adelay={delay}S:all=1[{label}]",a.gain));
        audio_labels.push(format!("[{label}]"));
    }
    if !audio_labels.is_empty() {
        filters.push(format!(
            "{}amix=inputs={}:normalize=0:duration=longest,apad,asettb=1/48000,asetpts=N/SR/TB,atrim=duration={:.9}[aout]",
            audio_labels.join(""),
            audio_labels.len(),
            project.duration()
        ));
    }
    cmd.arg("-filter_complex")
        .arg(filters.join(";"))
        .args(["-map", "[vout]"]);
    if !audio_labels.is_empty() {
        cmd.args(["-map", "[aout]"]);
    } else {
        cmd.arg("-an");
    }
    cmd.arg("-t").arg(format!("{:.9}", project.duration()));
    encode_options(&mut cmd, job, bitrate, true);
    finish_command(cmd, temporary)
}
fn execute_impl<F, C>(
    job: ExportJob,
    project: Option<crate::timeline::Project>,
    on_status: F,
    on_complete: C,
) -> Arc<AtomicBool>
where
    F: Fn(String) + Send + Sync + 'static,
    C: Fn(Result<PathBuf, String>) + Send + Sync + 'static,
{
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = cancelled.clone();
    let status = Arc::new(on_status);
    thread::spawn(move || {
        if let Err(error) = validate_job(&job) {
            on_complete(Err(error));
            return;
        }
        if let Some(project) = &project {
            if matches!(job.profile, ExportProfile::LosslessCopy) {
                on_complete(Err("Fast copy requires one unchanged source clip. Choose Target size or Lossless for an edited timeline.".into()));
                return;
            }
            for source in project
                .video
                .iter()
                .map(|c| &c.source.path)
                .chain(project.audio.iter().map(|a| &a.clip.source.path))
            {
                if std::fs::canonicalize(&job.output_path)
                    .ok()
                    .is_some_and(|dest| std::fs::canonicalize(source).ok().as_ref() == Some(&dest))
                {
                    on_complete(Err("The source media cannot be overwritten.".into()));
                    return;
                }
            }
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let extension = job
            .output_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4");
        let temporary = job.output_path.with_file_name(format!(
            ".feathercut-{}-{serial}.{extension}",
            std::process::id()
        ));
        let duration = job.out_point_secs - job.in_point_secs;
        let target = if let ExportProfile::TargetSize { target_mb, .. } = &job.profile {
            Some((target_mb * 1024.0 * 1024.0).floor() as u64)
        } else {
            None
        };
        let mut bitrate = target
            .map(|bytes| calculate_target_bitrate_kbps(duration, bytes as f64 / 1024.0 / 1024.0))
            .unwrap_or(0);
        let result = (|| -> Result<(), String> {
            for attempt in 0..6 {
                status(if attempt == 0 {
                    "Starting export…".into()
                } else {
                    "Adjusting bitrate to meet the size limit…".into()
                });
                run_process(
                    if let Some(project) = &project {
                        project_command(project, &job, &temporary, bitrate)
                    } else {
                        command(&job, &temporary, bitrate)
                    },
                    duration,
                    attempt,
                    &cancel,
                    &status,
                )?;
                if cancel.load(Ordering::Relaxed) {
                    return Err("Export cancelled.".into());
                }
                let measured = std::fs::metadata(&temporary)
                    .map_err(|e| e.to_string())?
                    .len();
                if measured == 0 {
                    return Err("The encoder produced an empty file.".into());
                }
                if let Some(limit) = target
                    && measured > limit
                {
                    // Measure the complete muxed file, including audio and headers.
                    // Conservatively scale down and retry; never publish an oversized file.
                    if attempt == 5 {
                        return Err(
                            "Could not meet this size limit. Increase the size or shorten the cut."
                                .into(),
                        );
                    }
                    bitrate =
                        (bitrate as f64 * limit as f64 / measured as f64 * 0.93).floor() as u64;
                    if bitrate < 50 {
                        return Err(
                            "The size limit leaves too little room for video and audio.".into()
                        );
                    }
                    std::fs::remove_file(&temporary).map_err(|e| e.to_string())?;
                    continue;
                }
                status(format!(
                    "Verified export · {:.2} MiB",
                    measured as f64 / 1024.0 / 1024.0
                ));
                if cancel.load(Ordering::Relaxed) {
                    return Err("Export cancelled.".into());
                }
                return std::fs::rename(&temporary, &job.output_path)
                    .map_err(|e| format!("Could not save the export: {e}"));
            }
            unreachable!()
        })();
        match result {
            Ok(()) => on_complete(Ok(job.output_path)),
            Err(error) => {
                let _ = std::fs::remove_file(&temporary);
                on_complete(Err(error));
            }
        }
    });
    cancelled
}
#[cfg(test)]
mod tests {
    use super::*;
    fn run_job(job: ExportJob) -> Result<PathBuf, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        execute_export(
            job,
            |_| {},
            move |result| {
                let _ = tx.send(result);
            },
        );
        rx.recv_timeout(Duration::from_secs(60))
            .expect("export hung")
    }
    #[test]
    #[ignore = "requires ffmpeg and ffprobe"]
    fn measured_size_limit_includes_audio_and_container() {
        let path = crate::media::test_fixture();
        let output = path.with_file_name("capped.mp4");
        let target = 0.22;
        let job = ExportJob {
            input_path: path,
            output_path: output.clone(),
            in_point_secs: 0.0,
            out_point_secs: 3.0,
            profile: ExportProfile::TargetSize {
                target_mb: target,
                codec: ExportCodec::Libx264,
                fps: 30.0,
                preset: "fast".into(),
            },
        };
        run_job(job).unwrap();
        let bytes = std::fs::metadata(&output).unwrap().len();
        assert!(
            bytes <= (target * 1024.0 * 1024.0) as u64,
            "size cap exceeded: {bytes}"
        );
        assert!((crate::media::probe_video(&output).unwrap().duration_secs - 3.0).abs() < 0.06);
    }
    #[test]
    #[ignore = "requires ffmpeg and ffprobe"]
    fn exact_lossless_preserves_selected_decoded_frames_between_keyframes() {
        let path = crate::media::test_fixture();
        let output = path.with_file_name("exact-lossless.mkv");
        run_job(ExportJob {
            input_path: path.clone(),
            output_path: output.clone(),
            in_point_secs: 0.7,
            out_point_secs: 2.2,
            profile: ExportProfile::LosslessEncode,
        })
        .unwrap();
        let hashes = |file: &PathBuf, source: bool| {
            let mut cmd = Command::new("ffmpeg");
            cmd.args(["-v", "error"]);
            if source {
                cmd.args(["-ss", "0.7"]);
            }
            cmd.arg("-i").arg(file);
            if source {
                cmd.args(["-t", "1.5"]);
            }
            let result = cmd
                .args(["-map", "0:v:0", "-f", "framemd5", "-"])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            String::from_utf8_lossy(&result.stdout)
                .lines()
                .filter(|line| !line.starts_with('#'))
                .map(|line| line.rsplit(',').next().unwrap().trim().to_owned())
                .collect::<Vec<_>>()
        };
        let expected = hashes(&path, true);
        assert_eq!(expected.len(), 45);
        assert_eq!(
            hashes(&output, false),
            expected,
            "lossless video pixels differ"
        );
        assert!((crate::media::probe_video(&output).unwrap().duration_secs - 1.5).abs() < 0.06);
    }
    #[test]
    fn impossible_budget_is_not_silently_increased() {
        assert_eq!(calculate_target_bitrate_kbps(3600.0, 1.0), 0);
        assert_eq!(calculate_target_bitrate_kbps(f64::NAN, 10.0), 0);
        assert!(
            calculate_target_bitrate_kbps(0.1, 10.0) > calculate_target_bitrate_kbps(0.5, 10.0)
        );
        assert_eq!(calculate_target_bitrate_kbps(10.0, 10.0), 7925);
    }
    #[test]
    fn rejects_source_overwrite_and_invalid_ranges() {
        let mut job = ExportJob {
            input_path: PathBuf::from("Cargo.toml"),
            output_path: PathBuf::from("Cargo.toml"),
            in_point_secs: 0.0,
            out_point_secs: 1.0,
            profile: ExportProfile::LosslessCopy,
        };
        assert!(validate_job(&job).unwrap_err().contains("overwritten"));
        job.out_point_secs = 0.0;
        assert!(validate_job(&job).is_err());
    }

    #[test]
    #[ignore = "requires ffmpeg and ffprobe on PATH"]
    fn real_exports_preserve_duration_and_protect_existing_files() {
        let directory = PathBuf::from("target/export-verification");
        std::fs::create_dir_all(&directory).unwrap();
        let run = |job: ExportJob| {
            let (tx, rx) = std::sync::mpsc::channel();
            execute_export(
                job,
                |_| {},
                move |result| {
                    tx.send(result).unwrap();
                },
            );
            rx.recv_timeout(Duration::from_secs(30))
                .expect("export hung")
        };
        let encoded = directory.join("encoded.mp4");
        let job = ExportJob {
            input_path: crate::media::test_fixture(),
            output_path: encoded.clone(),
            in_point_secs: 2.0,
            out_point_secs: 8.0,
            profile: ExportProfile::TargetSize {
                target_mb: 1.0,
                codec: ExportCodec::Libx264,
                fps: 30.0,
                preset: "fast".into(),
            },
        };
        run(job.clone()).unwrap();
        let metadata = crate::media::probe_video(&encoded).unwrap();
        assert!((metadata.duration_secs - 6.0).abs() < 0.05);
        let original_bytes = std::fs::read(&encoded).unwrap();
        // A failing FFmpeg process must drain its diagnostics and preserve the destination.
        let mut failing = job.clone();
        failing.profile = ExportProfile::TargetSize {
            target_mb: 1.0,
            codec: ExportCodec::Libx264,
            fps: 30.0,
            preset: "not-a-preset".into(),
        };
        assert!(run(failing).unwrap_err().contains("FFmpeg"));
        assert_eq!(std::fs::read(&encoded).unwrap(), original_bytes);
        let mut copied = job.clone();
        copied.output_path = directory.join("copied.mkv");
        copied.in_point_secs = 0.0;
        copied.out_point_secs = 2.0;
        copied.profile = ExportProfile::LosslessCopy;
        let copied_path = run(copied).unwrap();
        assert!(
            crate::media::probe_video(&copied_path)
                .unwrap()
                .duration_secs
                > 1.9
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = execute_export(
            job,
            |_| {},
            move |result| {
                tx.send(result).unwrap();
            },
        );
        cancel.store(true, Ordering::Relaxed);
        assert!(
            rx.recv_timeout(Duration::from_secs(10))
                .unwrap()
                .unwrap_err()
                .contains("cancelled")
        );
        assert_eq!(std::fs::read(&encoded).unwrap(), original_bytes);
    }
}
