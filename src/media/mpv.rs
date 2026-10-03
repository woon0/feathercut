//! Small bindings to the stable libmpv client/render ABI. The bundled headers
//! in runtime/mpv/include/mpv define the C layouts and function contracts.
use libloading::Library;
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

type Handle = *mut c_void;
#[repr(C)]
struct Param {
    kind: c_int,
    data: *mut c_void,
}
#[repr(C)]
pub struct Event {
    pub id: c_int,
    pub error: c_int,
    pub userdata: u64,
    pub data: *mut c_void,
}
#[repr(C)]
struct EndFile {
    reason: c_int,
    error: c_int,
    playlist_entry_id: i64,
    playlist_insert_id: i64,
    playlist_insert_num_entries: c_int,
}

struct Api {
    create: unsafe extern "C" fn() -> Handle,
    initialize: unsafe extern "C" fn(Handle) -> c_int,
    destroy: unsafe extern "C" fn(Handle),
    option: unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int,
    command: unsafe extern "C" fn(Handle, u64, *const *const c_char) -> c_int,
    get: unsafe extern "C" fn(Handle, *const c_char, c_int, *mut c_void) -> c_int,
    wait: unsafe extern "C" fn(Handle, f64) -> *mut Event,
    error: unsafe extern "C" fn(c_int) -> *const c_char,
    render_create: unsafe extern "C" fn(*mut Handle, Handle, *mut Param) -> c_int,
    render_callback:
        unsafe extern "C" fn(Handle, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void),
    render_update: unsafe extern "C" fn(Handle) -> u64,
    render: unsafe extern "C" fn(Handle, *mut Param) -> c_int,
    render_swap: unsafe extern "C" fn(Handle),
    render_free: unsafe extern "C" fn(Handle),
    _library: Library,
}
impl Api {
    fn load() -> Result<Arc<Self>, String> {
        let mut paths = Vec::new();
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            paths.push(dir.join("libmpv-2.dll"));
            if let Some(parent) = dir.parent() {
                paths.push(parent.join("libmpv-2.dll"));
            }
        }
        paths.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/mpv/libmpv-2.dll"));
        paths.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("_archive/runtime/mpv/libmpv-2.dll"));
        let path = paths
            .into_iter()
            .find(|p| p.is_file())
            .ok_or("Playback runtime is missing. Place libmpv-2.dll beside the executable or in runtime/mpv/.")?;
        // SAFETY: load only our explicitly located, checksum-verified runtime;
        // symbols use the signatures in the supplied mpv client/render headers.
        unsafe {
            let library = Library::new(path).map_err(|e| e.to_string())?;
            macro_rules! symbol {
                ($name:literal) => {
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|e| e.to_string())?
                };
            }
            let api = Self {
                create: symbol!("mpv_create"),
                initialize: symbol!("mpv_initialize"),
                destroy: symbol!("mpv_terminate_destroy"),
                option: symbol!("mpv_set_option_string"),
                command: symbol!("mpv_command_async"),
                get: symbol!("mpv_get_property"),
                wait: symbol!("mpv_wait_event"),
                error: symbol!("mpv_error_string"),
                render_create: symbol!("mpv_render_context_create"),
                render_callback: symbol!("mpv_render_context_set_update_callback"),
                render_update: symbol!("mpv_render_context_update"),
                render: symbol!("mpv_render_context_render"),
                render_swap: symbol!("mpv_render_context_report_swap"),
                render_free: symbol!("mpv_render_context_free"),
                _library: library,
            };
            Ok(Arc::new(api))
        }
    }
    fn check(&self, status: c_int) -> Result<(), String> {
        if status >= 0 {
            Ok(())
        } else {
            // SAFETY: mpv_error_string returns a static NUL-terminated string.
            Err(unsafe { CStr::from_ptr((self.error)(status)) }
                .to_string_lossy()
                .into_owned())
        }
    }
}
struct Signal {
    ready: Mutex<bool>,
    wake: Condvar,
    stop: AtomicBool,
}
unsafe extern "C" fn render_ready(data: *mut c_void) {
    // SAFETY: points to a Box<Arc<Signal>> held alive until the callback is removed.
    let signal = unsafe { &*(data as *const Arc<Signal>) };
    if let Ok(mut ready) = signal.ready.lock() {
        *ready = true;
        signal.wake.notify_one();
    }
}

