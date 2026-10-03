pub mod export;
mod mpv;
pub mod player;
pub mod probe;
pub mod waveform;

pub use export::{
    ExportCodec, ExportJob, ExportProfile, calculate_target_bitrate_kbps, execute_export,
};
pub use player::PlayerController;
pub use probe::{VideoMetadata, probe_video};

#[cfg(test)]
pub fn test_fixture() -> std::path::PathBuf {
    static FIXTURE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let directory = std::path::PathBuf::from("target/media-verification");
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("motion-audio.mp4");
            let mut command = std::process::Command::new("ffmpeg");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            let output = command
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=640x360:rate=30",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=44100",
                    "-t",
                    "3",
                    "-c:v",
                    "libx264",
                    "-g",
                    "30",
                    "-keyint_min",
                    "30",
                    "-sc_threshold",
                    "0",
                    "-c:a",
                    "aac",
                ])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::fs::canonicalize(path).unwrap()
        })
        .clone()
}
