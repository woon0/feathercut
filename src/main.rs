#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

slint::include_modules!();

mod editor;
mod media;
#[cfg(test)]
mod sequence_verification;
mod settings;
mod timeline;
#[cfg(test)]
mod ui_verification;
mod window_glass;

use media::{
    ExportCodec, ExportJob, ExportProfile, PlayerController, VideoMetadata,
    calculate_target_bitrate_kbps, execute_export, probe_video,
};
use slint::Image;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} bytes", bytes)
    }
}

fn format_timecode(current: f64, total: f64) -> String {
    fn fmt_time(t: f64) -> String {
        let total_secs = t.max(0.0);
        let mins = (total_secs / 60.0).floor() as u64;
        let secs = total_secs % 60.0;
        format!("{:02}:{:04.1}", mins, secs)
    }
    format!("{} / {}", fmt_time(current), fmt_time(total))
}

fn format_trim_summary(in_sec: f64, out_sec: f64) -> String {
    fn fmt_time(t: f64) -> String {
        let total_secs = t.max(0.0);
        let mins = (total_secs / 60.0).floor() as u64;
        let secs = total_secs % 60.0;
        format!("{:02}:{:04.1}", mins, secs)
    }
    let duration = (out_sec - in_sec).max(0.0);
    format!(
        "{} – {} • {:.1}s",
        fmt_time(in_sec),
        fmt_time(out_sec),
        duration
    )
}

fn refresh_copy_summary(ui: &MainWindow, metadata: &VideoMetadata) {
    if metadata.keyframes.is_empty() {
        return;
    }
    let (start, end) = media::probe::copy_range(
        &metadata.keyframes,
        ui.get_in_point_secs() as f64,
        ui.get_out_point_secs() as f64,
        metadata.duration_secs,
    );
    ui.set_copy_range_summary(
        format!(
            "Keyframe-aligned range: {}\nOriginal streams are copied within these boundaries.",
            format_trim_summary(start, end)
        )
        .into(),
    );
    ui.set_keyframes_ready(true);
}

