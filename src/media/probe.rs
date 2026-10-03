use serde::Deserialize;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct VideoMetadata {
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub keyframes: Vec<f64>,
}

#[derive(Deserialize)]
struct ProbeOutput {
    streams: Option<Vec<ProbeStream>>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    duration: Option<String>,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

fn parse_frame_rate(fps_str: &str) -> f64 {
    let parts: Vec<&str> = fps_str.split('/').collect();
    if parts.len() == 2 {
        if let (Ok(num), Ok(den)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>())
            && den > 0.0
            && den.is_finite()
            && num.is_finite()
            && num > 0.0
        {
            return num / den;
        }
    } else if let Ok(val) = fps_str.parse::<f64>()
        && val.is_finite()
        && val > 0.0
    {
        return val;
    }
    30.0 // Default fallback
}

pub fn probe_video(path: &Path) -> Result<VideoMetadata, String> {
    let mut command = Command::new("ffprobe");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate,avg_frame_rate,duration",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
            path.to_str().ok_or("Invalid path string")?,
        ])
        .output()
        .map_err(|e| format!("Failed to execute ffprobe: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "ffprobe exited with error: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let parsed: ProbeOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse ffprobe JSON output: {}", e))?;

    let stream = parsed
        .streams
        .and_then(|s| s.into_iter().next())
        .ok_or("No video stream found in file")?;

    let width = stream
        .width
        .filter(|w| *w > 0)
        .ok_or("Invalid video width")?;
    let height = stream
        .height
        .filter(|h| *h > 0)
        .ok_or("Invalid video height")?;
    let fps = stream
        .avg_frame_rate
        .filter(|f| f != "0/0")
        .or(stream.r_frame_rate)
        .as_deref()
        .map(parse_frame_rate)
        .unwrap_or(30.0);

    let duration_secs = stream
        .duration
        .as_deref()
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .or_else(|| {
            parsed
                .format
                .and_then(|f| f.duration)
                .and_then(|d| d.parse::<f64>().ok())
                .filter(|d| d.is_finite() && *d > 0.0)
        })
        .ok_or("This video has no valid finite duration")?;

    Ok(VideoMetadata {
        duration_secs,
        width,
        height,
        fps,
        keyframes: Vec::new(),
    })
}

pub fn probe_keyframes(path: &Path) -> Result<Vec<f64>, String> {
    let mut command = Command::new("ffprobe");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command
        .args([
            "-v",
            "error",
            "-skip_frame",
            "nokey",
            "-select_streams",
            "V:0",
            "-show_frames",
            "-show_entries",
            "frame=best_effort_timestamp_time",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let mut keys: Vec<f64> = json["frames"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|frame| {
            frame["best_effort_timestamp_time"]
                .as_str()?
                .parse::<f64>()
                .ok()
        })
        .filter(|value| value.is_finite() && *value >= 0.0)
        .collect();
    keys.sort_by(f64::total_cmp);
    keys.dedup();
    if keys.is_empty() {
        return Err("No source keyframes were found.".into());
    }
    Ok(keys)
}

pub fn copy_range(keys: &[f64], start: f64, end: f64, duration: f64) -> (f64, f64) {
    let start = keys
        .iter()
        .copied()
        .filter(|key| *key <= start + 0.0001)
        .next_back()
        .unwrap_or(0.0);
    let end = keys
        .iter()
        .copied()
        .find(|key| *key >= end - 0.0001 && *key > start)
        .unwrap_or(duration);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copied_range_is_visible_and_aligned() {
        assert_eq!(copy_range(&[0.0, 1.0, 2.0], 0.7, 1.7, 3.0), (0.0, 2.0));
        assert_eq!(copy_range(&[0.0, 1.0, 2.0], 1.0, 2.0, 3.0), (1.0, 2.0));
        assert_eq!(copy_range(&[0.0, 1.0, 2.0], 2.1, 2.9, 3.0), (2.0, 3.0));
    }
    #[test]
    #[ignore = "requires ffprobe"]
    fn discovers_real_keyframes() {
        let keys = probe_keyframes(&crate::media::test_fixture()).unwrap();
        assert_eq!(keys.len(), 3);
        assert!((keys[1] - 1.0).abs() < 0.001);
    }
    #[test]
    fn frame_rates_are_positive_and_finite() {
        assert!((parse_frame_rate("30000/1001") - 29.97002997).abs() < 0.00001);
        for value in ["0/0", "NaN", "-1", "0", "bad"] {
            assert_eq!(parse_frame_rate(value), 30.0);
        }
    }
}
