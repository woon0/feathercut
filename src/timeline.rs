//! Lightweight source references, free placement, layers and reversible editing.
use crate::media::VideoMetadata;
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Debug)]
pub struct Source {
    pub path: PathBuf,
    pub metadata: VideoMetadata,
    pub has_audio: bool,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crop {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}
impl Default for Crop {
    fn default() -> Self {
        Self {
            x: 0.,
            y: 0.,
            w: 1.,
            h: 1.,
        }
    }
}
impl Crop {
    pub fn pixels(self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let x = ((self.x * width as f64).round() as u32).min(width.saturating_sub(2)) / 2 * 2;
        let y = ((self.y * height as f64).round() as u32).min(height.saturating_sub(2)) / 2 * 2;
        let w = ((self.w * width as f64).round() as u32)
            .min(width - x)
            .max(2)
            / 2
            * 2;
        let h = ((self.h * height as f64).round() as u32)
            .min(height - y)
            .max(2)
            / 2
            * 2;
        (x, y, w, h)
    }
}
#[derive(Clone, Debug)]
pub struct Clip {
    pub id: u64,
    pub source: Arc<Source>,
    pub input: f64,
    pub output: f64,
    pub start: f64,
    pub layer: u32,
    pub crop: Crop,
}
impl Clip {
    pub fn duration(&self) -> f64 {
        self.output - self.input
    }
}
#[derive(Clone, Debug)]
pub struct AudioClip {
    pub clip: Clip,
    pub start: f64,
    pub linked_video: Option<u64>,
    pub gain: f64,
    pub muted: bool,
}
#[derive(Clone, Debug)]
pub struct Project {
    pub video: Vec<Clip>,
    pub audio: Vec<AudioClip>,
    pub video_layers: u32,
    pub audio_layers: u32,
    next_id: u64,
}
impl Default for Project {
    fn default() -> Self {
        Self {
            video: vec![],
            audio: vec![],
            video_layers: 1,
            audio_layers: 1,
            next_id: 0,
        }
    }
}
#[derive(Clone, Debug)]
pub struct AudioPreview {
    pub path: PathBuf,
    pub gains: Vec<(f64, f64, f64)>,
}
#[derive(Clone, Debug)]
pub struct PreviewPlan {
    pub video: PathBuf,
    pub audio: Option<PathBuf>,
    pub gains: Vec<(f64, f64, f64)>,
    pub extra_audio: Vec<AudioPreview>,
    pub crops: Vec<(f64, f64, Crop, u32, u32)>,
    pub metadata: VideoMetadata,
    pub embedded_audio: bool,
    pub needs_black: bool,
}
fn segment(path: &str, start: f64, duration: f64) -> String {
    format!("%{}%{path},{start:.9},{duration:.9}", path.len())
}
pub fn silence_path() -> PathBuf {
    std::env::temp_dir().join("feathercut-preview-silence-10s-v1.wav")
}
pub fn black_path() -> PathBuf {
    std::env::temp_dir().join("feathercut-preview-black-10s-v1.mp4")
}
fn fill(segments: &mut Vec<String>, path: &std::path::Path, mut duration: f64) {
    while duration > 1e-8 {
        let length = duration.min(10.);
        segments.push(segment(&path.to_string_lossy(), 0., length));
        duration -= length;
    }
}
impl Project {
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
    pub fn duration(&self) -> f64 {
        self.video
            .iter()
            .map(|v| v.start + v.duration())
            .chain(self.audio.iter().map(|a| a.start + a.clip.duration()))
            .fold(0., f64::max)
    }
    pub fn start(&self, id: u64) -> Option<f64> {
        self.video.iter().find(|c| c.id == id).map(|c| c.start)
    }
    pub fn add_video(&mut self, source: Arc<Source>) -> u64 {
        let start = self
            .video
            .iter()
            .map(|c| c.start + c.duration())
            .fold(0., f64::max);
        let id = self.id();
        let clip = Clip {
            id,
            input: 0.,
            output: source.metadata.duration_secs,
            source,
            start,
            layer: 0,
            crop: Crop::default(),
        };
        if clip.source.has_audio {
            let audio_id = self.id();
            self.audio.push(AudioClip {
                clip: Clip {
                    id: audio_id,
                    ..clip.clone()
                },
                start,
                linked_video: Some(id),
                gain: 1.,
                muted: false,
            });
        }
        self.video.push(clip);
        id
    }
    pub fn add_audio(&mut self, source: Arc<Source>, start: f64) -> Result<u64, String> {
        if self.video.is_empty() {
            return Err("Open a video before adding audio.".into());
        }
        let duration = source.metadata.duration_secs;
        let layer = (0..self.audio_layers)
            .find(|layer| self.audio_space(None, start, duration, *layer).is_ok())
            .unwrap_or(self.audio_layers);
        self.audio_layers = self.audio_layers.max(layer + 1);
        let id = self.id();
        self.audio.push(AudioClip {
            clip: Clip {
                id,
                source,
                input: 0.,
                output: duration,
                start,
                layer,
                crop: Crop::default(),
            },
            start,
            linked_video: None,
            gain: 1.,
            muted: false,
        });
        Ok(id)
    }
    pub fn sync_links(&mut self) {
        for v in &self.video {
            for a in &mut self.audio {
                if a.linked_video == Some(v.id) {
                    a.start = v.start;
                    a.clip.start = v.start;
                    a.clip.input = v.input;
                    a.clip.output = v.output;
                    a.clip.layer = v.layer;
                }
            }
        }
        self.audio.retain(|a| {
            a.linked_video.is_none() || self.video.iter().any(|v| Some(v.id) == a.linked_video)
        });
    }
    pub fn add_layer(&mut self, audio: bool) {
        if audio {
            self.audio_layers += 1;
        } else {
            self.video_layers += 1;
        }
    }
    pub fn move_video(&mut self, id: u64, start: f64, layer: u32) -> Result<(), String> {
        let c = self
            .video
            .iter()
            .find(|c| c.id == id)
            .ok_or("Select a video clip.")?;
        if !start.is_finite() || start < 0. {
            return Err("Keep clips at or after zero.".into());
        }
        if self.video.iter().any(|v| {
            v.id != id
                && v.layer == layer
                && start < v.start + v.duration() - 1e-6
                && start + c.duration() > v.start + 1e-6
        }) {
            return Err(
                "Clips on the same video layer cannot overlap. Add a video layer to overlap them."
                    .into(),
            );
        }
        let c = self.video.iter_mut().find(|c| c.id == id).unwrap();
        c.start = start;
        c.layer = layer;
        self.video_layers = self.video_layers.max(layer + 1);
        self.sync_links();
        Ok(())
    }
    #[cfg(test)]
    pub fn split_video(&mut self, time: f64) -> Result<u64, String> {
        let id = self
            .video
            .iter()
            .filter(|c| time > c.start && time < c.start + c.duration())
            .max_by_key(|c| (c.layer, c.id))
            .map(|c| c.id)
            .ok_or("Place the playhead inside a clip.")?;
        self.split_video_at(id, time)
    }
    pub fn split_video_at(&mut self, id: u64, time: f64) -> Result<u64, String> {
        let i = self
            .video
            .iter()
            .position(|c| c.id == id)
            .ok_or("Select a clip.")?;
        let original = self.video[i].clone();
        let fps = original.source.metadata.fps.max(1.);
        let offset = ((time - original.start) * fps).round() / fps;
        if offset < 1. / fps || offset > original.duration() - 1. / fps + 1e-7 {
            return Err("Place the playhead inside the selected clip, away from its edges.".into());
        }
        let id = self.id();
        self.video[i].output = original.input + offset;
        self.video.insert(
            i + 1,
            Clip {
                id,
                input: original.input + offset,
                start: original.start + offset,
                ..original.clone()
            },
        );
        if let Some(a) = self
            .audio
            .iter()
            .find(|a| a.linked_video == Some(original.id))
            .cloned()
        {
            let audio_id = self.id();
            self.audio.push(AudioClip {
                clip: Clip {
                    id: audio_id,
                    input: original.input + offset,
                    ..a.clip.clone()
                },
                start: original.start + offset,
                linked_video: Some(id),
                ..a
            });
        }
        self.sync_links();
        Ok(id)
    }
    pub fn split_audio(&mut self, id: u64, time: f64) -> Result<u64, String> {
        let i = self
            .audio
            .iter()
            .position(|a| a.clip.id == id)
            .ok_or("Select audio.")?;
        let a = self.audio[i].clone();
        if a.linked_video.is_some() {
            return Err("Detach audio to split it independently.".into());
        }
        let offset = time - a.start;
        if offset <= 0.001 || offset >= a.clip.duration() - 0.001 {
            return Err("Place the playhead inside the audio clip.".into());
        }
        let id = self.id();
        self.audio[i].clip.output = a.clip.input + offset;
        self.audio.push(AudioClip {
            clip: Clip {
                id,
                input: a.clip.input + offset,
                start: time,
                ..a.clip
            },
            start: time,
            ..a
        });
        Ok(id)
    }
    // Explicit packing is retained for the simple arrange operation and existing projects.
    #[cfg(test)]
    pub fn reorder(&mut self, id: u64, index: usize) -> Result<(), String> {
        let i = self
            .video
            .iter()
            .position(|c| c.id == id)
            .ok_or("Select video.")?;
        let c = self.video.remove(i);
        let index = index.min(self.video.len());
        self.video.insert(index, c);
        let mut start = 0.;
        for c in &mut self.video {
            c.start = start;
            start += c.duration();
        }
        self.sync_links();
        Ok(())
    }
    pub fn detach(&mut self, id: u64) -> Result<u64, String> {
        let i = self
            .audio
            .iter()
            .position(|a| a.linked_video == Some(id))
            .ok_or("This video has no linked audio.")?;
        let a = &self.audio[i];
        let layer = (0..self.audio_layers)
            .find(|l| {
                self.audio_space(Some(a.clip.id), a.start, a.clip.duration(), *l)
                    .is_ok()
            })
            .unwrap_or(self.audio_layers);
        self.audio_layers = self.audio_layers.max(layer + 1);
        self.audio[i].linked_video = None;
        self.audio[i].clip.layer = layer;
        Ok(self.audio[i].clip.id)
    }
    pub fn trim(&mut self, id: u64, audio: bool, input: f64, output: f64) -> Result<(), String> {
        let c = if audio {
            self.audio.iter().find(|a| a.clip.id == id).map(|a| &a.clip)
        } else {
            self.video.iter().find(|c| c.id == id)
        }
        .ok_or("Select a clip.")?;
        let gap = if audio {
            0.001
        } else {
            1. / c.source.metadata.fps.max(1.)
        };
        if !input.is_finite()
            || !output.is_finite()
            || input < 0.
            || output > c.source.metadata.duration_secs + 1e-6
            || output - input < gap - 1e-7
        {
            return Err(
                "Keep a valid source range with at least one frame (or 1 ms of audio).".into(),
            );
        }
        if audio {
            let a = self.audio.iter().find(|a| a.clip.id == id).unwrap();
            if a.linked_video.is_some() {
                return Err("Detach audio to trim it independently.".into());
            }
            self.audio_space(Some(id), a.start, output - input, a.clip.layer)?;
            let a = self.audio.iter_mut().find(|a| a.clip.id == id).unwrap();
            a.clip.input = input;
            a.clip.output = output;
        } else {
            if self.video.iter().any(|v| {
                v.id != id
                    && v.layer == c.layer
                    && c.start < v.start + v.duration() - 1e-6
                    && c.start + output - input > v.start + 1e-6
            }) {
                return Err("Trim would overlap another clip on this layer.".into());
            }
            let c = self.video.iter_mut().find(|c| c.id == id).unwrap();
            c.input = input;
            c.output = output;
            self.sync_links();
        }
        Ok(())
    }
    pub fn trim_edge(&mut self, id: u64, audio: bool, out: bool, delta: f64) -> Result<(), String> {
        let c = if audio {
            self.audio
                .iter()
                .find(|a| a.clip.id == id)
                .map(|a| a.clip.clone())
        } else {
            self.video.iter().find(|c| c.id == id).cloned()
        }
        .ok_or("Select a clip.")?;
        let start = if audio {
            self.audio.iter().find(|a| a.clip.id == id).unwrap().start
        } else {
            c.start
        };
        let gap = if audio {
            0.001
        } else {
            1. / c.source.metadata.fps.max(1.)
        };
        let value = if out {
            (c.output + delta).clamp(c.input + gap, c.source.metadata.duration_secs)
        } else {
            (c.input + delta).clamp((c.input - start).max(0.), c.output - gap)
        };
        let new_start = if out { start } else { start + value - c.input };
        let input = if out { c.input } else { value };
        let output = if out { value } else { c.output };
        if audio {
            if self
                .audio
                .iter()
                .find(|a| a.clip.id == id)
                .unwrap()
                .linked_video
                .is_some()
            {
                return Err("Detach audio before trimming independently.".into());
            }
            self.audio_space(Some(id), new_start, output - input, c.layer)?;
            let a = self.audio.iter_mut().find(|a| a.clip.id == id).unwrap();
            a.start = new_start;
            a.clip.start = new_start;
            a.clip.input = input;
            a.clip.output = output;
        } else {
            if self.video.iter().any(|v| {
                v.id != id
                    && v.layer == c.layer
                    && new_start < v.start + v.duration() - 1e-6
                    && new_start + output - input > v.start + 1e-6
            }) {
                return Err("Trim would overlap another clip on this layer.".into());
            }
            let v = self.video.iter_mut().find(|v| v.id == id).unwrap();
            v.start = new_start;
            v.input = input;
            v.output = output;
            self.sync_links();
        }
        Ok(())
    }
    fn audio_space(
        &self,
        id: Option<u64>,
        start: f64,
        duration: f64,
        layer: u32,
    ) -> Result<(), String> {
        if !start.is_finite() || start < 0. {
            return Err("Keep audio at or after zero.".into());
        }
        if self.audio.iter().any(|a| {
            a.linked_video.is_none()
                && !a.muted
                && Some(a.clip.id) != id
                && a.clip.layer == layer
                && start < a.start + a.clip.duration() - 1e-6
                && start + duration > a.start + 1e-6
        }) {
            return Err("Audio overlaps on this layer. Move it to another audio layer.".into());
        }
        Ok(())
    }
    pub fn move_audio(&mut self, id: u64, start: f64) -> Result<(), String> {
        let layer = self
            .audio
            .iter()
            .find(|a| a.clip.id == id)
            .ok_or("Select audio.")?
            .clip
            .layer;
        self.move_audio_layer(id, start, layer)
    }
    pub fn move_audio_layer(&mut self, id: u64, start: f64, layer: u32) -> Result<(), String> {
        let a = self
            .audio
            .iter()
            .find(|a| a.clip.id == id)
            .ok_or("Select audio.")?;
        if a.linked_video.is_some() {
            return Err("Detach audio to move it independently.".into());
        }
        self.audio_space(Some(id), start, a.clip.duration(), layer)?;
        let a = self.audio.iter_mut().find(|a| a.clip.id == id).unwrap();
        a.start = start;
        a.clip.start = start;
        a.clip.layer = layer;
        self.audio_layers = self.audio_layers.max(layer + 1);
        Ok(())
    }
    pub fn remove(&mut self, id: u64, audio: bool) {
        if audio {
            self.audio.retain(|a| a.clip.id != id);
        } else {
            self.video.retain(|c| c.id != id);
            self.sync_links();
        }
    }
    pub fn gain(&mut self, id: u64, gain: f64, muted: bool) -> Result<(), String> {
        let a = self
            .audio
            .iter_mut()
            .find(|a| a.clip.id == id || a.linked_video == Some(id))
            .ok_or("This clip has no audio.")?;
        a.gain = gain.clamp(0., 2.);
        a.muted = muted;
        Ok(())
    }
    pub fn crop(&mut self, id: u64, crop: Crop) -> Result<(), String> {
        let source = &self
            .video
            .iter()
            .find(|v| v.id == id)
            .ok_or("Select video to crop.")?
            .source
            .metadata;
        if [crop.x, crop.y, crop.w, crop.h]
            .iter()
            .any(|v| !v.is_finite())
            || crop.x < 0.
            || crop.y < 0.
            || crop.w < 2. / source.width as f64 - 1e-7
            || crop.h < 2. / source.height as f64 - 1e-7
            || crop.x + crop.w > 1.00001
            || crop.y + crop.h > 1.00001
        {
            return Err("Keep a non-empty crop inside the video frame.".into());
        }
        self.video
            .iter_mut()
            .find(|v| v.id == id)
            .ok_or("Select video to crop.")?
            .crop = crop;
        Ok(())
    }
    pub fn simple_source(&self) -> Option<&Clip> {
        if self.video.len() != 1 {
            return None;
        }
        let v = &self.video[0];
        if v.start != 0. || v.crop != Crop::default() {
            return None;
        }
        let unchanged = if v.source.has_audio {
            self.audio.len() == 1
                && self.audio[0].linked_video == Some(v.id)
                && self.audio[0].gain == 1.
                && !self.audio[0].muted
        } else {
            self.audio.is_empty()
        };
        unchanged.then_some(v)
    }
    pub fn selected_project(&self, id: u64, full: bool) -> Self {
        let mut p = Self::default();
        if let Some(v) = self.video.iter().find(|v| v.id == id).cloned() {
            let mut v = v;
            v.start = 0.;
            v.layer = 0;
            if full {
                v.input = 0.;
                v.output = v.source.metadata.duration_secs;
            }
            for a in self.audio.iter().filter(|a| a.linked_video == Some(id)) {
                let mut a = a.clone();
                a.start = 0.;
                a.clip.start = 0.;
                a.clip.layer = 0;
                a.clip.input = v.input;
                a.clip.output = v.output;
                p.audio.push(a);
            }
            p.video.push(v);
        }
        p
    }
    pub fn visible_segments(&self) -> Vec<(f64, f64, Option<&Clip>)> {
        let duration = self.duration();
        let mut points = vec![0., duration];
        for v in &self.video {
            points.push(v.start);
            points.push(v.start + v.duration());
        }
        points.sort_by(f64::total_cmp);
        points.dedup_by(|a, b| (*a - *b).abs() < 1e-8);
        points
            .windows(2)
            .filter(|p| p[1] > p[0] + 1e-8)
            .map(|p| {
                let t = (p[0] + p[1]) / 2.;
                let v = self
                    .video
                    .iter()
                    .filter(|v| t >= v.start && t < v.start + v.duration())
                    .max_by_key(|v| (v.layer, v.id));
                (p[0], p[1], v)
            })
            .collect()
    }
    pub fn preview(&self) -> Result<PreviewPlan, String> {
        let first = self.video.first().ok_or("Add a video clip.")?;
        let mut metadata = first.source.metadata.clone();
        metadata.duration_secs = self.duration();
        metadata.keyframes.clear();
        let mut video = vec![];
        let mut crops = vec![];
        let mut needs_black = false;
        for (start, end, clip) in self.visible_segments() {
            if let Some(c) = clip {
                video.push(segment(
                    &c.source.path.to_string_lossy(),
                    c.input + start - c.start,
                    end - start,
                ));
                crops.push((
                    start,
                    end,
                    c.crop,
                    c.source.metadata.width,
                    c.source.metadata.height,
                ));
            } else {
                needs_black = true;
                fill(&mut video, &black_path(), end - start);
            }
        }
        if let Some(c) = self.simple_source() {
            let path = if c.input == 0. && c.output == c.source.metadata.duration_secs {
                c.source.path.clone()
            } else {
                PathBuf::from("edl://".to_owned() + &video.join(";"))
            };
            return Ok(PreviewPlan {
                video: path,
                audio: None,
                gains: vec![],
                extra_audio: vec![],
                crops,
                metadata,
                embedded_audio: true,
                needs_black: false,
            });
        }
        let mut keys: Vec<_> = self
            .audio
            .iter()
            .filter(|a| !a.muted)
            .map(|a| (a.linked_video.is_some(), a.clip.layer))
            .collect();
        keys.sort();
        keys.dedup();
        let mut tracks = vec![];
        for key in keys {
            let mut clips: Vec<_> = self
                .audio
                .iter()
                .filter(|a| !a.muted && (a.linked_video.is_some(), a.clip.layer) == key)
                .collect();
            clips.sort_by(|a, b| a.start.total_cmp(&b.start));
            let mut cursor = 0.;
            let mut segments = vec![];
            let mut gains = vec![];
            for a in clips {
                if a.start < cursor - 1e-6 {
                    return Err("Audio clips overlap within one layer.".into());
                }
                if a.start > cursor {
                    fill(&mut segments, &silence_path(), a.start - cursor);
                }
                segments.push(segment(
                    &a.clip.source.path.to_string_lossy(),
                    a.clip.input,
                    a.clip.duration(),
                ));
                gains.push((a.start, a.start + a.clip.duration(), a.gain));
                cursor = a.start + a.clip.duration();
            }
            if cursor < self.duration() {
                fill(&mut segments, &silence_path(), self.duration() - cursor);
            }
            if !segments.is_empty() {
                tracks.push(AudioPreview {
                    path: PathBuf::from("edl://".to_owned() + &segments.join(";")),
                    gains,
                });
            }
        }
        let first = if tracks.is_empty() {
            None
        } else {
            Some(tracks.remove(0))
        };
        Ok(PreviewPlan {
            video: PathBuf::from("edl://".to_owned() + &video.join(";")),
            audio: first.as_ref().map(|a| a.path.clone()),
            gains: first.map(|a| a.gains).unwrap_or_default(),
            extra_audio: tracks,
            crops,
            metadata,
            embedded_audio: false,
            needs_black,
        })
    }
}

#[derive(Default)]
pub struct History {
    pub project: Project,
    undo: Vec<Project>,
    redo: Vec<Project>,
}
impl History {
    pub fn edit<T>(
        &mut self,
        edit: impl FnOnce(&mut Project) -> Result<T, String>,
    ) -> Result<T, String> {
        let before = self.project.clone();
        match edit(&mut self.project) {
            Ok(value) => {
                self.undo.push(before);
                if self.undo.len() > 100 {
                    self.undo.remove(0);
                }
                self.redo.clear();
                Ok(value)
            }
            Err(e) => {
                self.project = before;
                Err(e)
            }
        }
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo(&mut self) {
        if let Some(p) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.project, p));
        }
    }
    pub fn redo(&mut self) {
        if let Some(p) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.project, p));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> Arc<Source> {
        Arc::new(Source {
            path: "D:/a,é;clip.mp4".into(),
            metadata: VideoMetadata {
                duration_secs: 10.,
                width: 640,
                height: 360,
                fps: 30.,
                keyframes: vec![],
            },
            has_audio: true,
        })
    }
    #[test]
    fn split_reorder_trim_detach_and_undo_keep_sources_and_audio_aligned() {
        let mut h = History::default();
        let id = h.edit(|p| Ok(p.add_video(source()))).unwrap();
        let right = h.edit(|p| p.split_video(4.)).unwrap();
        assert_eq!(h.project.duration(), 10.);
        assert_eq!(h.project.audio.len(), 2);
        h.edit(|p| p.reorder(right, 0)).unwrap();
        assert_eq!(h.project.video[0].input, 4.);
        assert_eq!(
            h.project
                .audio
                .iter()
                .find(|a| a.linked_video == Some(id))
                .unwrap()
                .start,
            6.
        );
        let audio = h.edit(|p| p.detach(right)).unwrap();
        h.edit(|p| p.trim(audio, true, 4., 5.)).unwrap();
        h.edit(|p| p.move_audio(audio, 2.)).unwrap();
        h.undo();
        assert_eq!(
            h.project
                .audio
                .iter()
                .find(|a| a.clip.id == audio)
                .unwrap()
                .start,
            0.
        );
        h.redo();
        assert_eq!(
            h.project
                .audio
                .iter()
                .find(|a| a.clip.id == audio)
                .unwrap()
                .start,
            2.
        );
        assert!(
            h.project
                .preview()
                .unwrap()
                .video
                .to_string_lossy()
                .contains("%")
        );
        assert!(h.edit(|p| p.move_audio(audio, -1.)).is_err());
        assert_eq!(
            h.project
                .audio
                .iter()
                .find(|a| a.clip.id == audio)
                .unwrap()
                .start,
            2.
        );
    }
}