fn finish_load(
    path: PathBuf,
    ui: MainWindow,
    player_ref: &Arc<Mutex<Option<PlayerController>>>,
    path_ref: &Arc<Mutex<Option<PathBuf>>>,
    meta_ref: &Arc<Mutex<Option<VideoMetadata>>>,
    metadata: VideoMetadata,
    generation: i32,
) {
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown File".to_string());

    let path_str = path.to_string_lossy().to_string();
    let size_str = match std::fs::metadata(&path) {
        Ok(meta) => format_file_size(meta.len()),
        Err(_) => "Unknown size".to_string(),
    };

    let info_str = format!(
        "{} × {} • {:.0} fps • {}",
        metadata.width, metadata.height, metadata.fps, size_str
    );
    let duration = metadata.duration_secs;

    if let Ok(mut guard) = player_ref.lock() {
        *guard = None;
    }

    if let Ok(mut guard) = path_ref.lock() {
        *guard = Some(path.clone());
    }
    if let Ok(mut guard) = meta_ref.lock() {
        *guard = Some(metadata.clone());
    }

    let initial_bitrate = calculate_target_bitrate_kbps(duration, 10.0);

    ui.set_file_name(file_name.into());
    ui.set_file_path(path_str.into());
    ui.set_file_size_str(size_str.into());
    ui.set_video_info_str(info_str.into());
    ui.set_duration_secs(duration as f32);
    ui.set_source_fps(metadata.fps.max(1.0) as f32);
    ui.set_current_time_secs(0.0);
    ui.set_in_point_secs(0.0);
    ui.set_out_point_secs(duration as f32);
    ui.set_trim_summary_str(format_trim_summary(0.0, duration).into());
    ui.set_timecode_display(format_timecode(0.0, duration).into());
    ui.set_target_size_mb(10.0);
    ui.set_target_size_str("10.0".into());
    ui.set_export_mode_idx(0);
    ui.set_estimated_bitrate_str(format!("{} kbps", initial_bitrate).into());
    ui.set_has_video(true);
    ui.set_video_frame(Image::default());
    ui.set_video_clips(slint::ModelRc::default());
    ui.set_audio_clips(slint::ModelRc::default());
    ui.set_sequence_mode(false);
    ui.set_selected_clip(-1);
    ui.set_selected_name("".into());
    ui.set_crop_mode(false);
    ui.set_timeline_busy(false);
    ui.set_selected_audio(false);
    ui.set_selected_linked(false);
    ui.set_selected_has_audio(false);
    ui.set_selected_muted(false);
    ui.set_clip_gain(100.);
    ui.set_is_playing(false);
    ui.set_is_scrubbing(false);
    let show_export = std::env::args().any(|a| a == "--show-export");
    ui.set_show_export_modal(show_export);
    ui.set_status_message("Video loaded".into());
    ui.set_thumbnails(std::rc::Rc::new(slint::VecModel::from(vec![Image::default(); 8])).into());
    ui.set_export_status_text("".into());
    ui.set_fps_idx(0);
    ui.set_keyframes_ready(false);
    ui.set_copy_range_summary("Finding source keyframes…".into());
    let keyframe_ui = ui.as_weak();
    let keyframe_path = path.clone();
    let keyframe_metadata = meta_ref.clone();
    std::thread::spawn(move || {
        let result = media::probe::probe_keyframes(&keyframe_path);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = keyframe_ui.upgrade()
                && ui.get_media_generation() == generation
            {
                match result {
                    Ok(keys) => {
                        if let Ok(mut guard) = keyframe_metadata.lock()
                            && let Some(metadata) = guard.as_mut()
                        {
                            metadata.keyframes = keys;
                            refresh_copy_summary(&ui, metadata);
                        }
                    }
                    Err(error) => ui.set_copy_range_summary(
                        format!("Could not analyze keyframes: {error}").into(),
                    ),
                }
            }
        });
    });
    let thumbnail_ui = ui.as_weak();
    let thumbnail_path = path.clone();
    std::thread::spawn(move || {
        for index in 0..8 {
            let time = duration * (index as f64 + 0.5) / 8.0;
            let mut command = std::process::Command::new("ffmpeg");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            let output = command
                .args(["-v", "error", "-nostdin", "-ss"])
                .arg(format!("{time:.6}"))
                .arg("-i")
                .arg(&thumbnail_path)
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
            if let Ok(output) = output
                && output.status.success()
                && output.stdout.len() == 160 * 90 * 3
            {
                let mut pixels = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(160, 90);
                pixels.make_mut_bytes().copy_from_slice(&output.stdout);
                let weak = thumbnail_ui.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    use slint::Model;
                    if let Some(ui) = weak.upgrade()
                        && ui.get_media_generation() == generation
                        && ui.get_has_video()
                    {
                        let model = ui.get_thumbnails();
                        if let Some(model) = model.as_any().downcast_ref::<slint::VecModel<Image>>()
                        {
                            model.set_row_data(index, Image::from_rgb8(pixels));
                        }
                    }
                });
            }
        }
    });

    let ui_for_frame = ui.as_weak();
    let ui_for_state = ui.as_weak();
    let ui_for_error = ui.as_weak();
    let frame_pending = Arc::new(AtomicBool::new(false));

    let controller = PlayerController::new(
        path,
        metadata,
        move |pixel_buf, pos_sec, is_live_stream| {
            if is_live_stream && frame_pending.swap(true, Ordering::Relaxed) {
                return;
            }
            let pending = frame_pending.clone();
            let ui = ui_for_frame.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if is_live_stream {
                    pending.store(false, Ordering::Relaxed);
                }
                if let Some(u) = ui.upgrade() {
                    if u.get_media_generation() != generation || !u.get_has_video() {
                        return;
                    }
                    let img = Image::from_rgba8(pixel_buf);
                    u.set_video_frame(img);
                    if is_live_stream && u.get_is_playing() && !u.get_is_scrubbing() {
                        u.set_current_time_secs(pos_sec as f32);
                        u.set_timecode_display(format_timecode(pos_sec, duration).into());
                    }
                }
            });
        },
        move |is_playing, pos_sec| {
            let ui = ui_for_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(u) = ui.upgrade() {
                    if u.get_media_generation() != generation || !u.get_has_video() {
                        return;
                    }
                    u.set_is_playing(is_playing);
                    if !is_playing && !u.get_is_scrubbing() {
                        u.set_current_time_secs(pos_sec as f32);
                        u.set_timecode_display(format_timecode(pos_sec, duration).into());
                    }
                }
            });
        },
        move |error| {
            let ui = ui_for_error.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade()
                    && ui.get_media_generation() == generation
                {
                    ui.set_status_message(error.into());
                }
            });
        },
    );
    controller.volume(ui.get_volume() as f64);

    if let Ok(mut guard) = player_ref.lock() {
        *guard = Some(controller);
    }
}

