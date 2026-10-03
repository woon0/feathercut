//! Streaming peak analysis: bounded memory, cancellable, never on the UI thread.
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
pub fn peaks(path: &Path, duration: f64, cancel: &Arc<AtomicBool>) -> Result<Vec<f32>, String> {
    static ANALYSIS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _slot = ANALYSIS
        .lock()
        .map_err(|_| "Waveform worker unavailable.")?;
    if cancel.load(Ordering::Relaxed) {
        return Err("Waveform cancelled.".into());
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-threads", "1", "-vn", "-ac", "1", "-ar", "8000", "-f", "f32le", "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000 | 0x00004000);
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stream = std::io::BufReader::with_capacity(65536, child.stdout.take().unwrap());
    let per_bin = ((duration * 8000. / 2048.).ceil() as usize).max(1);
    let mut peaks = Vec::with_capacity(2049);
    let mut peak = 0f32;
    let mut count = 0usize;
    let mut sample = [0u8; 4];
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Waveform cancelled.".into());
        }
        match stream.read_exact(&mut sample) {
            Ok(()) => {
                let level = f32::from_le_bytes(sample);
                if level.is_finite() {
                    peak = peak.max(level.abs());
                }
                count += 1;
                if count == per_bin {
                    if peaks.len() < 2048 {
                        peaks.push(peak);
                    } else if let Some(last) = peaks.last_mut() {
                        *last = f32::max(*last, peak);
                    }
                    peak = 0.;
                    count = 0;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
    }
    if count > 0 {
        if peaks.len() < 2048 {
            peaks.push(peak);
        } else if let Some(last) = peaks.last_mut() {
            *last = f32::max(*last, peak);
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("Could not analyze audio.".into());
    }
    let highest = peaks.iter().copied().fold(0f32, f32::max).max(0.02);
    for p in &mut peaks {
        *p = (*p / highest).sqrt();
    }
    Ok(peaks)
}
pub fn range(peaks: &[f32], source_duration: f64, input: f64, output: f64) -> Vec<f32> {
    if peaks.is_empty() {
        return vec![];
    }
    let mut bins = Vec::with_capacity(96);
    for i in 0..96 {
        let a = ((input + (output - input) * i as f64 / 96.) / source_duration * peaks.len() as f64)
            .floor() as usize;
        let b = ((input + (output - input) * (i + 1) as f64 / 96.) / source_duration
            * peaks.len() as f64)
            .ceil() as usize;
        bins.push(
            peaks[a.min(peaks.len() - 1)..b.max(a + 1).min(peaks.len())]
                .iter()
                .copied()
                .fold(0., f32::max),
        );
    }
    bins
}
