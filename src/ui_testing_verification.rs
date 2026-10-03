//! Offscreen verification: no winit backend, native window, or desktop interaction.
use super::*;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use std::rc::Rc;

struct Offscreen(Rc<MinimalSoftwareWindow>);
impl slint::platform::Platform for Offscreen {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
fn time_entry_accepts_timecodes_and_rejects_invalid_values() {
    assert_eq!(parse_trim_time("02:03.5"), Some(123.5));
    assert_eq!(parse_trim_time("8.25"), Some(8.25));
    for text in ["NaN", "inf", "-1", "00:60", "1:2:3", "", "00:-1"] {
        assert_eq!(parse_trim_time(text), None, "{text}");
    }
    assert_eq!(trim_time_text(59.96), "01:00.0");
}

fn render(window: &MinimalSoftwareWindow, name: &str, width: u32, height: u32) {
    window.set_size(slint::PhysicalSize::new(width, height));
    slint::platform::update_timers_and_animations();
    // Settle transitions without creating a native window.
    std::thread::sleep(std::time::Duration::from_millis(240));
    slint::platform::update_timers_and_animations();
    window.request_redraw();
    // Native transparent windows use premultiplied RGBA. Match that path here
    // and composite over a neutral synthetic backdrop for the saved preview.
    use slint::platform::software_renderer::PremultipliedRgbaColor;
    let mut rgba = vec![PremultipliedRgbaColor::default(); (width * height) as usize];
    assert!(window.draw_if_needed(|renderer| {
        renderer.render(&mut rgba, width as usize);
    }));
    let mut pixels = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(width, height);
    for (target, source) in pixels.make_mut_slice().iter_mut().zip(rgba) {
        let backdrop = (28u16 * (255 - u16::from(source.alpha)) / 255) as u8;
        *target = slint::Rgb8Pixel {
            r: source.red.saturating_add(backdrop),
            g: source.green.saturating_add(backdrop),
            b: source.blue.saturating_add(backdrop),
        };
    }
    let path = PathBuf::from("target/testing-ui-verification");
    std::fs::create_dir_all(&path).unwrap();
    let mut encoder = png::Encoder::new(
        std::fs::File::create(path.join(format!("{name}.png"))).unwrap(),
        width,
        height,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(pixels.as_bytes())
        .unwrap();
}

#[test]
fn render_all_editor_states_without_a_desktop_window() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen(window.clone()))).unwrap();
    let ui = MainWindow::new().unwrap();
    let weak = ui.as_weak();
    ui.on_refresh_trim_labels(move || {
        if let Some(ui) = weak.upgrade() {
            refresh_trim_labels(&ui);
        }
    });
    let plays = Rc::new(std::cell::Cell::new(0));
    let play_calls = plays.clone();
    ui.on_toggle_play(move || play_calls.set(play_calls.get() + 1));
    let weak = ui.as_weak();
    ui.on_close_export_modal(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_show_export_modal(false);
        }
    });
    let weak = ui.as_weak();
    ui.on_set_volume(move |value| {
        if let Some(ui) = weak.upgrade() {
            ui.set_volume(value.clamp(0.0, 100.0));
        }
    });
    // This adapter's show operation is entirely offscreen.
    ui.show().unwrap();
    render(&window, "import", 1060, 720);
    assert!(ui.get_import_visible() && !ui.get_editor_visible());
    ui.set_drop_hovered(true);
    render(&window, "import-drop", 900, 660);
    ui.set_is_loading(true);
    render(&window, "import-loading", 900, 660);
    ui.set_is_loading(false);
    ui.set_drop_hovered(false);
    ui.set_has_video(true);
    ui.set_file_name("lake-morning.mp4".into());
    ui.set_video_info_str("1280 × 720 · 30 fps · 146 KB".into());
    ui.set_status_message("Video loaded".into());
    ui.set_duration_secs(10.0);
    ui.set_in_point_secs(2.0);
    ui.set_out_point_secs(8.0);
    ui.set_current_time_secs(3.2);
    ui.set_timecode_display("00:03.2 / 00:10.0".into());
    ui.set_trim_summary_str("00:02.0 – 00:08.0 · 6.0s".into());
    // A deterministic landscape fixture checks preview fitting without files,
    // a decoder, a native window, or sampling the user's desktop.
    let mut frame = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(640, 360);
    for (i, pixel) in frame.make_mut_slice().iter_mut().enumerate() {
        let x = i % 640;
        let y = i / 640;
        let mountain = 110 + x.abs_diff(330) / 4 + (x % 90).abs_diff(45) / 2;
        let valley = 145 + x.abs_diff(330) / 5;
        let rgb = if y > 250 {
            [31 + (y % 9) as u8, 54 + (y % 7) as u8, 75 + (y % 8) as u8]
        } else if y > valley {
            [25, 43, 58]
        } else if y > mountain {
            [55, 64, 92]
        } else {
            [80 + (y / 6) as u8, 90 + (y / 5) as u8, 133 + (y / 7) as u8]
        };
        *pixel = slint::Rgb8Pixel {
            r: rgb[0],
            g: rgb[1],
            b: rgb[2],
        };
    }
    let frame = Image::from_rgb8(frame);
    ui.set_video_frame(frame.clone());
    ui.set_thumbnails(Rc::new(slint::VecModel::from(vec![frame; 8])).into());
    render(&window, "work", 1060, 720);
    assert!(!ui.get_import_visible() && ui.get_editor_visible());
    ui.set_drop_hovered(true);
    render(&window, "work-drop", 900, 660);
    ui.set_drop_hovered(false);
    ui.set_theme_idx(1);
    render(&window, "work-monochrome", 1060, 720);
    ui.set_show_export_modal(true);
    render(&window, "export-monochrome", 1060, 720);
    ui.set_show_export_modal(false);
    ui.set_theme_idx(0);
    render(&window, "work", 1060, 720);
    assert_eq!(ui.get_in_time_text(), "00:02.0");
    assert_eq!(ui.get_out_time_text(), "00:08.0");
    window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: " ".into() });
    window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: " ".into() });
    assert_eq!(plays.get(), 1, "workspace keyboard shortcut must work");
    ui.set_show_export_modal(true);
    render(&window, "export", 1060, 720);
    window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: " ".into() });
    window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: " ".into() });
    assert_eq!(
        plays.get(),
        1,
        "export must isolate workspace keyboard shortcuts"
    );
    render(&window, "export-small", 900, 660);
    render(&window, "export-tall", 1060, 900);
    ui.set_export_mode_idx(1);
    render(&window, "lossless", 900, 660);
    ui.set_export_mode_idx(2);
    render(&window, "copy", 900, 660);
    window.dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    window.dispatch_event(slint::platform::WindowEvent::KeyReleased {
        text: slint::platform::Key::Escape.into(),
    });
    assert!(!ui.get_show_export_modal(), "Escape must close the overlay");
    render(&window, "work-small", 900, 660);
    window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: " ".into() });
    window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: " ".into() });
    assert_eq!(
        plays.get(),
        2,
        "closing export must restore workspace shortcuts"
    );
    ui.set_has_video(false);
    ui.set_in_point_secs(0.0);
    ui.set_out_point_secs(1.0);
    ui.set_duration_secs(1.0);
    ui.set_status_message("Ready".into());
    render(&window, "import-small", 900, 660);
    ui.set_show_settings(true);
    render(&window, "settings", 900, 660);
    ui.set_theme_idx(1);
    render(&window, "settings-monochrome", 900, 660);
    let mut monochrome = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(900, 660);
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(monochrome.make_mut_slice(), 900);
    });
    // Branding retains the user's supplied lilac icon in the titlebar; validate
    // the monochrome settings surfaces and controls independently of that asset.
    for pixel in monochrome
        .as_slice()
        .chunks_exact(900)
        .skip(100)
        .take(460)
        .flat_map(|row| &row[232..668])
    {
        assert!(
            pixel.r.abs_diff(pixel.g) <= 1 && pixel.g.abs_diff(pixel.b) <= 1,
            "Monochrome settings must have neutral surfaces and controls"
        );
    }
    let mut darkest = 255u8;
    let mut lightest = 0u8;
    for y in 239..253 {
        for x in 495..583 {
            let level = monochrome.as_slice()[y * 900 + x].r;
            darkest = darkest.min(level);
            lightest = lightest.max(level);
        }
    }
    assert!(
        darkest < 35 && lightest > 180,
        "Monochrome selected controls must have dark text against a bright silver fill"
    );
    ui.set_theme_idx(0);
    let appearance_calls = Rc::new(std::cell::Cell::new(0));
    let changed = appearance_calls.clone();
    ui.on_appearance_changed(move || changed.set(changed.get() + 1));
    let position = slint::LogicalPosition::new(352.0, 318.0);
    window.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    window.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    assert!(
        (ui.get_transparency() - 25.0).abs() < 2.0,
        "Transparency slider clicks must track the pointer from the left edge"
    );
    window.dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: slint::platform::Key::End.into(),
    });
    window.dispatch_event(slint::platform::WindowEvent::KeyReleased {
        text: slint::platform::Key::End.into(),
    });
    assert_eq!(
        ui.get_transparency(),
        100.0,
        "Slider keyboard control must work"
    );
    assert!(
        appearance_calls.get() >= 2,
        "Slider changes must request persistence"
    );
    ui.set_transparency(55.0);
    window.dispatch_event(slint::platform::WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    window.dispatch_event(slint::platform::WindowEvent::KeyReleased {
        text: slint::platform::Key::Escape.into(),
    });
    assert!(!ui.get_show_settings(), "Escape must close Settings");
    ui.set_transparency(0.0);
    render(&window, "solid", 900, 660);
    // Verify that the actual client pixels change alpha, not just color.
    use slint::platform::software_renderer::PremultipliedRgbaColor;
    let mut pixels = vec![PremultipliedRgbaColor::default(); 900 * 660];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 900);
    });
    assert_eq!(
        pixels[20 * 900 + 500].alpha,
        255,
        "solid mode must be opaque"
    );
    ui.set_transparency(100.0);
    render(&window, "translucent", 900, 660);
    pixels.fill(PremultipliedRgbaColor::default());
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 900);
    });
    assert!(
        pixels[20 * 900 + 500].alpha < 200,
        "translucent mode must preserve desktop visibility"
    );

    // Uniform thumbnails make misplaced excluded-region shading measurable.
    ui.set_has_video(true);
    ui.set_duration_secs(10.0);
    ui.set_in_point_secs(2.0);
    ui.set_out_point_secs(8.0);
    let mut white = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(16, 16);
    white.make_mut_bytes().fill(220);
    let white = Image::from_rgb8(white);
    ui.set_thumbnails(Rc::new(slint::VecModel::from(vec![white; 8])).into());
    ui.set_volume(25.0);
    render(&window, "anchoring", 1060, 720);
    let capture = || {
        let mut pixels = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(1060, 720);
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(pixels.make_mut_slice(), 1060);
        });
        pixels
    };
    let first = capture();
    let pixel = |frame: &slint::SharedPixelBuffer<slint::Rgb8Pixel>, x: usize, y: usize| {
        frame.as_slice()[y * 1060 + x]
    };
    let timeline_left = ui.get_timeline_left() as usize;
    let timeline_top = ui.get_timeline_top() as usize;
    let timeline_width = ui.get_timeline_width() as usize;
    assert!(
        pixel(
            &first,
            timeline_left + timeline_width / 10,
            timeline_top + 32
        )
        .r < pixel(
            &first,
            timeline_left + timeline_width / 2,
            timeline_top + 32
        )
        .r,
        "left excluded region must be dimmed at the left edge"
    );
    assert!(
        pixel(
            &first,
            timeline_left + timeline_width * 9 / 10,
            timeline_top + 32
        )
        .r < pixel(
            &first,
            timeline_left + timeline_width / 2,
            timeline_top + 32
        )
        .r,
        "right excluded region must be dimmed at the right edge"
    );
    let fill_positions = |frame: &slint::SharedPixelBuffer<slint::Rgb8Pixel>| {
        let left = ui.get_volume_left() as usize;
        let top = ui.get_volume_top() as usize;
        let width = ui.get_volume_width() as usize;
        (left..left + width)
            .filter(|&x| {
                (top..top + 30).any(|y| {
                    let p = pixel(frame, x, y);
                    p.r == 183 && p.g == 148 && p.b == 235
                })
            })
            .collect::<Vec<_>>()
    };
    let low = fill_positions(&first);
    ui.set_volume(75.0);
    let high = fill_positions(&capture());
    assert!(
        !low.is_empty() && !high.is_empty(),
        "volume fill must render"
    );
    assert_eq!(
        low.first(),
        high.first(),
        "volume fill must stay anchored left as its width changes"
    );
    assert!(
        high.len() > low.len() * 2,
        "volume fill must grow toward the thumb"
    );
    let position = slint::LogicalPosition::new(
        ui.get_volume_left() + ui.get_volume_width() * 0.25,
        ui.get_volume_top() + 15.0,
    );
    window.dispatch_event(slint::platform::WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    window.dispatch_event(slint::platform::WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    assert!(
        (ui.get_volume() - 25.0).abs() < 1.0,
        "The relocated volume slider must follow the pointer"
    );
    let before_play = plays.get();
    window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: " ".into() });
    window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: " ".into() });
    assert_eq!(
        plays.get(),
        before_play + 1,
        "Space must still play when the volume slider has focus"
    );
    for settings in [true, false] {
        // Repeat modal openings after the volume control owns focus.
        for _ in 0..2 {
            if settings {
                ui.set_show_settings(true);
            } else {
                ui.set_show_export_modal(true);
            }
            slint::platform::update_timers_and_animations();
            let before = plays.get();
            window.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: " ".into() });
            window.dispatch_event(slint::platform::WindowEvent::KeyReleased { text: " ".into() });
            assert_eq!(
                plays.get(),
                before,
                "Modal shortcuts must remain isolated on repeated opens"
            );
            window.dispatch_event(slint::platform::WindowEvent::KeyPressed {
                text: slint::platform::Key::Escape.into(),
            });
            window.dispatch_event(slint::platform::WindowEvent::KeyReleased {
                text: slint::platform::Key::Escape.into(),
            });
            assert!(!ui.get_show_settings() && !ui.get_show_export_modal());
        }
    }
    // Functional fields retain their own backing even at maximum shell transparency.
    let mut controls = vec![PremultipliedRgbaColor::default(); 1060 * 720];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut controls, 1060);
    });
    assert!(
        controls[195 * 1060 + 900].alpha > 240,
        "Trim input contrast must not depend on shell transparency"
    );
    // Exercise the real close reset after populated thumbnails/playback state.
    ui.set_is_playing(true);
    ui.set_is_scrubbing(true);
    ui.set_keyframes_ready(true);
    let generation = ui.get_media_generation();
    reset_media_ui(&ui);
    use slint::Model;
    assert_eq!(
        ui.get_thumbnails().row_count(),
        0,
        "Closing must release every timeline thumbnail"
    );
    assert_eq!(
        ui.get_video_frame().size().width,
        0,
        "Closing must release the preview frame"
    );
    assert!(!ui.get_has_video() && !ui.get_is_playing() && !ui.get_is_scrubbing());
    assert!(!ui.get_keyframes_ready());
    assert_ne!(
        ui.get_media_generation(),
        generation,
        "Closing must reject late thumbnail updates"
    );
    render(&window, "closed-video", 1060, 720);
    assert!(
        ui.get_import_visible() && !ui.get_editor_visible(),
        "Closing must return to the uncluttered import state"
    );
}
