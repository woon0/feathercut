use super::*;
use crate::timeline::{History, Project, Source};
use slint::{ModelRc, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

thread_local! { static EDITOR: RefCell<Option<Rc<Editor>>> = const { RefCell::new(None) }; }
type Player = Arc<Mutex<Option<PlayerController>>>;
pub struct Editor {
    ui: slint::Weak<MainWindow>,
    history: RefCell<History>,
    selected: Cell<(u64, bool)>,
    player: Player,
    thumbnails: RefCell<HashMap<PathBuf, Image>>,
    keys: RefCell<HashMap<PathBuf, Vec<f64>>>,
    waves: RefCell<HashMap<PathBuf, Vec<f32>>>,
    wave_cancel: RefCell<Arc<AtomicBool>>,
    import_epoch: Cell<u64>,
    pending: RefCell<Vec<(PathBuf, bool)>>,
    import_timer: slint::Timer,
    reload_timer: slint::Timer,
    asset_timer: slint::Timer,
    export: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    finished: Arc<AtomicBool>,
}
fn project_source_time(project: &Project, id: u64, time: f64) -> f64 {
    project
        .video
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.input + (time - c.start).clamp(0., c.duration()))
        .unwrap_or(0.)
}
fn on_editor(action: impl FnOnce(&Rc<Editor>)) {
    EDITOR.with(|e| {
        if let Some(e) = e.borrow().as_ref() {
            action(e);
        }
    });
}
fn probe_source(path: PathBuf, audio: bool) -> Result<Arc<Source>, String> {
    let path = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new("ffprobe");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let output = cmd
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,width,height,avg_frame_rate,r_frame_rate,duration:format=duration",
            "-of",
            "json",
        ])
        .arg(&path)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let streams = value["streams"]
        .as_array()
        .ok_or("No media streams found.")?;
    let has_audio = streams.iter().any(|s| s["codec_type"] == "audio");
    let video = streams.iter().find(|s| s["codec_type"] == "video");
    if audio && !has_audio {
        return Err("This file has no audio stream.".into());
    }
    if !audio && video.is_none() {
        return Err("This file has no video stream. Use Add audio for sound files.".into());
    }
    let duration = value["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            video
                .and_then(|v| v["duration"].as_str())
                .and_then(|s| s.parse::<f64>().ok())
        })
        .filter(|s| s.is_finite() && *s > 0.)
        .ok_or("Media duration is invalid.")?;
    let fps = video
        .and_then(|v| v["avg_frame_rate"].as_str())
        .and_then(|s| {
            let (n, d) = s.split_once('/')?;
            Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?)
        })
        .filter(|f| f.is_finite() && *f > 0.)
        .unwrap_or(30.);
    Ok(Arc::new(Source {
        path,
        metadata: VideoMetadata {
            duration_secs: duration,
            width: video.and_then(|v| v["width"].as_u64()).unwrap_or(640) as u32,
            height: video.and_then(|v| v["height"].as_u64()).unwrap_or(360) as u32,
            fps,
            keyframes: vec![],
        },
        has_audio,
    }))
}
impl Editor {
    fn refresh_assets(self: &Rc<Self>) {
        if self
            .ui
            .upgrade()
            .is_some_and(|ui| ui.get_timeline_busy() || ui.get_is_scrubbing() || ui.get_crop_mode())
        {
            let weak = Rc::downgrade(self);
            self.asset_timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(100),
                move || {
                    if let Some(e) = weak.upgrade() {
                        e.refresh_assets();
                    }
                },
            );
        } else {
            self.sync();
        }
    }
    fn message(&self, text: impl Into<slint::SharedString>) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_status_message(text.into());
        }
    }
    fn output_project(&self) -> Project {
        let p = &self.history.borrow().project;
        if self.ui.upgrade().is_some_and(|ui| ui.get_advanced_editor()) {
            p.clone()
        } else {
            p.selected_project(self.selected.get().0, false)
        }
    }
    fn output_duration(&self) -> f64 {
        self.output_project().duration()
    }
    fn selected_clip(&self) -> Option<(crate::timeline::Clip, f64)> {
        let h = self.history.borrow();
        let (id, audio) = self.selected.get();
        if audio {
            h.project
                .audio
                .iter()
                .find(|a| a.clip.id == id)
                .map(|a| (a.clip.clone(), a.start))
        } else {
            h.project
                .video
                .iter()
                .find(|c| c.id == id)
                .map(|c| (c.clone(), h.project.start(id).unwrap_or(0.)))
        }
    }
    fn selection(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some((clip, start)) = self.selected_clip() else {
            return;
        };
        let (id, audio) = self.selected.get();
        ui.set_selected_clip(id as i32);
        ui.set_selected_name(
            clip.source
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
                .into(),
        );
        if start + clip.duration() <= ui.get_view_start() as f64
            || start >= (ui.get_view_start() + ui.get_view_span()) as f64
        {
            ui.set_view_start(
                (start / ui.get_view_span() as f64).floor() as f32 * ui.get_view_span(),
            );
        }
        ui.set_selected_audio(audio);
        ui.set_selected_layer(clip.layer as i32);
        let count = if audio {
            self.history.borrow().project.audio_layers
        } else {
            self.history.borrow().project.video_layers
        };
        ui.set_layer_names(
            Rc::new(VecModel::from(
                (0..count)
                    .map(|i| format!("{}{}", if audio { "A" } else { "V" }, i + 1).into())
                    .collect::<Vec<slint::SharedString>>(),
            ))
            .into(),
        );
        ui.set_source_width(clip.source.metadata.width as f32);
        ui.set_source_height(clip.source.metadata.height as f32);
        ui.set_crop_x(clip.crop.x as f32);
        ui.set_crop_y(clip.crop.y as f32);
        ui.set_crop_w(clip.crop.w as f32);
        ui.set_crop_h(clip.crop.h as f32);
        let (_, _, cw, ch) = clip
            .crop
            .pixels(clip.source.metadata.width, clip.source.metadata.height);
        ui.set_crop_width_text(cw.to_string().into());
        ui.set_crop_height_text(ch.to_string().into());
        let h = self.history.borrow();
        let sound = h.project.audio.iter().find(|a| {
            if audio {
                a.clip.id == id
            } else {
                a.linked_video == Some(id)
            }
        });
        ui.set_selected_has_audio(sound.is_some());
        ui.set_selected_linked(sound.is_some_and(|a| a.linked_video.is_some()));
        ui.set_selected_muted(sound.is_some_and(|a| a.muted));
        ui.set_clip_gain(sound.map(|a| a.gain * 100.).unwrap_or(0.) as f32);
        ui.set_in_point_secs(if ui.get_advanced_editor() {
            start as f32
        } else {
            clip.input as f32
        });
        ui.set_out_point_secs(if ui.get_advanced_editor() {
            (start + clip.duration()) as f32
        } else {
            clip.output as f32
        });
        ui.set_in_time_text(trim_time_text(clip.input as f32).into());
        ui.set_out_time_text(trim_time_text(clip.output as f32).into());
        ui.set_selection_duration_text(format!("{:.2} seconds", clip.duration()).into());
        ui.set_source_fps(clip.source.metadata.fps as f32);
        ui.set_audio_position_text(trim_time_text(start as f32).into());
    }
    fn sync(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let h = self.history.borrow();
        let p = &h.project;
        ui.set_can_undo(h.can_undo());
        ui.set_can_redo(h.can_redo());
        if p.video.is_empty() {
            drop(h);
            reset_media_ui(&ui);
            ui.set_sequence_mode(false);
            ui.set_video_clips(ModelRc::default());
            ui.set_audio_clips(ModelRc::default());
            if let Ok(mut player) = self.player.lock() {
                *player = None;
            }
            return;
        }
        let valid = if self.selected.get().1 {
            p.audio.iter().any(|a| a.clip.id == self.selected.get().0)
        } else {
            p.video.iter().any(|c| c.id == self.selected.get().0)
        };
        if !valid {
            self.selected.set((p.video[0].id, false));
        }
        let thumbs = self.thumbnails.borrow();
        let name = |path: &PathBuf| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        let video: Vec<_> = p
            .video
            .iter()
            .map(|c| TimelineClip {
                id: c.id as i32,
                name: name(&c.source.path).into(),
                start: c.start as f32,
                layer: c.layer as i32,
                waveform: self.wave(&c.source, c.input, c.output),
                duration: c.duration() as f32,
                thumbnail: thumbs.get(&c.source.path).cloned().unwrap_or_default(),
                linked: p.audio.iter().any(|a| a.linked_video == Some(c.id)),
                muted: false,
            })
            .collect();
        let audio: Vec<_> = p
            .audio
            .iter()
            .filter(|a| a.linked_video.is_none())
            .map(|a| TimelineClip {
                id: a.clip.id as i32,
                name: name(&a.clip.source.path).into(),
                start: a.start as f32,
                layer: a.clip.layer as i32,
                waveform: self.wave(&a.clip.source, a.clip.input, a.clip.output),
                duration: a.clip.duration() as f32,
                thumbnail: Image::default(),
                linked: a.linked_video.is_some(),
                muted: a.muted,
            })
            .collect();
        let canvas = if ui.get_advanced_editor() {
            &p.video[0]
        } else {
            p.video
                .iter()
                .find(|c| c.id == self.selected.get().0)
                .unwrap_or(&p.video[0])
        };
        let (_, _, width, height) = canvas
            .crop
            .pixels(canvas.source.metadata.width, canvas.source.metadata.height);
        ui.set_canvas_aspect(width as f32 / height as f32);
        ui.set_video_layers(p.video_layers as i32);
        ui.set_audio_layers(p.audio_layers as i32);
        ui.set_video_clips(Rc::new(VecModel::from(video)).into());
        ui.set_audio_clips(Rc::new(VecModel::from(audio)).into());
        if !ui.get_has_video() {
            ui.set_view_start(0.);
            ui.set_view_span((p.duration() * 1.3).max(5.) as f32);
        }
        ui.set_duration_secs(p.duration() as f32);
        ui.set_sequence_mode(ui.get_advanced_editor());
        ui.set_has_video(true);
        ui.set_file_name(
            if p.video.len() == 1 {
                name(&p.video[0].source.path)
            } else {
                format!("{} video clips", p.video.len())
            }
            .into(),
        );
        ui.set_file_path(p.video[0].source.path.to_string_lossy().into_owned().into());
        ui.set_video_info_str(
            format!(
                "{:.1}s · {} × {}",
                p.duration(),
                p.video[0].source.metadata.width,
                p.video[0].source.metadata.height
            )
            .into(),
        );
        ui.set_end_time_text(trim_time_text(p.duration() as f32).into());
        ui.set_trim_summary_str(format!("Entire timeline · {:.2} seconds", p.duration()).into());
        ui.set_export_summary(
            format!(
                "{} video clips · {:.2} seconds",
                p.video.len(),
                p.duration()
            )
            .into(),
        );
        ui.set_can_fast_copy(if ui.get_advanced_editor() {
            p.simple_source().is_some()
        } else {
            p.selected_project(self.selected.get().0, false)
                .simple_source()
                .is_some()
        });
        if ui.get_advanced_editor() {
            ui.set_current_time_secs(
                ui.get_current_time_secs()
                    .min((p.duration() - 1. / p.video[0].source.metadata.fps).max(0.) as f32),
            );
        }
        ui.set_timecode_display(
            format_timecode(ui.get_current_time_secs() as f64, p.duration()).into(),
        );
        drop(h);
        self.selection();
        if !ui.get_advanced_editor()
            && let Some((c, _)) = self.selected_clip()
        {
            ui.set_file_name(
                c.source
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            );
            ui.set_file_path(c.source.path.to_string_lossy().into_owned().into());
            ui.set_video_info_str(
                format!(
                    "{} × {} · {:.2} fps · {:.1}s",
                    c.source.metadata.width,
                    c.source.metadata.height,
                    c.source.metadata.fps,
                    c.source.metadata.duration_secs
                )
                .into(),
            );
            ui.set_duration_secs(c.source.metadata.duration_secs as f32);
            ui.set_in_point_secs(c.input as f32);
            ui.set_out_point_secs(c.output as f32);
            ui.set_end_time_text(trim_time_text(c.source.metadata.duration_secs as f32).into());
            ui.set_trim_summary_str(format_trim_summary(c.input, c.output).into());
            ui.set_timecode_display(
                format_timecode(
                    ui.get_current_time_secs() as f64,
                    c.source.metadata.duration_secs,
                )
                .into(),
            );
            ui.set_thumbnails(
                Rc::new(VecModel::from(vec![
                    self.thumbnails
                        .borrow()
                        .get(&c.source.path)
                        .cloned()
                        .unwrap_or_default();
                    8
                ]))
                .into(),
            );
            ui.set_export_summary("Selected clip".into());
        }
    }
    fn wave(&self, source: &Source, input: f64, output: f64) -> ModelRc<f32> {
        let waves = self.waves.borrow();
        let values = waves
            .get(&source.path)
            .map(|p| media::waveform::range(p, source.metadata.duration_secs, input, output))
            .unwrap_or_default();
        Rc::new(VecModel::from(values)).into()
    }
    fn waveforms(&self, sources: Vec<Arc<Source>>, epoch: u64) {
        let cancel = self.wave_cancel.borrow().clone();
        std::thread::spawn(move || {
            for source in sources.into_iter().filter(|s| s.has_audio) {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let result =
                    media::waveform::peaks(&source.path, source.metadata.duration_secs, &cancel);
                let _ = slint::invoke_from_event_loop(move || {
                    on_editor(|e| {
                        if e.import_epoch.get() != epoch {
                            return;
                        }
                        if let Ok(peaks) = result {
                            e.waves.borrow_mut().insert(source.path.clone(), peaks);
                            e.refresh_assets();
                        }
                    })
                });
            }
        });
    }
    fn edit(
        self: &Rc<Self>,
        action: impl FnOnce(&mut Project) -> Result<Option<(u64, bool)>, String>,
    ) -> bool {
        if self.ui.upgrade().is_some_and(|ui| ui.get_is_exporting()) {
            return false;
        }
        let result = self.history.borrow_mut().edit(|p| {
            let selected = action(p)?;
            if !p.video.is_empty() {
                p.preview()?;
            }
            Ok(selected)
        });
        match result {
            Ok(selected) => {
                if let Some(s) = selected {
                    self.selected.set(s);
                }
                self.sync();
                self.schedule_reload();
                self.message("Timeline updated · Ctrl+Z to undo");
                true
            }
            Err(e) => {
                self.selection();
                self.message(e);
                false
            }
        }
    }
    fn schedule_reload(self: &Rc<Self>) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_is_playing(false);
        }
        if let Ok(player) = self.player.lock()
            && let Some(player) = &*player
        {
            player.pause();
        }
        let weak = Rc::downgrade(self);
        self.reload_timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(120),
            move || {
                if let Some(e) = weak.upgrade() {
                    e.reload();
                }
            },
        );
    }
    fn reload(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let project = self.history.borrow().project.clone();
        let selected = self.selected.get().0;
        let preview_project = if !ui.get_advanced_editor() || ui.get_crop_mode() {
            let mut p = project.selected_project(selected, true);
            if ui.get_crop_mode() {
                for v in &mut p.video {
                    v.crop = crate::timeline::Crop::default();
                }
            }
            p
        } else {
            project
        };
        let plan = match preview_project.preview() {
            Ok(p) => p,
            Err(_) => return,
        };
        if let Ok(player) = self.player.lock()
            && let Some(player) = &*player
        {
            let time = if ui.get_crop_mode() && ui.get_advanced_editor() {
                project_source_time(
                    &self.history.borrow().project,
                    selected,
                    ui.get_current_time_secs() as f64,
                )
            } else {
                ui.get_current_time_secs() as f64
            };
            player.reload(plan, time);
            if !ui.get_advanced_editor() {
                player.set_in_point(ui.get_in_point_secs() as f64);
                player.set_out_point(ui.get_out_point_secs() as f64);
            }
            return;
        }
        let generation = ui.get_media_generation();
        let frame_ui = ui.as_weak();
        let state_ui = ui.as_weak();
        let error_ui = ui.as_weak();
        let pending = Arc::new(AtomicBool::new(false));
        let player = PlayerController::timeline(
            plan,
            move |pixels, time, live| {
                if live && pending.swap(true, Ordering::Relaxed) {
                    return;
                }
                let weak = frame_ui.clone();
                let pending = pending.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if live {
                        pending.store(false, Ordering::Relaxed);
                    }
                    if let Some(ui) = weak.upgrade()
                        && ui.get_has_video()
                        && ui.get_media_generation() == generation
                    {
                        ui.set_video_frame(Image::from_rgba8(pixels));
                        if live && ui.get_is_playing() && !ui.get_is_scrubbing() {
                            ui.set_current_time_secs(time as f32);
                            ui.set_timecode_display(
                                format_timecode(time, ui.get_duration_secs() as f64).into(),
                            );
                        }
                    }
                });
            },
            move |playing, time| {
                let weak = state_ui.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade()
                        && ui.get_has_video()
                        && ui.get_media_generation() == generation
                    {
                        ui.set_is_playing(playing);
                        if !playing && !ui.get_is_scrubbing() && !ui.get_crop_mode() {
                            ui.set_current_time_secs(time as f32);
                            ui.set_timecode_display(
                                format_timecode(time, ui.get_duration_secs() as f64).into(),
                            );
                        }
                    }
                });
            },
            move |error| {
                let weak = error_ui.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade()
                        && ui.get_media_generation() == generation
                    {
                        ui.set_status_message(error.into());
                    }
                });
            },
        );
        player.volume(ui.get_volume() as f64);
        if !ui.get_advanced_editor() {
            player.set_in_point(ui.get_in_point_secs() as f64);
            player.set_out_point(ui.get_out_point_secs() as f64);
        }
        if let Ok(mut active) = self.player.lock() {
            *active = Some(player);
        }
    }
    fn queue(self: &Rc<Self>, paths: Vec<PathBuf>, audio: bool) {
        if self.ui.upgrade().is_some_and(|ui| ui.get_is_exporting()) {
            return;
        }
        self.pending
            .borrow_mut()
            .extend(paths.into_iter().map(|p| (p, audio)));
        let weak = Rc::downgrade(self);
        self.import_timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(25),
            move || {
                if let Some(e) = weak.upgrade() {
                    e.import();
                }
            },
        );
    }
    fn import(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if ui.get_is_loading() {
            let weak = Rc::downgrade(self);
            self.import_timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(50),
                move || {
                    if let Some(e) = weak.upgrade() {
                        e.import();
                    }
                },
            );
            return;
        }
        let paths = std::mem::take(&mut *self.pending.borrow_mut());
        if paths.is_empty() {
            return;
        }
        ui.set_is_loading(true);
        self.message("Reading clip metadata…");
        let epoch = self.import_epoch.get();
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let sources: Vec<_> = paths
                .into_iter()
                .map(|(path, audio)| (probe_source(path, audio), audio))
                .collect();
            let _ = slint::invoke_from_event_loop(move || {
                if weak.upgrade().is_some() {
                    on_editor(|e| {
                        if e.import_epoch.get() != epoch {
                            return;
                        }
                        if let Some(ui) = e.ui.upgrade() {
                            ui.set_is_loading(false);
                        }
                        let mut thumbnails = Vec::new();
                        let mut waveform_sources = Vec::new();
                        let mut errors = Vec::new();
                        for (result, audio) in sources {
                            match result {
                                Ok(source) => {
                                    let cursor =
                                        e.ui.upgrade()
                                            .map(|ui| ui.get_current_time_secs() as f64)
                                            .unwrap_or(0.);
                                    let cached = e
                                        .history
                                        .borrow()
                                        .project
                                        .video
                                        .iter()
                                        .any(|c| c.source.path == source.path);
                                    let source_copy = source.clone();
                                    if !e.waves.borrow().contains_key(&source.path) {
                                        waveform_sources.push(source.clone());
                                    }
                                    if audio && let Some(ui) = e.ui.upgrade() {
                                        ui.set_advanced_editor(true);
                                    }
                                    let advanced =
                                        e.ui.upgrade().is_some_and(|ui| ui.get_advanced_editor());
                                    if !advanced
                                        && !audio
                                        && let Some(ui) = e.ui.upgrade()
                                    {
                                        ui.set_current_time_secs(0.);
                                    }
                                    let added = e.edit(|p| {
                                        let id = if audio {
                                            p.add_audio(source, cursor)?
                                        } else {
                                            if !advanced {
                                                *p = Project::default();
                                            }
                                            p.add_video(source)
                                        };
                                        Ok(Some((id, audio)))
                                    });
                                    if !added && let Some(ui) = e.ui.upgrade() {
                                        errors.push(ui.get_status_message().to_string());
                                    }
                                    if added && !audio && !cached {
                                        thumbnails.push(source_copy);
                                    }
                                }
                                Err(error) => errors.push(error),
                            }
                        }
                        if let Some(ui) = e.ui.upgrade() {
                            ui.set_view_start(0.);
                            ui.set_view_span(
                                (e.history.borrow().project.duration() * 1.3).max(5.) as f32
                            );
                        }
                        if e.ui.upgrade().is_some_and(|ui| ui.get_has_video()) {
                            e.reload_timer.stop();
                            e.reload();
                        }
                        if !errors.is_empty() {
                            e.message(errors.join(" · "));
                        } else {
                            e.message("Clips ready · S to split · drag to reorder");
                        }
                        e.thumbnails(thumbnails, epoch);
                        e.waveforms(waveform_sources, epoch);
                    });
                }
            });
        });
    }
    fn thumbnails(&self, sources: Vec<Arc<Source>>, epoch: u64) {
        if sources.is_empty() {
            return;
        }
        std::thread::spawn(move || {
            for source in sources {
                let mut cmd = std::process::Command::new("ffmpeg");
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                let output = cmd
                    .args(["-v", "error", "-nostdin", "-ss"])
                    .arg(format!("{:.6}", source.metadata.duration_secs / 2.))
                    .arg("-i")
                    .arg(&source.path)
                    .args([
                        "-an",
                        "-sn",
                        "-threads",
                        "2",
                        "-vf",
                        "scale=160:90:force_original_aspect_ratio=increase,crop=160:90",
                        "-frames:v",
                        "1",
                        "-f",
                        "rawvideo",
                        "-pix_fmt",
                        "rgb24",
                        "-",
                    ])
                    .output();
                if let Ok(out) = output
                    && out.status.success()
                    && out.stdout.len() == 160 * 90 * 3
                {
                    let mut pixels = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(160, 90);
                    pixels.make_mut_bytes().copy_from_slice(&out.stdout);
                    let path = source.path.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        on_editor(|e| {
                            if e.import_epoch.get() == epoch {
                                e.thumbnails
                                    .borrow_mut()
                                    .insert(path, Image::from_rgb8(pixels));
                                e.refresh_assets();
                            }
                        })
                    });
                }
            }
        });
    }
    fn trim(self: &Rc<Self>, out: bool, value: f64) {
        if let Some((c, _)) = self.selected_clip() {
            let audio = self.selected.get().1;
            self.edit(|p| {
                p.trim(
                    c.id,
                    audio,
                    if out { c.input } else { value },
                    if out { value } else { c.output },
                )?;
                Ok(None)
            });
        }
    }
    fn keys(self: &Rc<Self>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let preview = if ui.get_advanced_editor() {
            self.history.borrow().project.clone()
        } else {
            self.history
                .borrow()
                .project
                .selected_project(self.selected.get().0, false)
        };
        let Some(c) = preview.simple_source().cloned() else {
            ui.set_keyframes_ready(false);
            ui.set_copy_range_summary("Edited timelines need encoding.".into());
            return;
        };
        if let Some(keys) = self.keys.borrow().get(&c.source.path) {
            let (start, end) =
                media::probe::copy_range(keys, c.input, c.output, c.source.metadata.duration_secs);
            ui.set_copy_range_summary(
                format!("Source keyframes: {}", format_trim_summary(start, end)).into(),
            );
            ui.set_keyframes_ready(true);
            return;
        }
        ui.set_keyframes_ready(false);
        ui.set_copy_range_summary("Finding source keyframes…".into());
        let path = c.source.path.clone();
        let epoch = self.import_epoch.get();
        std::thread::spawn(move || {
            let result = media::probe::probe_keyframes(&path);
            let _ = slint::invoke_from_event_loop(move || {
                on_editor(|e| {
                    if e.import_epoch.get() != epoch {
                        return;
                    }
                    match result {
                        Ok(keys) => {
                            e.keys.borrow_mut().insert(path, keys);
                            e.keys();
                        }
                        Err(error) => {
                            if let Some(ui) = e.ui.upgrade() {
                                ui.set_copy_range_summary(error.into());
                            }
                        }
                    }
                })
            });
        });
    }
    fn export(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if ui.get_is_exporting() {
            return;
        }
        let full = self.history.borrow().project.clone();
        let project = if ui.get_advanced_editor() {
            full
        } else {
            full.selected_project(self.selected.get().0, false)
        };
        let Some(first) = project.video.first() else {
            return;
        };
        let mode = ui.get_export_mode_idx();
        if mode == 2 && project.simple_source().is_none() {
            ui.set_export_status_text(
                "Choose Target size or Lossless for an edited timeline.".into(),
            );
            return;
        }
        if mode == 0
            && ui
                .get_target_size_str()
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.1 && *v <= 50000.)
                .is_none()
        {
            ui.set_export_status_text("Enter a target size from 0.1 to 50,000 MiB.".into());
            return;
        }
        let profile = if mode == 2 {
            ExportProfile::LosslessCopy
        } else if mode == 1 {
            ExportProfile::LosslessEncode
        } else {
            let (codec, presets) = match ui.get_codec_idx() {
                0 => (ExportCodec::HevcNvenc10Bit, ["p2", "p4", "p6"]),
                1 => (ExportCodec::Libx265_10Bit, ["fast", "medium", "slow"]),
                2 => (ExportCodec::H264Nvenc, ["p2", "p4", "p6"]),
                _ => (ExportCodec::Libx264, ["fast", "medium", "slow"]),
            };
            ExportProfile::TargetSize {
                target_mb: ui.get_target_size_mb() as f64,
                codec,
                fps: match ui.get_fps_idx() {
                    1 => 24.,
                    2 => 30.,
                    3 => 60.,
                    _ => first.source.metadata.fps,
                },
                preset: presets[ui.get_preset_idx().clamp(0, 2) as usize].into(),
            }
        };
        let mut simple = project.simple_source().cloned();
        if mode == 2
            && let Some(c) = &mut simple
        {
            let keys = self.keys.borrow();
            let Some(keys) = keys.get(&c.source.path) else {
                ui.set_export_status_text("Please wait for source keyframe analysis.".into());
                return;
            };
            (c.input, c.output) =
                media::probe::copy_range(keys, c.input, c.output, c.source.metadata.duration_secs);
        }
        let extension = if mode == 0 { "mp4" } else { "mkv" };
        let Some(destination) = rfd::FileDialog::new()
            .set_title("Export timeline")
            .set_file_name(format!(
                "{}_edit.{extension}",
                first
                    .source
                    .path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
            ))
            .add_filter("Video", &[extension])
            .save_file()
        else {
            return;
        };
        ui.set_is_exporting(true);
        ui.set_export_status_text("Starting export…".into());
        self.finished.store(false, Ordering::Relaxed);
        let status_ui = ui.as_weak();
        let done_ui = ui.as_weak();
        let finished = self.finished.clone();
        let status = move |message: String| {
            let weak = status_ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_export_status_text(message.into());
                }
            });
        };
        let complete = move |result: Result<PathBuf, String>| {
            finished.store(true, Ordering::Relaxed);
            let weak = done_ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_is_exporting(false);
                    match result {
                        Ok(path) => {
                            ui.set_show_export_modal(false);
                            ui.set_status_message(
                                format!(
                                    "Saved {}",
                                    path.file_name().unwrap_or_default().to_string_lossy()
                                )
                                .into(),
                            );
                        }
                        Err(error) => {
                            ui.set_export_status_text(error.clone().into());
                            ui.set_status_message(error.into());
                        }
                    }
                }
            });
        };
        let cancel = if let Some(c) = simple {
            execute_export(
                ExportJob {
                    input_path: c.source.path.clone(),
                    output_path: destination,
                    in_point_secs: c.input,
                    out_point_secs: c.output,
                    profile,
                },
                status,
                complete,
            )
        } else {
            media::export::execute_project_export(project, destination, profile, status, complete)
        };
        if let Ok(mut guard) = self.export.lock() {
            *guard = Some(cancel);
        }
    }
}
pub fn install(
    ui: &MainWindow,
    player: Player,
    export: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    finished: Arc<AtomicBool>,
) -> Rc<Editor> {
    let editor = Rc::new(Editor {
        ui: ui.as_weak(),
        history: RefCell::default(),
        selected: Cell::new((0, false)),
        player,
        thumbnails: RefCell::default(),
        keys: RefCell::default(),
        waves: RefCell::default(),
        wave_cancel: RefCell::new(Arc::new(AtomicBool::new(false))),
        import_epoch: Cell::new(0),
        pending: RefCell::default(),
        import_timer: slint::Timer::default(),
        reload_timer: slint::Timer::default(),
        asset_timer: slint::Timer::default(),
        export,
        finished,
    });
    EDITOR.with(|e| *e.borrow_mut() = Some(editor.clone()));
    macro_rules! bind { ($name:ident, |$e:ident $(,$arg:ident)*| $body:block) => {{ let weak = Rc::downgrade(&editor); ui.$name(move |$($arg),*| { if let Some($e) = weak.upgrade() { $body } }); }}; }
    bind!(on_choose_file, |e| {
        let advanced = e.ui.upgrade().is_some_and(|ui| ui.get_advanced_editor());
        if !advanced {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Open video")
                .add_filter("Video", &["mp4", "mov", "mkv", "webm", "avi", "ts"])
                .pick_file()
            {
                e.queue(vec![path], false);
            }
            return;
        }
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Add video clips")
            .add_filter("Video", &["mp4", "mov", "mkv", "webm", "avi", "ts"])
            .pick_files()
        {
            e.queue(paths, false);
        }
    });
    bind!(on_add_audio, |e| {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Add audio clips")
            .add_filter(
                "Audio or video",
                &[
                    "wav", "mp3", "flac", "m4a", "aac", "ogg", "opus", "mp4", "mkv", "mov", "webm",
                ],
            )
            .pick_files()
        {
            e.queue(paths, true);
        }
    });
    bind!(on_import_path, |e, path| {
        let path = PathBuf::from(path.as_str());
        let audio = path.extension().is_some_and(|v| {
            ["wav", "mp3", "flac", "m4a", "aac", "ogg", "opus"]
                .iter()
                .any(|ext| v.eq_ignore_ascii_case(ext))
        });
        e.queue(vec![path], audio);
    });
    bind!(on_seek, |e, raw| {
        if let Some(ui) = e.ui.upgrade() {
            let duration = ui.get_duration_secs() as f64;
            let target = (raw as f64).clamp(0., duration);
            ui.set_current_time_secs(target as f32);
            ui.set_timecode_display(format_timecode(target, duration).into());
            if let Ok(player) = e.player.lock()
                && let Some(player) = &*player
            {
                player.seek(target);
            }
        }
    });
    bind!(on_select_clip, |e, id, audio| {
        e.selected.set((id as u64, audio));
        e.selection();
    });
    bind!(on_move_clip, |e, id, audio, time, layer| {
        e.edit(|p| {
            if audio {
                p.move_audio_layer(id as u64, time as f64, layer.max(0) as u32)?;
            } else {
                p.move_video(id as u64, time as f64, layer.max(0) as u32)?;
            }
            Ok(None)
        });
    });
    bind!(on_nudge_clip, |e, delta| {
        let (id, audio) = e.selected.get();
        e.edit(|p| {
            if audio {
                let a = p
                    .audio
                    .iter()
                    .find(|a| a.clip.id == id)
                    .ok_or("Select an audio clip.")?;
                p.move_audio(id, (a.start + delta as f64 / 30.).max(0.))?;
            } else {
                let c = p.video.iter().find(|c| c.id == id).ok_or("Select video.")?;
                p.move_video(
                    id,
                    (c.start + delta as f64 / c.source.metadata.fps.max(1.)).max(0.),
                    c.layer,
                )?;
            }
            Ok(None)
        });
    });
    bind!(on_split_clip, |e| {
        let time =
            e.ui.upgrade()
                .map(|ui| ui.get_current_time_secs() as f64)
                .unwrap_or(0.);
        let (id, audio) = e.selected.get();
        e.edit(|p| {
            Ok(Some((
                if audio {
                    p.split_audio(id, time)?
                } else {
                    p.split_video_at(id, time)?
                },
                audio,
            )))
        });
    });
    bind!(on_remove_clip, |e| {
        let (id, audio) = e.selected.get();
        e.edit(|p| {
            p.remove(id, audio);
            Ok(None)
        });
    });
    bind!(on_detach_audio, |e| {
        let (id, audio) = e.selected.get();
        e.edit(|p| {
            let video = if audio {
                p.audio
                    .iter()
                    .find(|a| a.clip.id == id)
                    .and_then(|a| a.linked_video)
                    .ok_or("Audio is already detached.")?
            } else {
                id
            };
            Ok(Some((p.detach(video)?, true)))
        });
    });
    bind!(on_change_clip_gain, |e, gain| {
        let (id, _) = e.selected.get();
        e.edit(|p| {
            let muted = p
                .audio
                .iter()
                .find(|a| a.clip.id == id || a.linked_video == Some(id))
                .is_some_and(|a| a.muted);
            p.gain(id, gain as f64 / 100., muted)?;
            Ok(None)
        });
    });
    bind!(on_mute_clip, |e| {
        let (id, _) = e.selected.get();
        e.edit(|p| {
            let a = p
                .audio
                .iter()
                .find(|a| a.clip.id == id || a.linked_video == Some(id))
                .ok_or("This clip has no audio.")?;
            p.gain(id, a.gain, !a.muted)?;
            Ok(None)
        });
    });
    bind!(on_edit_audio_position, |e, text| {
        if let Some(start) = parse_trim_time(&text) {
            let (id, _) = e.selected.get();
            e.edit(|p| {
                p.move_audio(id, start as f64)?;
                Ok(None)
            });
        }
        e.selection();
    });
    bind!(on_undo_edit, |e, redo| {
        if e.ui.upgrade().is_some_and(|u| u.get_is_exporting()) {
            return;
        }
        if redo {
            e.history.borrow_mut().redo();
        } else {
            e.history.borrow_mut().undo();
        }
        e.sync();
        e.schedule_reload();
    });
    bind!(on_refresh_trim_labels, |e| {
        if let Some(ui) = e.ui.upgrade()
            && !ui.get_sequence_mode()
        {
            refresh_trim_labels(&ui);
        }
    });
    bind!(on_edit_trim, |e, out, text| {
        if let Some(value) = parse_trim_time(&text) {
            e.trim(out, value as f64);
        }
        e.selection();
    });
    bind!(on_step_trim, |e, out, delta| {
        if let Some((c, _)) = e.selected_clip() {
            let value = if out { c.output } else { c.input };
            e.trim(out, value + delta as f64 / c.source.metadata.fps.max(1.));
        }
    });
    bind!(on_update_in_point, |e, time| {
        if let Some((c, start)) = e.selected_clip() {
            e.trim(
                false,
                if e.ui.upgrade().is_some_and(|ui| ui.get_advanced_editor()) {
                    c.input + time as f64 - start
                } else {
                    time as f64
                },
            );
        }
    });
    bind!(on_trim_clip, |e, out, delta| {
        let (id, audio) = e.selected.get();
        let fps = e
            .selected_clip()
            .map(|(c, _)| c.source.metadata.fps.max(1.))
            .unwrap_or(30.);
        e.edit(|p| {
            let delta = if audio {
                delta as f64
            } else {
                (delta as f64 * fps).round() / fps
            };
            p.trim_edge(id, audio, out, delta)?;
            Ok(None)
        });
    });
    bind!(on_update_out_point, |e, time| {
        if let Some((c, start)) = e.selected_clip() {
            e.trim(
                true,
                if e.ui.upgrade().is_some_and(|ui| ui.get_advanced_editor()) {
                    c.input + time as f64 - start
                } else {
                    time as f64
                },
            );
        }
    });
    bind!(on_set_in_to_playhead, |e| {
        if let Some(ui) = e.ui.upgrade() {
            ui.invoke_update_in_point(ui.get_current_time_secs());
        }
    });
    bind!(on_set_out_to_playhead, |e| {
        if let Some(ui) = e.ui.upgrade() {
            ui.invoke_update_out_point(ui.get_current_time_secs());
        }
    });
    bind!(on_reset_trim, |e| {
        if let Some((c, _)) = e.selected_clip() {
            let audio = e.selected.get().1;
            e.edit(|p| {
                p.trim(c.id, audio, 0., c.source.metadata.duration_secs)?;
                Ok(None)
            });
        }
    });
    bind!(on_toggle_play, |e| {
        if e.ui.upgrade().is_some_and(|ui| ui.get_crop_mode()) {
            return;
        }
        let pending = e.reload_timer.running();
        e.reload_timer.stop();
        if pending {
            e.reload();
        }
        if let Some(ui) = e.ui.upgrade()
            && let Ok(player) = e.player.lock()
            && let Some(player) = &*player
        {
            if ui.get_is_playing() {
                player.pause();
            } else {
                if ui.get_current_time_secs() >= ui.get_duration_secs() - 0.03 {
                    player.seek(0.);
                }
                player.play();
            }
        }
    });
    bind!(on_open_export_modal, |e| {
        if let Some(ui) = e.ui.upgrade() {
            if ui.get_is_loading() || !e.pending.borrow().is_empty() {
                e.message("Wait for clip metadata before exporting.");
                return;
            }
            if let Ok(player) = e.player.lock()
                && let Some(p) = &*player
            {
                p.pause();
            }
            ui.set_is_playing(false);
            ui.set_export_status_text("".into());
            ui.set_estimated_bitrate_str(
                format!(
                    "{} kbps",
                    calculate_target_bitrate_kbps(
                        e.output_duration(),
                        ui.get_target_size_mb() as f64
                    )
                )
                .into(),
            );
            ui.set_show_export_modal(true);
            if ui.get_export_mode_idx() == 2 {
                e.keys();
            }
        }
    });
    bind!(on_export_mode_changed, |e| {
        if e.ui.upgrade().is_some_and(|u| u.get_export_mode_idx() == 2) {
            e.keys();
        }
    });
    bind!(on_snap_copy_range, |e| {
        let c = e.output_project().simple_source().cloned();
        if let Some(c) = c {
            let range = e.keys.borrow().get(&c.source.path).map(|keys| {
                media::probe::copy_range(keys, c.input, c.output, c.source.metadata.duration_secs)
            });
            if let Some((start, end)) = range {
                e.edit(|p| {
                    p.trim(c.id, false, start, end)?;
                    Ok(None)
                });
                e.keys();
            }
        }
    });
    bind!(on_start_export, |e| {
        e.export();
    });
    bind!(on_set_target_size, |e, size| {
        if let Some(ui) = e.ui.upgrade() {
            let size = size.clamp(0.1, 50000.);
            ui.set_target_size_mb(size);
            ui.set_target_size_str(format!("{size:.1}").into());
            ui.set_estimated_bitrate_str(
                format!(
                    "{} kbps",
                    calculate_target_bitrate_kbps(e.output_duration(), size as f64)
                )
                .into(),
            );
        }
    });
    bind!(on_target_size_edited, |e, text| {
        if let Some(ui) = e.ui.upgrade() {
            ui.set_target_size_str(text.clone());
            if let Ok(size) = text.trim().parse::<f64>()
                && size.is_finite()
                && (0.1..=50000.).contains(&size)
            {
                ui.set_target_size_mb(size as f32);
                ui.set_estimated_bitrate_str(
                    format!(
                        "{} kbps",
                        calculate_target_bitrate_kbps(e.history.borrow().project.duration(), size)
                    )
                    .into(),
                );
            }
        }
    });
    bind!(on_mode_changed, |e| {
        if let Some(ui) = e.ui.upgrade() {
            if e.selected.get().1
                && let Some(v) = e.history.borrow().project.video.first()
            {
                e.selected.set((v.id, false));
            }
            if let Some((c, start)) = e.selected_clip() {
                let time = ui.get_current_time_secs() as f64;
                ui.set_current_time_secs(if ui.get_advanced_editor() {
                    (start + (time - c.input).clamp(0., c.duration())) as f32
                } else {
                    (c.input + (time - start).clamp(0., c.duration())) as f32
                });
            }
            e.sync();
            e.schedule_reload();
        }
    });
    bind!(on_add_layer, |e, audio| {
        e.edit(|p| {
            p.add_layer(audio);
            Ok(None)
        });
    });
    bind!(on_move_selected_layer, |e, layer| {
        let (id, audio) = e.selected.get();
        e.edit(|p| {
            if audio {
                let start = p
                    .audio
                    .iter()
                    .find(|a| a.clip.id == id)
                    .ok_or("Select audio.")?
                    .start;
                p.move_audio_layer(id, start, layer.max(0) as u32)?;
            } else {
                let start = p.start(id).ok_or("Select video.")?;
                p.move_video(id, start, layer.max(0) as u32)?;
            }
            Ok(None)
        });
    });
    bind!(on_begin_crop, |e| {
        if let Some(ui) = e.ui.upgrade() {
            if e.selected.get().1 {
                return;
            }
            if let Ok(p) = e.player.lock()
                && let Some(p) = &*p
            {
                p.pause();
            }
            ui.set_is_playing(false);
            ui.set_crop_aspect(0);
            ui.set_crop_mode(true);
            e.selection();
            e.reload();
        }
    });
    bind!(on_crop_commit, |e| {
        if let Some(ui) = e.ui.upgrade() {
            let id = e.selected.get().0;
            let crop = crate::timeline::Crop {
                x: ui.get_crop_x() as f64,
                y: ui.get_crop_y() as f64,
                w: ui.get_crop_w() as f64,
                h: ui.get_crop_h() as f64,
            };
            e.edit(|p| {
                p.crop(id, crop)?;
                Ok(None)
            });
        }
    });
    bind!(on_end_crop, |e| {
        if let Some(ui) = e.ui.upgrade() {
            ui.set_crop_mode(false);
            e.sync();
            e.reload();
        }
    });
    bind!(on_reset_crop, |e| {
        if let Some(ui) = e.ui.upgrade() {
            ui.set_crop_aspect(0);
        }
        let id = e.selected.get().0;
        e.edit(|p| {
            p.crop(id, crate::timeline::Crop::default())?;
            Ok(None)
        });
    });
    bind!(on_crop_ratio, |e, index| {
        if let Some(ui) = e.ui.upgrade() {
            let ratio = match index {
                1 => 16. / 9.,
                2 => 9. / 16.,
                3 => 1.,
                4 => 4. / 3.,
                5 => 3. / 2.,
                _ => return,
            };
            let natural = ui.get_source_width() / ui.get_source_height().max(1.);
            let mut w = ui.get_crop_w();
            let mut h = w * natural / ratio;
            if h > ui.get_crop_h() {
                h = ui.get_crop_h();
                w = h * ratio / natural;
            }
            ui.set_crop_x((ui.get_crop_x() + ui.get_crop_w() / 2. - w / 2.).clamp(0., 1. - w));
            ui.set_crop_y((ui.get_crop_y() + ui.get_crop_h() / 2. - h / 2.).clamp(0., 1. - h));
            ui.set_crop_w(w);
            ui.set_crop_h(h);
            ui.invoke_crop_commit();
        }
    });
    bind!(on_crop_dimension, |e, width, text| {
        if let Some(ui) = e.ui.upgrade()
            && let Ok(value) = text.trim().parse::<u32>()
        {
            let max = if width {
                ui.get_source_width()
            } else {
                ui.get_source_height()
            };
            let value = ((value.max(2) / 2 * 2) as f32 / max).clamp(2. / max, 1.);
            ui.set_crop_aspect(0);
            if width {
                ui.set_crop_w(value);
                ui.set_crop_x(ui.get_crop_x().min(1. - value));
            } else {
                ui.set_crop_h(value);
                ui.set_crop_y(ui.get_crop_y().min(1. - value));
            }
            ui.invoke_crop_commit();
        }
    });
    bind!(on_clear_file, |e| {
        if e.ui.upgrade().is_some_and(|u| u.get_is_exporting()) {
            return;
        }
        e.import_epoch.set(e.import_epoch.get() + 1);
        e.pending.borrow_mut().clear();
        e.import_timer.stop();
        e.reload_timer.stop();
        *e.history.borrow_mut() = History::default();
        e.thumbnails.borrow_mut().clear();
        e.waves.borrow_mut().clear();
        e.wave_cancel.borrow().store(true, Ordering::Relaxed);
        *e.wave_cancel.borrow_mut() = Arc::new(AtomicBool::new(false));
        e.keys.borrow_mut().clear();
        if let Some(ui) = e.ui.upgrade() {
            ui.set_is_loading(false);
        }
        e.sync();
    });
    editor
}
