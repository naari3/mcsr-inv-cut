use crate::detect::{Features, Roi};
use crate::video::VideoInfo;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Class {
    Game,
    Container,
    OtherGui,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Class::Game => "ゲーム画面",
            Class::Container => "コンテナGUI",
            Class::OtherGui => "その他GUI",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Minimum panel area (fraction of ROI) to call a frame a container GUI.
    pub panel_threshold: f32,
    pub detect_other_gui: bool,
    /// Below this fraction of heart-red pixels the HUD is considered dimmed.
    pub red_threshold: f32,
    /// Non-container gaps of at most this many frames between two container runs
    /// are treated as container (detection dropouts).
    pub fill_gap_frames: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            panel_threshold: 0.02,
            detect_other_gui: true,
            red_threshold: 0.00002,
            fill_gap_frames: 0,
        }
    }
}

/// Manual correction over an inclusive frame range. `None` is not stored; removing
/// an override is done by deleting it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Override {
    pub start: usize,
    pub end: usize,
    pub class: Class,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub video: PathBuf,
    pub info: VideoInfo,
    pub roi: Roi,
    pub settings: Settings,
    pub overrides: Vec<Override>,
    /// Inclusive frame range that is measured and exported.
    pub range: Option<(usize, usize)>,
    /// ROI the current features were computed with.
    #[serde(default)]
    pub analyzed_roi: Option<Roi>,
    pub pts: Vec<f64>,
    /// Indices of keyframes in `pts`; used to seek the preview.
    #[serde(default)]
    pub keyframes: Vec<usize>,
    pub features: Vec<Features>,
}

impl Project {
    pub fn project_path(video: &Path) -> PathBuf {
        let mut s = video.as_os_str().to_owned();
        s.push(".mcsrcut.json");
        PathBuf::from(s)
    }

    /// Probes a video and builds an unanalyzed project.
    pub fn from_video(video: &Path) -> anyhow::Result<Self> {
        let info = crate::video::probe(video)?;
        let (pts, keyframes) = crate::video::frame_index(video)?;
        anyhow::ensure!(!pts.is_empty(), "フレームが見つからない");
        Ok(Project {
            video: video.to_path_buf(),
            info,
            roi: Roi::default(),
            settings: Settings::default(),
            overrides: Vec::new(),
            range: None,
            analyzed_roi: None,
            pts,
            keyframes,
            features: Vec::new(),
        })
    }

    /// Loads the saved project next to the video, or probes the video.
    pub fn open(video: &Path) -> anyhow::Result<Self> {
        let pp = Self::project_path(video);
        if pp.exists() {
            let mut p = Self::load(&pp)?;
            p.video = video.to_path_buf();
            return Ok(p);
        }
        Self::from_video(video)
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }

    pub fn save(&self) -> anyhow::Result<PathBuf> {
        let p = Self::project_path(&self.video);
        std::fs::write(&p, serde_json::to_vec(self)?)?;
        Ok(p)
    }

    pub fn frame_count(&self) -> usize {
        self.pts.len()
    }

    pub fn analyzed(&self) -> bool {
        !self.features.is_empty() && self.features.len() == self.pts.len()
    }

    /// Duration of each frame: until the next frame, the last one gets the average.
    pub fn frame_duration(&self, i: usize) -> f64 {
        match self.pts.get(i + 1) {
            Some(n) => n - self.pts[i],
            None => 1.0 / self.info.avg_fps,
        }
    }

    pub fn frame_end_time(&self, i: usize) -> f64 {
        self.pts[i] + self.frame_duration(i)
    }

    pub fn frame_at_time(&self, t: f64) -> usize {
        match self.pts.binary_search_by(|p| p.total_cmp(&t)) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        }
        .min(self.pts.len().saturating_sub(1))
    }

    pub fn range(&self) -> (usize, usize) {
        let last = self.frame_count().saturating_sub(1);
        match self.range {
            Some((a, b)) => (a.min(last), b.min(last).max(a.min(last))),
            None => (0, last),
        }
    }

    pub fn auto_classes(&self) -> Vec<Class> {
        let s = &self.settings;
        // Frames without features (not analyzed yet) count as gameplay.
        let mut c: Vec<Class> = (0..self.pts.len())
            .map(|i| {
                let Some(f) = self.features.get(i) else {
                    return Class::Game;
                };
                if f.panel >= s.panel_threshold {
                    Class::Container
                } else if s.detect_other_gui && f.red < s.red_threshold {
                    Class::OtherGui
                } else {
                    Class::Game
                }
            })
            .collect();
        if s.fill_gap_frames > 0 {
            fill_gaps(&mut c, Class::Container, s.fill_gap_frames);
        }
        c
    }

    pub fn final_classes(&self) -> Vec<Class> {
        let mut c = self.auto_classes();
        for o in &self.overrides {
            let end = o.end.min(c.len().saturating_sub(1));
            for x in c.iter_mut().take(end + 1).skip(o.start) {
                *x = o.class;
            }
        }
        c
    }

    /// Sets an override on [a, b], replacing overlapping parts of older overrides.
    pub fn set_override(&mut self, a: usize, b: usize, class: Option<Class>) {
        let (a, b) = (a.min(b), a.max(b));
        let mut out = Vec::new();
        for o in self.overrides.drain(..) {
            if o.end < a || o.start > b {
                out.push(o);
                continue;
            }
            if o.start < a {
                out.push(Override { start: o.start, end: a - 1, class: o.class });
            }
            if o.end > b {
                out.push(Override { start: b + 1, end: o.end, class: o.class });
            }
        }
        if let Some(class) = class {
            out.push(Override { start: a, end: b, class });
        }
        out.sort_by_key(|o| o.start);
        self.overrides = out;
    }
}