fn load_video(
    path: PathBuf,
    ui: MainWindow,
    player_ref: &Arc<Mutex<Option<PlayerController>>>,
    path_ref: &Arc<Mutex<Option<PathBuf>>>,
    meta_ref: &Arc<Mutex<Option<VideoMetadata>>>,
) {
    if ui.get_is_exporting() {
        return;
    }
    let generation = ui.get_media_generation().wrapping_add(1);
    ui.set_media_generation(generation);
    ui.set_is_loading(true);
    ui.set_is_playing(false);
    ui.set_has_video(false);
    ui.set_show_export_modal(false);
    ui.set_status_message("Reading video…".into());
    if let Ok(mut player) = player_ref.lock() {
        *player = None;
    }
    if let Ok(mut stored) = path_ref.lock() {
        *stored = None;
    }
    if let Ok(mut stored) = meta_ref.lock() {
        *stored = None;
    }
    let weak = ui.as_weak();
    let player = player_ref.clone();
    let stored_path = path_ref.clone();
    let stored_meta = meta_ref.clone();
    std::thread::spawn(move || {
        let result = probe_video(&path);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_media_generation() != generation {
                    return;
                }
                ui.set_is_loading(false);
                match result {
                    Ok(metadata) => finish_load(
                        path,
                        ui,
                        &player,
                        &stored_path,
                        &stored_meta,
                        metadata,
                        generation,
                    ),
                    Err(error) => {
                        ui.set_status_message(format!("Could not open video: {error}").into())
                    }
                }
            }
        });
    });
}

fn minimum_trim_gap(ui: &MainWindow) -> f32 {
    ui.get_duration_secs().min(0.01)
}

fn trim_time_text(seconds: f32) -> String {
    let seconds = seconds.max(0.0) as f64;
    // Round once before splitting so 59.96 seconds becomes 01:00.0.
    let tenths = (seconds * 10.0).round() as u64;
    format!(
        "{:02}:{:02}.{}",
        tenths / 600,
        tenths / 10 % 60,
        tenths % 10
    )
}

fn parse_trim_time(text: &str) -> Option<f32> {
    let text = text.trim();
    let result = if let Some((minutes, seconds)) = text.split_once(':') {
        let minutes = minutes.parse::<u64>().ok()?;
        let seconds = seconds.parse::<f64>().ok()?;
        if !(0.0..60.0).contains(&seconds) {
            return None;
        }
        minutes as f64 * 60.0 + seconds
    } else {
        text.parse::<f64>().ok()?
    };
    (result.is_finite() && result >= 0.0 && result <= f32::MAX as f64).then_some(result as f32)
}

fn refresh_trim_labels(ui: &MainWindow) {
    ui.set_in_time_text(trim_time_text(ui.get_in_point_secs()).into());
    ui.set_out_time_text(trim_time_text(ui.get_out_point_secs()).into());
    ui.set_end_time_text(trim_time_text(ui.get_duration_secs()).into());
    ui.set_selection_duration_text(
        format!(
            "{:.1} seconds",
            ui.get_out_point_secs() - ui.get_in_point_secs()
        )
        .into(),
    );
}

fn reset_media_ui(ui: &MainWindow) {
    // Invalidate queued decoder/thumbnail updates before releasing their images.
    ui.set_media_generation(ui.get_media_generation().wrapping_add(1));
    ui.set_thumbnails(std::rc::Rc::new(slint::VecModel::<Image>::from(Vec::new())).into());
    ui.set_video_frame(Image::default());
    ui.set_video_clips(slint::ModelRc::default());
    ui.set_audio_clips(slint::ModelRc::default());
    ui.set_sequence_mode(false);
    ui.set_selected_clip(-1);
    ui.set_selected_name("".into());
    ui.set_crop_mode(false);
    ui.set_timeline_busy(false);
    ui.set_selected_audio(false);
    ui.set_selected_linked(false);
    ui.set_selected_has_audio(false);
    ui.set_selected_muted(false);
    ui.set_clip_gain(100.);
    ui.set_has_video(false);
    ui.set_file_name("".into());
    ui.set_file_path("".into());
    ui.set_file_size_str("".into());
    ui.set_video_info_str("".into());
    ui.set_is_playing(false);
    ui.set_is_scrubbing(false);
    ui.set_show_export_modal(false);
    ui.set_target_size_mb(10.0);
    ui.set_target_size_str("10.0".into());
    ui.set_export_mode_idx(0);
    ui.set_in_drag_snapped(false);
    ui.set_out_drag_snapped(false);
    ui.set_current_time_secs(0.0);
    ui.set_duration_secs(1.0);
    ui.set_in_point_secs(0.0);
    ui.set_out_point_secs(1.0);
    ui.set_timecode_display("00:00.0 / 00:00.0".into());
    ui.set_trim_summary_str("".into());
    ui.set_copy_range_summary("".into());
    ui.set_keyframes_ready(false);
    ui.set_status_message("Ready".into());
}

