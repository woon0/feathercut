use super::{mpv::Engine, probe::VideoMetadata};
use crate::timeline::PreviewPlan;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

enum PlayerCommand {
    Play,
    Pause,
    Seek { id: u64, seconds: f64 },
    Step(i32),
    In(f64),
    Out(f64),
    Volume(f64),
    Reload(PreviewPlan, f64),
    Stop,
}
pub struct PlayerController {
    commands: Sender<PlayerCommand>,
    generation: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}
fn preview_size(width: u32, height: u32) -> (u32, u32) {
    let scale = (1280.0 / width.max(1) as f64)
        .min(720.0 / height.max(1) as f64)
        .min(1.0);
    // Four-byte pixels; align each row to 64 bytes for libmpv's SIMD scaler.
    let width = ((width as f64 * scale).round().max(1.0) as u32).div_ceil(16) * 16;
    (width, (height as f64 * scale).round().max(1.0) as u32)
}
type CropPlan = Vec<(f64, f64, crate::timeline::Crop, u32, u32)>;
fn cropped_frame(
    frame: SharedPixelBuffer<Rgba8Pixel>,
    time: f64,
    crops: &CropPlan,
) -> SharedPixelBuffer<Rgba8Pixel> {
    let Some((_, _, crop, sw, sh)) = crops
        .iter()
        .find(|(start, end, _, _, _)| time >= *start && time < *end)
    else {
        return frame;
    };
    let fw = frame.width();
    let fh = frame.height();
    let scale = (fw as f64 / *sw as f64).min(fh as f64 / *sh as f64);
    let cw = (*sw as f64 * scale).round().max(2.) as u32;
    let ch = (*sh as f64 * scale).round().max(2.) as u32;
    if *crop == crate::timeline::Crop::default() && cw == fw && ch == fh {
        return frame;
    }
    let (x, y, w, h) = crop.pixels(cw, ch);
    let x = (fw - cw) / 2 + x;
    let y = (fh - ch) / 2 + y;
    let mut result = SharedPixelBuffer::new(w, h);
    for row in 0..h {
        let source = ((y + row) * fw + x) as usize;
        let target = (row * w) as usize;
        result.make_mut_slice()[target..target + w as usize]
            .copy_from_slice(&frame.as_slice()[source..source + w as usize]);
    }
    result
}
fn prepare_black() -> Result<(), String> {
    let path = crate::timeline::black_path();
    if path.is_file() {
        return Ok(());
    }
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args([
        "-v",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=64x64:r=30:d=10",
        "-threads",
        "1",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
    ])
    .arg(&path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(())
}
struct ExtraAudio {
    engine: Engine,
    gains: Vec<(f64, f64, f64)>,
    ready: bool,
    volume: f64,
}
fn reload_extra(
    extra: &mut Vec<ExtraAudio>,
    plans: &[crate::timeline::AudioPreview],
    on_error: &Arc<impl Fn(String) + Send + Sync + 'static>,
) {
    extra.truncate(plans.len());
    for (i, p) in plans.iter().enumerate() {
        if let Some(a) = extra.get_mut(i) {
            let _ = a.engine.command(&["set", "pause", "yes"]);
            let _ = a.engine.command(&["loadfile", &p.path.to_string_lossy()]);
            a.ready = false;
            a.gains = p.gains.clone();
            a.volume = -1.;
        } else {
            match Engine::audio(&p.path) {
                Ok(engine) => extra.push(ExtraAudio {
                    engine,
                    gains: p.gains.clone(),
                    ready: false,
                    volume: -1.,
                }),
                Err(e) => on_error(e),
            }
        }
    }
}
impl PlayerController {
    pub fn new<F>(
        path: PathBuf,
        metadata: VideoMetadata,
        on_frame: F,
        on_state: impl Fn(bool, f64) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Self
    where
        F: Fn(SharedPixelBuffer<Rgba8Pixel>, f64, bool) + Send + Sync + 'static,
    {
        Self::new_internal(path, metadata, None, on_frame, on_state, on_error)
    }
    pub fn timeline<F>(
        plan: PreviewPlan,
        on_frame: F,
        on_state: impl Fn(bool, f64) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Self
    where
        F: Fn(SharedPixelBuffer<Rgba8Pixel>, f64, bool) + Send + Sync + 'static,
    {
        Self::new_internal(
            plan.video.clone(),
            plan.metadata.clone(),
            Some(plan),
            on_frame,
            on_state,
            on_error,
        )
    }
    fn new_internal<F>(
        path: PathBuf,
        mut metadata: VideoMetadata,
        mut plan: Option<PreviewPlan>,
        on_frame: F,
        on_state: impl Fn(bool, f64) + Send + Sync + 'static,
        on_error: impl Fn(String) + Send + Sync + 'static,
    ) -> Self
    where
        F: Fn(SharedPixelBuffer<Rgba8Pixel>, f64, bool) + Send + Sync + 'static,
    {
        let (commands, receiver) = channel();
        let generation = Arc::new(AtomicU64::new(0));
        let requested_generation = generation.clone();
        let worker = thread::spawn(move || {
            let on_error = Arc::new(on_error);
            if plan.as_ref().is_some_and(|p| p.needs_black)
                && let Err(e) = prepare_black()
            {
                on_error(format!("Could not prepare timeline gaps: {e}"));
                return;
            }
            let crops = Arc::new(Mutex::new(
                plan.as_ref().map(|p| p.crops.clone()).unwrap_or_default(),
            ));
            let render_crops = crops.clone();
            let on_frame = Arc::new(on_frame);
            let render_frame = move |frame, time, live| {
                let rendered = if let Ok(c) = render_crops.lock() {
                    cropped_frame(frame, time, &c)
                } else {
                    frame
                };
                on_frame(rendered, time, live);
            };
            let engine = match Engine::new(
                &path,
                preview_size(metadata.width, metadata.height),
                Arc::new(render_frame),
                on_error.clone(),
            ) {
                Ok(engine) => engine,
                Err(error) => {
                    on_error(format!("Could not start playback: {error}"));
                    on_state(false, 0.0);
                    return;
                }
            };
            let mut playing = false;
            let _ = engine.command(&[
                "set",
                "aid",
                if plan.as_ref().is_some_and(|p| !p.embedded_audio) {
                    "no"
                } else {
                    "auto"
                },
            ]);
            let mut audio = plan
                .as_ref()
                .and_then(|p| p.audio.as_ref())
                .and_then(|path| match Engine::audio(path) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        on_error(e);
                        None
                    }
                });
            let mut extra = Vec::new();
            if let Some(p) = &plan {
                reload_extra(&mut extra, &p.extra_audio, &on_error);
            }
            let mut master_volume = 80.;
            let mut audio_volume = -1.;
            let mut audio_ready = false;
            let mut load_seek = None;
            let mut last_sync = std::time::Instant::now();
            let mut current = 0.0;
            let mut trim_in = 0.0;
            let mut trim_out = metadata.duration_secs;
            let mut seek_id = 0;
            let mut seek_pending = false;
            let mut stepped = false;
            let mut last_frame = (metadata.duration_secs - 1.0 / metadata.fps).max(0.0);
            loop {
                let mut batch = Vec::new();
                match receiver.recv_timeout(Duration::from_millis(10)) {
                    Ok(command) => batch.push(command),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                batch.extend(receiver.try_iter());
                let mut seek = None;
                for command in batch {
                    let result = match command {
                        PlayerCommand::Reload(next, seconds) => {
                            if next.needs_black
                                && let Err(e) = prepare_black()
                            {
                                on_error(e);
                                continue;
                            }
                            if let Ok(mut c) = crops.lock() {
                                *c = next.crops.clone();
                            }
                            reload_extra(&mut extra, &next.extra_audio, &on_error);
                            audio_ready = false;
                            playing = false;
                            let _ = engine.command(&["set", "pause", "yes"]);
                            let _ = engine.command(&[
                                "set",
                                "aid",
                                if next.embedded_audio { "auto" } else { "no" },
                            ]);
                            metadata = next.metadata.clone();
                            trim_in = 0.;
                            trim_out = metadata.duration_secs;
                            last_frame = (metadata.duration_secs - 1. / metadata.fps).max(0.);
                            current = seconds.clamp(0., last_frame);
                            load_seek = Some(current);
                            seek_pending = true;
                            if let Some(path) = &next.audio {
                                if let Some(a) = &audio {
                                    let _ = a.command(&["set", "pause", "yes"]);
                                    let _ = a.command(&["loadfile", &path.to_string_lossy()]);
                                } else {
                                    audio = match Engine::audio(path) {
                                        Ok(a) => Some(a),
                                        Err(e) => {
                                            on_error(e);
                                            None
                                        }
                                    };
                                }
                            } else {
                                audio = None;
                            }
                            audio_volume = -1.;
                            let result =
                                engine.command(&["loadfile", &next.video.to_string_lossy()]);
                            plan = Some(next);
                            result
                        }
                        PlayerCommand::Stop => return,
                        PlayerCommand::In(seconds) => {
                            trim_in = seconds.clamp(0.0, metadata.duration_secs);
                            Ok(())
                        }
                        PlayerCommand::Out(seconds) => {
                            trim_out = seconds.clamp(trim_in, metadata.duration_secs);
                            Ok(())
                        }
                        PlayerCommand::Volume(volume) => {
                            master_volume = volume.clamp(0., 100.);
                            engine.command(&[
                                "set",
                                "volume",
                                &format!("{}", volume.clamp(0.0, 100.0)),
                            ])
                        }
                        PlayerCommand::Pause => {
                            for a in &extra {
                                let _ = a.engine.command(&["set", "pause", "yes"]);
                            }
                            playing = false;
                            if let Some(a) = &audio {
                                let _ = a.command(&["set", "pause", "yes"]);
                            }
                            engine.state(false, current);
                            on_state(false, current);
                            engine.command(&["set", "pause", "yes"])
                        }
                        PlayerCommand::Play => {
                            for a in &extra {
                                let _ = a.engine.command(&["set", "pause", "no"]);
                            }
                            if current < trim_in || current >= trim_out - 0.5 / metadata.fps {
                                seek = Some((seek_id, trim_in));
                            }
                            playing = true;
                            if let Some(a) = &audio {
                                let _ = a.command(&["set", "pause", "no"]);
                            }
                            engine.state(true, current);
                            on_state(true, current);
                            engine.command(&["set", "pause", "no"])
                        }
                        PlayerCommand::Seek { id, seconds } => {
                            if id == requested_generation.load(Ordering::Relaxed) {
                                seek = Some((id, seconds));
                            }
                            Ok(())
                        }
                        PlayerCommand::Step(delta) => {
                            for a in &extra {
                                let _ = a.engine.command(&["set", "pause", "yes"]);
                            }
                            playing = false;
                            if let Some(a) = &audio {
                                let _ = a.command(&["set", "pause", "yes"]);
                            }
                            stepped = true;
                            engine.state(false, current);
                            // mpv steps decoded source frames, including variable-frame-rate video.
                            engine.command(&[if delta < 0 {
                                "frame-back-step"
                            } else {
                                "frame-step"
                            }])
                        }
                    };
                    if let Err(error) = result {
                        on_error(error);
                    }
                }
                if let Some((id, seconds)) = seek {
                    for a in &extra {
                        if a.ready {
                            let _ = a.engine.seek(seconds);
                        }
                    }
                    seek_id = id;
                    current = seconds.clamp(0.0, last_frame);
                    seek_pending = true;
                    if let Err(error) = engine.seek(current) {
                        seek_pending = false;
                        on_error(error);
                    }
                    if audio_ready && let Some(a) = &audio {
                        let _ = a.seek(current);
                    }
                }
                while let Some((event, error)) = engine.event() {
                    if let Some(error) = error {
                        playing = false;
                        on_error(error);
                        on_state(false, current);
                    }
                    match event {
                        8 => {
                            engine.state(false, 0.0);
                            if let Some(target) = load_seek.take() {
                                let _ = engine.seek(target);
                            }
                        }
                        21 => {
                            engine.seek_finished();
                            seek_pending = false;
                            if let Some(time) = engine.number(c"time-pos") {
                                current = time;
                            }
                            if stepped && seek_id == requested_generation.load(Ordering::Relaxed) {
                                if audio_ready && let Some(a) = &audio {
                                    let _ = a.seek(current);
                                }
                                for a in &extra {
                                    if a.ready {
                                        let _ = a.engine.seek(current);
                                    }
                                }
                                on_state(false, current);
                                stepped = false;
                            }
                        }
                        7 => {
                            playing = false;
                            on_state(false, current);
                        }
                        _ => {}
                    }
                }
                if !seek_pending && let Some(position) = engine.number(c"time-pos") {
                    current = position;
                }
                if playing && (current >= trim_out || engine.flag(c"eof-reached")) {
                    playing = false;
                    let _ = engine.command(&["set", "pause", "yes"]);
                    if let Some(a) = &audio {
                        let _ = a.command(&["set", "pause", "yes"]);
                    }
                    for a in &extra {
                        let _ = a.engine.command(&["set", "pause", "yes"]);
                    }
                    current = trim_out;
                    on_state(false, current);
                }
                if let Some(a) = &audio {
                    while let Some((event, error)) = a.event() {
                        if let Some(e) = error {
                            on_error(format!("Audio preview: {e}"));
                        }
                        if event == 8 {
                            audio_ready = true;
                            let _ = a.seek(current);
                            let _ =
                                a.command(&["set", "pause", if playing { "no" } else { "yes" }]);
                        }
                        if event == 21 {
                            a.seek_finished();
                        }
                    }
                    let gain = plan
                        .as_ref()
                        .and_then(|p| {
                            p.gains
                                .iter()
                                .find(|(start, end, _)| current >= *start && current < *end)
                        })
                        .map(|(_, _, g)| *g)
                        .unwrap_or(0.);
                    let volume = master_volume * gain;
                    if (volume - audio_volume).abs() > 0.001 {
                        let _ = a.command(&["set", "volume", &format!("{volume:.3}")]);
                        audio_volume = volume;
                    }
                    if playing
                        && !seek_pending
                        && audio_ready
                        && last_sync.elapsed() > Duration::from_millis(250)
                        && a.number(c"time-pos")
                            .is_some_and(|t| (t - current).abs() > 0.06)
                    {
                        let _ = a.seek(current);
                        last_sync = std::time::Instant::now();
                    }
                }
                for a in &mut extra {
                    while let Some((event, error)) = a.engine.event() {
                        if let Some(e) = error {
                            on_error(format!("Audio layer: {e}"));
                        }
                        if event == 8 {
                            a.ready = true;
                            let _ = a.engine.seek(current);
                            let _ = a.engine.command(&[
                                "set",
                                "pause",
                                if playing { "no" } else { "yes" },
                            ]);
                        }
                        if event == 21 {
                            a.engine.seek_finished();
                        }
                    }
                    let gain = a
                        .gains
                        .iter()
                        .find(|(start, end, _)| current >= *start && current < *end)
                        .map(|(_, _, g)| *g)
                        .unwrap_or(0.);
                    let volume = master_volume * gain;
                    if (volume - a.volume).abs() > 0.001 {
                        let _ = a
                            .engine
                            .command(&["set", "volume", &format!("{volume:.3}")]);
                        a.volume = volume;
                    }
                    if playing
                        && !seek_pending
                        && a.ready
                        && last_sync.elapsed() > Duration::from_millis(250)
                        && a.engine
                            .number(c"time-pos")
                            .is_some_and(|t| (t - current).abs() > 0.06)
                    {
                        let _ = a.engine.seek(current);
                    }
                }
                engine.state(playing, current);
            }
        });
        Self {
            commands,
            generation,
            worker: Some(worker),
        }
    }
    pub fn play(&self) {
        let _ = self.commands.send(PlayerCommand::Play);
    }
    pub fn reload(&self, plan: PreviewPlan, position: f64) {
        let _ = self.commands.send(PlayerCommand::Reload(plan, position));
    }
    pub fn pause(&self) {
        let _ = self.commands.send(PlayerCommand::Pause);
    }
    pub fn seek(&self, seconds: f64) {
        let id = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.commands.send(PlayerCommand::Seek { id, seconds });
    }
    pub fn step(&self, delta: i32) {
        let _ = self.commands.send(PlayerCommand::Step(delta));
    }
    pub fn set_in_point(&self, seconds: f64) {
        let _ = self.commands.send(PlayerCommand::In(seconds));
    }
    pub fn set_out_point(&self, seconds: f64) {
        let _ = self.commands.send(PlayerCommand::Out(seconds));
    }
    pub fn volume(&self, value: f64) {
        let _ = self.commands.send(PlayerCommand::Volume(value));
    }
    pub fn stop(&self) {
        let _ = self.commands.send(PlayerCommand::Stop);
    }
}
impl Drop for PlayerController {
    fn drop(&mut self) {
        self.stop();
        // Closing/replacing a file should not wait for native decoder teardown on the UI thread.
        if let Some(worker) = self.worker.take() {
            thread::spawn(move || {
                let _ = worker.join();
            });
        }
    }
}