fn fill_gaps(c: &mut [Class], class: Class, max_gap: usize) {
    let mut last_end: Option<usize> = None;
    let mut i = 0;
    while i < c.len() {
        if c[i] == class {
            if let Some(e) = last_end {
                if i - e - 1 <= max_gap && i > e + 1 {
                    for x in &mut c[e + 1..i] {
                        *x = class;
                    }
                }
            }
            while i < c.len() && c[i] == class {
                i += 1;
            }
            last_end = Some(i - 1);
        } else {
            i += 1;
        }
    }
}

/// Run of identical classes, inclusive frame indices.
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub start: usize,
    pub end: usize,
    pub class: Class,
}

pub fn segments(classes: &[Class], range: (usize, usize)) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for i in range.0..=range.1.min(classes.len().saturating_sub(1)) {
        match out.last_mut() {
            Some(s) if s.class == classes[i] && s.end + 1 == i => s.end = i,
            _ => out.push(Segment { start: i, end: i, class: classes[i] }),
        }
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub total: f64,
    pub container: f64,
    pub other_gui: f64,
    pub game: f64,
    pub container_count: usize,
    pub frames: usize,
}

impl Stats {
    pub fn non_container(&self) -> f64 {
        self.total - self.container
    }
}

pub fn stats(p: &Project, classes: &[Class]) -> Stats {
    let mut s = Stats::default();
    if p.frame_count() == 0 {
        return s;
    }
    let (a, b) = p.range();
    for i in a..=b {
        let d = p.frame_duration(i);
        s.total += d;
        match classes[i] {
            Class::Container => s.container += d,
            Class::OtherGui => s.other_gui += d,
            Class::Game => s.game += d,
        }
    }
    s.frames = b - a + 1;
    s.container_count = segments(classes, (a, b)).iter().filter(|x| x.class == Class::Container).count();
    s
}

pub fn fmt_time(t: f64) -> String {
    let neg = t < 0.0;
    let t = t.abs();
    let ms = (t * 1000.0).round() as u64;
    let (h, m, s, ms) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let sign = if neg { "-" } else { "" };
    if h > 0 {
        format!("{sign}{h}:{m:02}:{s:02}.{ms:03}")
    } else {
        format!("{sign}{m:02}:{s:02}.{ms:03}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_gaps_only_between_runs() {
        use Class::*;
        let mut c = vec![Game, Container, Game, Game, Container, Game, Game, Game, Container, Game];
        fill_gaps(&mut c, Container, 2);
        assert_eq!(c, vec![Game, Container, Container, Container, Container, Game, Game, Game, Container, Game]);
    }

    #[test]
    fn override_splits_existing() {
        let info = VideoInfo { width: 1, height: 1, start_time: 0.0, duration: 1.0, avg_fps: 10.0, has_audio: false };
        let mut p = Project {
            video: PathBuf::new(),
            info,
            roi: Roi::default(),
            settings: Settings::default(),
            overrides: vec![],
            range: None,
            analyzed_roi: None,
            pts: (0..10).map(|i| i as f64 / 10.0).collect(),
            keyframes: vec![],
            features: vec![Features::default(); 10],
        };
        p.set_override(2, 7, Some(Class::Container));
        p.set_override(4, 5, None);
        assert_eq!(p.overrides.len(), 2);
        assert_eq!((p.overrides[0].start, p.overrides[0].end), (2, 3));
        assert_eq!((p.overrides[1].start, p.overrides[1].end), (6, 7));
    }

    #[test]
    fn stats_sum_frame_durations() {
        let info = VideoInfo { width: 1, height: 1, start_time: 0.0, duration: 1.0, avg_fps: 10.0, has_audio: false };
        let mut p = Project {
            video: PathBuf::new(),
            info,
            roi: Roi::default(),
            settings: Settings { detect_other_gui: false, ..Settings::default() },
            overrides: vec![],
            range: None,
            analyzed_roi: None,
            pts: (0..10).map(|i| i as f64 / 10.0).collect(),
            keyframes: vec![],
            features: vec![Features::default(); 10],
        };
        p.set_override(3, 4, Some(Class::Container));
        let s = stats(&p, &p.final_classes());
        assert!((s.total - 1.0).abs() < 1e-9);
        assert!((s.container - 0.2).abs() < 1e-9);
        assert_eq!(s.container_count, 1);
    }
}