fn main() -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_window_attributes_hook(|attributes| {
            let attributes = attributes.with_transparent(true).with_decorations(false);
            #[cfg(windows)]
            let attributes = {
                use winit::platform::windows::WindowAttributesExtWindows;
                // Avoid winit's non-client 1px shadow strip; all rims are drawn by Slint.
                attributes.with_undecorated_shadow(false)
            };
            attributes
        })
        .select()?;
    let main_window = MainWindow::new()?;
    let appearance = settings::Appearance::load(&settings::Appearance::path());
    main_window.set_transparency(appearance.transparency);
    main_window.set_blur_amount(appearance.blur);
    main_window.set_theme_idx(i32::from(appearance.theme));
    {
        let weak = main_window.as_weak();
        let save_timer = std::rc::Rc::new(slint::Timer::default());
        main_window.on_appearance_changed(move || {
            if let Some(ui) = weak.upgrade() {
                let transparency = ui.get_transparency();
                let blur = ui.get_blur_amount();
                if let Some(active) = ui
                    .window()
                    .with_winit_window(|window| window_glass::apply(window, transparency, blur))
                {
                    ui.set_backdrop_available(active);
                }
                let weak = ui.as_weak();
                save_timer.start(
                    slint::TimerMode::SingleShot,
                    std::time::Duration::from_millis(250),
                    move || {
                        if let Some(ui) = weak.upgrade() {
                            let appearance = settings::Appearance {
                                transparency: ui.get_transparency(),
                                blur: ui.get_blur_amount(),
                                theme: ui.get_theme_idx().clamp(0, 1) as u8,
                            };
                            if let Err(error) = appearance.save(&settings::Appearance::path()) {
                                ui.set_status_message(
                                    format!("Could not save appearance: {error}").into(),
                                );
                            }
                        }
                    },
                );
            }
        });
    }
    main_window
        .window()
        .set_size(slint::LogicalSize::new(1060.0, 720.0));
    {
        let weak = main_window.as_weak();
        main_window.on_refresh_trim_labels(move || {
            if let Some(ui) = weak.upgrade() {
                refresh_trim_labels(&ui);
            }
        });
        let weak = main_window.as_weak();
        main_window.on_edit_trim(move |is_out, text| {
            if let Some(ui) = weak.upgrade() {
                if let Some(time) = parse_trim_time(&text) {
                    if is_out {
                        ui.invoke_update_out_point(time);
                    } else {
                        ui.invoke_update_in_point(time);
                    }
                }
                refresh_trim_labels(&ui);
            }
        });
        let weak = main_window.as_weak();
        main_window.on_step_trim(move |is_out, delta| {
            if let Some(ui) = weak.upgrade() {
                let fps = ui.get_source_fps().max(1.0);
                let time = if is_out {
                    ui.get_out_point_secs()
                } else {
                    ui.get_in_point_secs()
                };
                let time = time + delta as f32 / fps;
                if is_out {
                    ui.invoke_update_out_point(time);
                } else {
                    ui.invoke_update_in_point(time);
                }
            }
        });
    }
    let active_player: Arc<Mutex<Option<PlayerController>>> = Arc::new(Mutex::new(None));
    let loaded_video_path: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let loaded_metadata: Arc<Mutex<Option<VideoMetadata>>> = Arc::new(Mutex::new(None));
    let active_export: Arc<Mutex<Option<Arc<AtomicBool>>>> = Arc::new(Mutex::new(None));
    let export_finished = Arc::new(AtomicBool::new(true));
    {
        let weak = main_window.as_weak();
        main_window.on_window_command(move |command| {
            if let Some(ui) = weak.upgrade() {
                if command == 2 {
                    let _ = slint::quit_event_loop();
                    return;
                }
                ui.window().with_winit_window(|window| match command {
                    0 => window.set_minimized(true),
                    1 => {
                        let maximized = !window.is_maximized();
                        window.set_maximized(maximized);
                        ui.set_maximized(maximized);
                    }
                    3 => {
                        let _ = window.drag_window();
                    }
                    _ => {}
                });
            }
        });
        let weak = main_window.as_weak();
        main_window.on_resize_window(move |edge| {
            use winit::window::ResizeDirection as D;
            let direction = match edge {
                0 => D::NorthWest,
                1 => D::NorthEast,
                2 => D::SouthWest,
                3 => D::SouthEast,
                4 => D::West,
                5 => D::East,
                6 => D::North,
                _ => D::South,
            };
            if let Some(ui) = weak.upgrade() {
                ui.window().with_winit_window(|window| {
                    let _ = window.drag_resize_window(direction);
                });
            }
        });
    }
    {
        let player = active_player.clone();
        let weak = main_window.as_weak();
        main_window.on_set_volume(move |value| {
            let value = value.clamp(0.0, 100.0);
            if let Some(ui) = weak.upgrade() {
                ui.set_volume(value);
            }
            if let Ok(guard) = player.lock()
                && let Some(player) = &*guard
            {
                player.volume(value as f64);
            }
        });
    }
    {
        let weak = main_window.as_weak();
        let meta = loaded_metadata.clone();
        let player = active_player.clone();
        main_window.on_snap_copy_range(move || {
            if let Some(ui) = weak.upgrade()
                && let Ok(guard) = meta.lock()
                && let Some(metadata) = &*guard
            {
                if metadata.keyframes.is_empty() {
                    return;
                }
                let (start, end) = media::probe::copy_range(
                    &metadata.keyframes,
                    ui.get_in_point_secs() as f64,
                    ui.get_out_point_secs() as f64,
                    metadata.duration_secs,
                );
                ui.set_in_point_secs(start as f32);
                ui.set_out_point_secs(end as f32);
                ui.set_current_time_secs(start as f32);
                ui.set_trim_summary_str(format_trim_summary(start, end).into());
                ui.set_timecode_display(format_timecode(start, metadata.duration_secs).into());
                if let Ok(guard) = player.lock()
                    && let Some(player) = &*guard
                {
                    player.set_in_point(start);
                    player.set_out_point(end);
                    player.seek(start);
                }
                refresh_copy_summary(&ui, metadata);
            }
        });
    }
    {
        let weak = main_window.as_weak();

        let mut backdrop_applied = false;
        main_window
            .window()
            .on_winit_window_event(move |window, event| {
                if let Some(ui) = weak.upgrade() {
                    if !backdrop_applied
                        && let Some(available) = window.with_winit_window(|window| {
                            window_glass::apply(window, ui.get_transparency(), ui.get_blur_amount())
                        })
                    {
                        ui.set_backdrop_available(available);
                        backdrop_applied = true;
                    }
                    match event {
                        winit::event::WindowEvent::Resized(_) => {
                            window.with_winit_window(|window| {
                                window_glass::clear_caption(window);
                                window_glass::resize(window);
                                ui.set_maximized(window.is_maximized())
                            });
                        }
                        winit::event::WindowEvent::HoveredFile(_) => {
                            ui.set_drop_hovered(!ui.get_is_exporting())
                        }
                        winit::event::WindowEvent::HoveredFileCancelled => {
                            ui.set_drop_hovered(false)
                        }
                        winit::event::WindowEvent::DroppedFile(file) => {
                            ui.set_drop_hovered(false);
                            if !ui.get_show_export_modal() {
                                ui.invoke_import_path(file.to_string_lossy().into_owned().into());
                            }
                        }
                        _ => {}
                    }
                }
                EventResult::Propagate
            });
    }

    // 1. Choose File Callback
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        let path_ref = Arc::clone(&loaded_video_path);
        let meta_ref = Arc::clone(&loaded_metadata);

        main_window.on_choose_file(move || {
            let file = rfd::FileDialog::new()
                .set_title("Select a video to trim")
                .add_filter("Video Files", &["mp4", "mov", "mkv", "webm", "avi", "ts"])
                .pick_file();

            if let Some(path) = file
                && let Some(ui) = ui_handle.upgrade()
            {
                load_video(path, ui, &player_ref, &path_ref, &meta_ref);
            }
        });
    }

    // 2. Play / Pause Toggle Callback
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);

        main_window.on_toggle_play(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let currently_playing = ui.get_is_playing();
                if let Ok(guard) = player_ref.lock()
                    && let Some(ref player) = *guard
                {
                    if currently_playing {
                        player.pause();
                    } else {
                        let cur = ui.get_current_time_secs() as f64;
                        let in_pt = ui.get_in_point_secs() as f64;
                        let out_pt = ui.get_out_point_secs() as f64;
                        // If parked at or within 1 frame of the end bracket, restart from in-point
                        if cur >= out_pt - 0.08 {
                            ui.set_current_time_secs(in_pt as f32);
                            player.seek(in_pt);
                        }
                        player.play();
                    }
                }
            }
        });
    }

    // 3. Seek Timeline Callback (Magnetic snapping + Left/Right CTRL bypass + Instant truth)
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        main_window.on_seek(move |raw_sec| {
            if let Some(ui) = ui_handle.upgrade() {
                let dur = ui.get_duration_secs() as f64;
                let in_pt = ui.get_in_point_secs() as f64;
                let out_pt = ui.get_out_point_secs() as f64;
                let ctrl_held = ui.get_is_ctrl_held();

                let snap_dist = 0.25;
                let target_sec = if ctrl_held {
                    (raw_sec as f64).clamp(0.0, dur)
                } else if ((raw_sec as f64) - in_pt).abs() < snap_dist {
                    in_pt
                } else if ((raw_sec as f64) - out_pt).abs() < snap_dist {
                    out_pt
                } else if (raw_sec as f64) < snap_dist {
                    0.0
                } else if (raw_sec as f64) > dur - snap_dist {
                    dur
                } else {
                    (raw_sec as f64).clamp(0.0, dur)
                };

                // The latest pressed position is immediately set as the source of truth
                ui.set_current_time_secs(target_sec as f32);
                ui.set_timecode_display(format_timecode(target_sec, dur).into());

                if let Ok(guard) = player_ref.lock()
                    && let Some(ref player) = *guard
                {
                    player.seek(target_sec);
                }
            }
        });
    }

    // 4. Step Frame Callback
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        main_window.on_step_frame(move |delta| {
            if let Some(ui) = ui_handle.upgrade()
                && ui.get_is_playing()
            {
                ui.set_is_playing(false);
            }
            if let Ok(guard) = player_ref.lock()
                && let Some(ref player) = *guard
            {
                player.step(delta);
            }
        });
    }

    // 5. Set In-Point to Playhead (Shortcut: I)
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        main_window.on_set_in_to_playhead(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let cur = ui.get_current_time_secs() as f64;
                let out = ui.get_out_point_secs() as f64;
                if cur <= out - minimum_trim_gap(&ui) as f64 {
                    ui.set_in_point_secs(cur as f32);
                    ui.set_trim_summary_str(format_trim_summary(cur, out).into());
                    if let Ok(guard) = player_ref.lock()
                        && let Some(ref player) = *guard
                    {
                        player.set_in_point(cur);
                    }
                }
            }
        });
    }

    // 6. Set Out-Point to Playhead (Shortcut: O)
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        main_window.on_set_out_to_playhead(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let cur = ui.get_current_time_secs() as f64;
                let in_pt = ui.get_in_point_secs() as f64;
                if cur >= in_pt + minimum_trim_gap(&ui) as f64 {
                    ui.set_out_point_secs(cur as f32);
                    ui.set_trim_summary_str(format_trim_summary(in_pt, cur).into());
                    if let Ok(guard) = player_ref.lock()
                        && let Some(ref player) = *guard
                    {
                        player.set_out_point(cur);
                    }
                }
            }
        });
    }

    // 7. Reset Trim (Shortcut: R)
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        main_window.on_reset_trim(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let dur = ui.get_duration_secs() as f64;
                ui.set_in_point_secs(0.0);
                ui.set_out_point_secs(dur as f32);
                ui.set_trim_summary_str(format_trim_summary(0.0, dur).into());
                if let Ok(guard) = player_ref.lock()
                    && let Some(ref player) = *guard
                {
                    player.set_in_point(0.0);
                    player.set_out_point(dur);
                }
            }
        });
    }

    // 8. Update In-Point via Dragging [ Handle
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        main_window.on_update_in_point(move |new_in| {
            if let Some(ui) = ui_handle.upgrade() {
                let out = ui.get_out_point_secs();
                let dur = ui.get_duration_secs();
                let clamped = new_in.clamp(0.0, (out - minimum_trim_gap(&ui)).max(0.0));
                let was_snapped = ui.get_in_drag_snapped();
                ui.set_in_point_secs(clamped);
                ui.set_trim_summary_str(format_trim_summary(clamped as f64, out as f64).into());
                if was_snapped {
                    ui.set_current_time_secs(clamped);
                    ui.set_timecode_display(format_timecode(clamped as f64, dur as f64).into());
                }
                if let Ok(guard) = player_ref.lock()
                    && let Some(ref player) = *guard
                {
                    player.set_in_point(clamped as f64);
                    if was_snapped {
                        player.seek(clamped as f64);
                    }
                }
            }
        });
    }

    // 9. Update Out-Point via Dragging ] Handle
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        main_window.on_update_out_point(move |new_out| {
            if let Some(ui) = ui_handle.upgrade() {
                let in_pt = ui.get_in_point_secs();
                let dur = ui.get_duration_secs();
                let clamped = new_out.clamp((in_pt + minimum_trim_gap(&ui)).min(dur), dur);
                let was_snapped = ui.get_out_drag_snapped();
                ui.set_out_point_secs(clamped);
                ui.set_trim_summary_str(format_trim_summary(in_pt as f64, clamped as f64).into());
                if was_snapped {
                    ui.set_current_time_secs(clamped);
                    ui.set_timecode_display(format_timecode(clamped as f64, dur as f64).into());
                }
                if let Ok(guard) = player_ref.lock()
                    && let Some(ref player) = *guard
                {
                    player.set_out_point(clamped as f64);
                    if was_snapped {
                        player.seek(clamped as f64);
                    }
                }
            }
        });
    }

    // 10. Open Export Modal
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        let meta_ref = loaded_metadata.clone();
        main_window.on_open_export_modal(move || {
            // Auto pause playback when opening export modal
            if let Ok(guard) = player_ref.lock()
                && let Some(ref player) = *guard
            {
                player.pause();
            }
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_is_playing(false);
                ui.set_export_status_text("".into());
                let duration =
                    (ui.get_out_point_secs() - ui.get_in_point_secs()).max(0.0001) as f64;
                let target_mb = ui.get_target_size_mb() as f64;
                let bitrate = calculate_target_bitrate_kbps(duration, target_mb);
                ui.set_estimated_bitrate_str(format!("{} kbps", bitrate).into());
                if let Ok(guard) = meta_ref.lock()
                    && let Some(metadata) = &*guard
                {
                    refresh_copy_summary(&ui, metadata);
                }
                ui.set_show_export_modal(true);
            }
        });
    }

    // 11. Close Export Modal
    {
        let ui_handle = main_window.as_weak();
        let export_ref = active_export.clone();
        main_window.on_close_export_modal(move || {
            if let Some(ui) = ui_handle.upgrade() {
                if ui.get_is_exporting() {
                    if let Ok(guard) = export_ref.lock()
                        && let Some(flag) = &*guard
                    {
                        flag.store(true, Ordering::Relaxed);
                    }
                    ui.set_export_status_text("Cancelling…".into());
                } else {
                    ui.set_show_export_modal(false);
                }
            }
        });
    }

    // 12. Set Target Size Preset
    {
        let ui_handle = main_window.as_weak();
        main_window.on_set_target_size(move |size_mb| {
            if let Some(ui) = ui_handle.upgrade() {
                let size_mb = size_mb.clamp(0.1, 50000.0);
                ui.set_target_size_mb(size_mb);
                ui.set_target_size_str(format!("{:.1}", size_mb).into());
                let duration =
                    (ui.get_out_point_secs() - ui.get_in_point_secs()).max(0.0001) as f64;
                let bitrate = calculate_target_bitrate_kbps(duration, size_mb as f64);
                ui.set_estimated_bitrate_str(format!("{} kbps", bitrate).into());
            }
        });
    }

    // 13. Target Size Custom Input Edited
    {
        let ui_handle = main_window.as_weak();
        main_window.on_target_size_edited(move |text| {
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_target_size_str(text.clone());
                let trimmed = text.trim();
                if let Ok(val) = trimmed.parse::<f64>()
                    && val.is_finite()
                    && val > 0.0
                    && val <= 50000.0
                {
                    ui.set_target_size_mb(val as f32);
                    let duration =
                        (ui.get_out_point_secs() - ui.get_in_point_secs()).max(0.0001) as f64;
                    let bitrate = calculate_target_bitrate_kbps(duration, val);
                    ui.set_estimated_bitrate_str(format!("{} kbps", bitrate).into());
                }
            }
        });
    }

    // 14. Start Export
    {
        let ui_handle = main_window.as_weak();
        let path_ref = Arc::clone(&loaded_video_path);
        let meta_ref = Arc::clone(&loaded_metadata);
        let export_ref = active_export.clone();
        let finished = export_finished.clone();
        main_window.on_start_export(move || {
            let input_path = match path_ref.lock().ok().and_then(|g| g.clone()) {
                Some(p) => p,
                None => return,
            };
            let metadata = match meta_ref.lock().ok().and_then(|g| g.clone()) {
                Some(m) => m,
                None => return,
            };

            let ui = match ui_handle.upgrade() {
                Some(u) => u,
                None => return,
            };

            let mut in_pt = ui.get_in_point_secs() as f64;
            let mut out_pt = ui.get_out_point_secs() as f64;
            let mode_idx = ui.get_export_mode_idx();
            if mode_idx == 2 {
                if metadata.keyframes.is_empty() {
                    return;
                }
                (in_pt, out_pt) = media::probe::copy_range(
                    &metadata.keyframes,
                    in_pt,
                    out_pt,
                    metadata.duration_secs,
                );
            }
            if ui.get_is_exporting() {
                return;
            }
            if mode_idx == 0
                && ui
                    .get_target_size_str()
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && *n > 0.0 && *n <= 50000.0)
                    .is_none()
            {
                ui.set_export_status_text(
                    "Enter a valid target size between 0.1 and 50,000 MiB.".into(),
                );
                return;
            }

            // Default suggested output name: input_cut.mp4
            let stem = input_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("video");
            let extension = if mode_idx != 0 { "mkv" } else { "mp4" };
            let default_name = format!("{stem}_cut.{extension}");

            let save_dest = rfd::FileDialog::new()
                .set_title("Save Cut Video")
                .set_file_name(&default_name)
                .add_filter("Video", &[extension])
                .save_file();

            let output_path = match save_dest {
                Some(p) => p,
                None => return, // User canceled file picker
            };

            let profile = if mode_idx == 2 {
                ExportProfile::LosslessCopy
            } else if mode_idx == 1 {
                ExportProfile::LosslessEncode
            } else {
                let target_mb = ui.get_target_size_mb() as f64;
                let codec_idx = ui.get_codec_idx();
                let preset_idx = ui.get_preset_idx();

                let (codec, preset) = match (codec_idx, preset_idx) {
                    (0, 0) => (ExportCodec::HevcNvenc10Bit, "p2".to_string()),
                    (0, 1) => (ExportCodec::HevcNvenc10Bit, "p4".to_string()),
                    (0, 2) => (ExportCodec::HevcNvenc10Bit, "p6".to_string()),
                    (1, 0) => (ExportCodec::Libx265_10Bit, "fast".to_string()),
                    (1, 1) => (ExportCodec::Libx265_10Bit, "medium".to_string()),
                    (1, 2) => (ExportCodec::Libx265_10Bit, "slow".to_string()),
                    (2, 0) => (ExportCodec::H264Nvenc, "p2".to_string()),
                    (2, 1) => (ExportCodec::H264Nvenc, "p4".to_string()),
                    (2, 2) => (ExportCodec::H264Nvenc, "p6".to_string()),
                    (3, 0) => (ExportCodec::Libx264, "fast".to_string()),
                    (3, 2) => (ExportCodec::Libx264, "slow".to_string()),
                    _ => (ExportCodec::Libx264, "medium".to_string()),
                };

                ExportProfile::TargetSize {
                    target_mb,
                    codec,
                    fps: match ui.get_fps_idx() {
                        1 => 24.0,
                        2 => 30.0,
                        3 => 60.0,
                        _ => metadata.fps,
                    },
                    preset,
                }
            };

            let job = ExportJob {
                input_path,
                output_path,
                in_point_secs: in_pt,
                out_point_secs: out_pt,
                profile,
            };

            ui.set_is_exporting(true);
            ui.set_export_status_text("Starting export...".into());

            let ui_status = ui_handle.clone();
            let ui_done = ui_handle.clone();
            finished.store(false, Ordering::Relaxed);
            let finished_job = finished.clone();

            let cancel = execute_export(
                job,
                move |status| {
                    let ui = ui_status.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(u) = ui.upgrade() {
                            u.set_export_status_text(status.into());
                        }
                    });
                },
                move |result| {
                    finished_job.store(true, Ordering::Relaxed);
                    let ui = ui_done.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(u) = ui.upgrade() {
                            u.set_is_exporting(false);
                            match result {
                                Ok(out_p) => {
                                    u.set_show_export_modal(false);
                                    let filename =
                                        out_p.file_name().unwrap_or_default().to_string_lossy();
                                    u.set_status_message(
                                        format!("Export saved successfully: {}", filename).into(),
                                    );
                                }
                                Err(err) => {
                                    u.set_export_status_text(err.clone().into());
                                    u.set_status_message(format!("Export error: {}", err).into());
                                }
                            }
                        }
                    });
                },
            );
            if let Ok(mut guard) = export_ref.lock() {
                *guard = Some(cancel);
            }
        });
    }

    // 14. Close Video Callback
    {
        let ui_handle = main_window.as_weak();
        let player_ref = Arc::clone(&active_player);
        let path_ref = Arc::clone(&loaded_video_path);
        let meta_ref = Arc::clone(&loaded_metadata);

        main_window.on_clear_file(move || {
            if let Ok(mut guard) = player_ref.lock()
                && let Some(player) = guard.take()
            {
                player.stop();
            }
            if let Ok(mut guard) = path_ref.lock() {
                *guard = None;
            }
            if let Ok(mut guard) = meta_ref.lock() {
                *guard = None;
            }

            if let Some(ui) = ui_handle.upgrade() {
                reset_media_ui(&ui);
            }
        });
    }

    let _editor = editor::install(
        &main_window,
        active_player.clone(),
        active_export.clone(),
        export_finished.clone(),
    );
    if let Some(path) = std::env::args().nth(1)
        && !path.starts_with("--")
    {
        main_window.invoke_import_path(path.into());
    }
    println!("Starting Feathercut...");
    main_window.run()?;
    window_glass::shutdown();
    // Closing immediately after moving a slider must not lose the debounced change.
    let appearance = settings::Appearance {
        transparency: main_window.get_transparency(),
        blur: main_window.get_blur_amount(),
        theme: main_window.get_theme_idx().clamp(0, 1) as u8,
    };
    if let Err(error) = appearance.save(&settings::Appearance::path()) {
        eprintln!("Could not save appearance on exit: {error}");
    }

    // Cleanup on exit
    if let Ok(guard) = active_export.lock()
        && let Some(flag) = &*guard
    {
        flag.store(true, Ordering::Relaxed);
    }
    for _ in 0..30 {
        if export_finished.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if let Ok(mut guard) = active_player.lock()
        && let Some(player) = guard.take()
    {
        player.stop();
    }

    Ok(())
}