pub struct Engine {
    api: Arc<Api>,
    handle: Handle,
    render_thread: Option<JoinHandle<()>>,
    signal: Arc<Signal>,
    position: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
    seeking: Arc<AtomicBool>,
}
impl Engine {
    pub fn audio(path: &Path) -> Result<Self, String> {
        // A tiny cached PCM silence source represents gaps. Create it only on
        // the audio worker, never during startup or a timeline edit.
        let silence = crate::timeline::silence_path();
        if !std::fs::metadata(&silence).is_ok_and(|m| m.len() == 320044) {
            let mut wav = Vec::with_capacity(320044);
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&320036u32.to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&8000u32.to_le_bytes());
            wav.extend_from_slice(&32000u32.to_le_bytes());
            wav.extend_from_slice(&4u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&320000u32.to_le_bytes());
            wav.resize(320044, 0);
            std::fs::write(silence, wav)
                .map_err(|e| format!("Could not prepare silent audio gaps: {e}"))?;
        }
        let api = Api::load()?;
        let handle = unsafe { (api.create)() };
        if handle.is_null() {
            return Err("Could not create the audio engine.".into());
        }
        let init = (|| {
            for (name, value) in [
                ("config", "no"),
                ("terminal", "no"),
                ("vo", "null"),
                ("vid", "no"),
                ("pause", "yes"),
                ("idle", "yes"),
                ("keep-open", "yes"),
                ("volume-max", "200"),
                ("audio-file-auto", "no"),
                ("sub-auto", "no"),
            ] {
                let name = CString::new(name).unwrap();
                let value = CString::new(value).unwrap();
                api.check(unsafe { (api.option)(handle, name.as_ptr(), value.as_ptr()) })?;
            }
            #[cfg(test)]
            api.check(unsafe { (api.option)(handle, c"ao".as_ptr(), c"null".as_ptr()) })?;
            api.check(unsafe { (api.initialize)(handle) })
        })();
        if let Err(e) = init {
            unsafe { (api.destroy)(handle) };
            return Err(e);
        }
        let engine = Self {
            api,
            handle,
            render_thread: None,
            signal: Arc::new(Signal {
                ready: Mutex::new(false),
                wake: Condvar::new(),
                stop: AtomicBool::new(false),
            }),
            position: Arc::new(AtomicU64::new(0f64.to_bits())),
            playing: Arc::new(AtomicBool::new(false)),
            seeking: Arc::new(AtomicBool::new(false)),
        };
        engine.command(&["loadfile", path.to_str().ok_or("Invalid audio path.")?])?;
        Ok(engine)
    }
    pub fn new(
        path: &Path,
        size: (u32, u32),
        on_frame: Arc<impl Fn(SharedPixelBuffer<Rgba8Pixel>, f64, bool) + Send + Sync + 'static>,
        on_error: Arc<impl Fn(String) + Send + Sync + 'static>,
    ) -> Result<Self, String> {
        let api = Api::load()?;
        // SAFETY: the control handle is created, used, and destroyed on this thread.
        let handle = unsafe { (api.create)() };
        if handle.is_null() {
            return Err("Could not create the playback engine.".into());
        }
        let initialize = || -> Result<(), String> {
            for (name, value) in [
                ("config", "no"),
                ("terminal", "no"),
                ("vo", "libmpv"),
                ("pause", "yes"),
                ("keep-open", "yes"),
                ("idle", "yes"),
                ("osc", "no"),
                ("osd-level", "0"),
                ("input-default-bindings", "no"),
                ("input-vo-keyboard", "no"),
                ("sid", "no"),
                ("sws-fast", "yes"),
                ("hwdec", "auto-copy"),
                ("volume", "80"),
            ] {
                let name = CString::new(name).unwrap();
                let value = CString::new(value).unwrap();
                api.check(unsafe { (api.option)(handle, name.as_ptr(), value.as_ptr()) })
                    .map_err(|error| {
                        format!("Playback option {}: {error}", name.to_string_lossy())
                    })?;
            }
            #[cfg(test)]
            {
                // Exercise the audio clock/decoder without emitting test tones.
                api.check(unsafe { (api.option)(handle, c"ao".as_ptr(), c"null".as_ptr()) })?;
                api.check(unsafe { (api.option)(handle, c"hwdec".as_ptr(), c"no".as_ptr()) })?;
            }
            api.check(unsafe { (api.initialize)(handle) })
        };
        if let Err(error) = initialize() {
            unsafe { (api.destroy)(handle) };
            return Err(error);
        }
        let mut context = std::ptr::null_mut();
        let mut advanced = 1;
        let mut create_params = [
            Param {
                kind: 1,
                data: c"sw".as_ptr() as *mut c_void,
            },
            Param {
                kind: 10,
                data: (&mut advanced as *mut c_int).cast(),
            },
            Param {
                kind: 0,
                data: std::ptr::null_mut(),
            },
        ];
        if let Err(error) = api
            .check(unsafe { (api.render_create)(&mut context, handle, create_params.as_mut_ptr()) })
        {
            unsafe { (api.destroy)(handle) };
            return Err(error);
        }
        let signal = Arc::new(Signal {
            ready: Mutex::new(false),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let callback_data = Box::into_raw(Box::new(signal.clone()));
        unsafe { (api.render_callback)(context, Some(render_ready), callback_data as *mut c_void) };
        let position = Arc::new(AtomicU64::new(0f64.to_bits()));
        let playing = Arc::new(AtomicBool::new(false));
        let seeking = Arc::new(AtomicBool::new(false));
        let render_api = api.clone();
        let render_signal = signal.clone();
        let render_position = position.clone();
        let render_playing = playing.clone();
        let render_seeking = seeking.clone();
        // Transfer only the opaque renderer and callback pointer to its dedicated
        // thread. That thread never calls client/command/property functions.
        let context_address = context as usize;
        let callback_address = callback_data as usize;
        let render_thread = thread::spawn(move || {
            let context = context_address as Handle;
            let (width, height) = size;
            let mut surface_size = [width as c_int, height as c_int];
            let mut stride = width as usize * 4;
            loop {
                let Ok(ready) = render_signal.ready.lock() else {
                    break;
                };
                let Ok((mut ready, _)) = render_signal.wake.wait_timeout_while(
                    ready,
                    Duration::from_millis(100),
                    |ready| !*ready && !render_signal.stop.load(Ordering::Relaxed),
                ) else {
                    break;
                };
                if render_signal.stop.load(Ordering::Relaxed) {
                    break;
                }
                if !*ready {
                    continue;
                }
                *ready = false;
                drop(ready);
                if unsafe { (render_api.render_update)(context) } & 1 == 0 {
                    continue;
                }
                let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
                let mut params = [
                    Param {
                        kind: 17,
                        data: surface_size.as_mut_ptr().cast(),
                    },
                    Param {
                        kind: 18,
                        data: c"rgb0".as_ptr() as *mut c_void,
                    },
                    Param {
                        kind: 19,
                        data: (&mut stride as *mut usize).cast(),
                    },
                    Param {
                        kind: 20,
                        data: pixels.make_mut_bytes().as_mut_ptr().cast(),
                    },
                    Param {
                        kind: 0,
                        data: std::ptr::null_mut(),
                    },
                ];
                let status = unsafe { (render_api.render)(context, params.as_mut_ptr()) };
                if let Err(error) = render_api.check(status) {
                    on_error(format!("Preview rendering failed: {error}"));
                    break;
                }
                unsafe { (render_api.render_swap)(context) };
                // rgb0's fourth byte is undefined; Slint expects opaque RGBA.
                for pixel in pixels.make_mut_bytes().chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                {
                    on_frame(
                        pixels,
                        f64::from_bits(render_position.load(Ordering::Relaxed)),
                        render_playing.load(Ordering::Relaxed)
                            && !render_seeking.load(Ordering::Relaxed),
                    );
                }
            }
            // SAFETY: remove the callback, free its payload and the renderer,
            // then join this thread before destroying the control handle.
            unsafe {
                (render_api.render_callback)(context, None, std::ptr::null_mut());
                (render_api.render_free)(context);
                drop(Box::from_raw(callback_address as *mut Arc<Signal>));
            }
        });
        let engine = Self {
            api,
            handle,
            render_thread: Some(render_thread),
            signal,
            position,
            playing,
            seeking,
        };
        let source = path
            .to_str()
            .ok_or("The playback path is not valid Unicode.")?;
        engine.command(&["loadfile", source])?;
        Ok(engine)
    }
    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        let strings: Vec<CString> = args
            .iter()
            .map(|s| CString::new(*s).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        let mut pointers: Vec<*const c_char> = strings.iter().map(|s| s.as_ptr()).collect();
        pointers.push(std::ptr::null());
        // mpv copies the command arguments before this asynchronous call returns.
        self.api
            .check(unsafe { (self.api.command)(self.handle, 0, pointers.as_ptr()) })
    }
    pub fn number(&self, name: &CStr) -> Option<f64> {
        let mut value = 0.0f64;
        let status = unsafe {
            (self.api.get)(
                self.handle,
                name.as_ptr(),
                5,
                (&mut value as *mut f64).cast(),
            )
        };
        (status >= 0 && value.is_finite()).then_some(value)
    }
    pub fn flag(&self, name: &CStr) -> bool {
        let mut value = 0;
        let status = unsafe {
            (self.api.get)(
                self.handle,
                name.as_ptr(),
                3,
                (&mut value as *mut c_int).cast(),
            )
        };
        status >= 0 && value != 0
    }
    pub fn state(&self, playing: bool, position: f64) {
        self.playing.store(playing, Ordering::Relaxed);
        self.position.store(position.to_bits(), Ordering::Relaxed);
    }
    pub fn seek(&self, seconds: f64) -> Result<(), String> {
        self.seeking.store(true, Ordering::Relaxed);
        self.position.store(seconds.to_bits(), Ordering::Relaxed);
        self.command(&["seek", &format!("{seconds:.9}"), "absolute+exact"])
    }
    pub fn seek_finished(&self) {
        self.seeking.store(false, Ordering::Relaxed);
    }
    pub fn event(&self) -> Option<(c_int, Option<String>)> {
        // Event memory is owned by mpv and remains valid until the next wait.
        let event = unsafe { &*(self.api.wait)(self.handle, 0.0) };
        if event.id == 0 {
            return None;
        }
        let error = if event.error < 0 {
            self.api.check(event.error).err()
        } else if event.id == 7 && !event.data.is_null() {
            let end = unsafe { &*(event.data as *const EndFile) };
            (end.reason == 4)
                .then(|| self.api.check(end.error).err())
                .flatten()
        } else {
            None
        };
        Some((event.id, error))
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.signal.stop.store(true, Ordering::Relaxed);
        self.signal.wake.notify_all();
        if let Some(thread) = self.render_thread.take() {
            let _ = thread.join();
        }
        unsafe { (self.api.destroy)(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    #[test]
    #[ignore = "requires bundled libmpv and FFmpeg"]
    fn persistent_engine_decodes_audio_seeks_and_steps_source_frames() {
        let path = crate::media::test_fixture();
        let frames = Arc::new(AtomicU64::new(0));
        let observed = frames.clone();
        let errors = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed_errors = errors.clone();
        let engine = Engine::new(
            &path,
            (320, 180),
            Arc::new(move |pixels: SharedPixelBuffer<Rgba8Pixel>, _, _| {
                assert_eq!(pixels.width(), 320);
                assert!(pixels.as_bytes().chunks_exact(4).all(|p| p[3] == 255));
                observed.fetch_add(1, Ordering::Relaxed);
            }),
            Arc::new(move |error| {
                observed_errors.lock().unwrap().push(error);
            }),
        )
        .unwrap();
        let pump = |duration: Duration| {
            let until = Instant::now() + duration;
            while Instant::now() < until {
                while let Some((event, error)) = engine.event() {
                    assert!(error.is_none(), "native playback error: {error:?}");
                    if event == 21 {
                        engine.seek_finished();
                    }
                }
                if let Some(position) = engine.number(c"time-pos") {
                    engine.state(!engine.flag(c"pause"), position);
                }
                thread::sleep(Duration::from_millis(10));
            }
        };
        pump(Duration::from_millis(2000));
        assert!(
            frames.load(Ordering::Relaxed) > 0,
            "paused import did not render; errors: {:?}, position: {:?}, video: {:?}",
            errors.lock().unwrap(),
            engine.number(c"time-pos"),
            engine.number(c"vid")
        );
        assert_eq!(engine.number(c"audio-params/samplerate"), Some(44100.0));
        engine.command(&["set", "pause", "no"]).unwrap();
        pump(Duration::from_millis(500));
        assert!(engine.number(c"time-pos").unwrap() > 0.2);
        assert!(frames.load(Ordering::Relaxed) > 5);
        engine.command(&["set", "pause", "yes"]).unwrap();
        for time in [2.1, 0.8, 2.0, 1.5] {
            engine.seek(time).unwrap();
        }
        pump(Duration::from_millis(500));
        assert!((engine.number(c"time-pos").unwrap() - 1.5).abs() < 0.04);
        let before = engine.number(c"time-pos").unwrap();
        engine.command(&["frame-step"]).unwrap();
        pump(Duration::from_millis(250));
        assert!((engine.number(c"time-pos").unwrap() - before - 1.0 / 30.0).abs() < 0.005);
        assert!(errors.lock().unwrap().is_empty());
        drop(engine);
    }
}
