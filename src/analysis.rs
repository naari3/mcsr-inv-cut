use crate::detect::{self, Features, Roi};
use crate::video::{self, FrameReader, VideoInfo};
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const ANALYSIS_WIDTH: u32 = 960;
/// Frames analyzed together on the rayon pool.
const BATCH: usize = 48;

pub struct Progress {
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub cancel: AtomicBool,
}

impl Progress {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            done: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            cancel: AtomicBool::new(false),
        })
    }
}

pub struct AnalysisResult {
    pub pts: Vec<f64>,
    pub features: Vec<Features>,
}

/// Decodes every frame and computes its features. Frames are decoded sequentially by
/// ffmpeg and analyzed on the rayon pool in batches.
pub fn run(path: &Path, info: &VideoInfo, roi: Roi, progress: &Progress) -> Result<AnalysisResult> {
    let mut pts = video::frame_pts(path)?;
    progress.total.store(pts.len(), Ordering::Relaxed);
    let (w, h) = video::analysis_size(info, ANALYSIS_WIDTH);
    let mut reader = FrameReader::open(path, w, h, None)?;

    let mut features = Vec::with_capacity(pts.len());
    let mut batch: Vec<Vec<u8>> = Vec::with_capacity(BATCH);
    let mut spare: Vec<Vec<u8>> = Vec::new();
    loop {
        if progress.cancel.load(Ordering::Relaxed) {
            anyhow::bail!("キャンセルされた");
        }
        let mut buf = spare.pop().unwrap_or_default();
        let more = reader.next_frame(&mut buf)?;
        if more {
            batch.push(buf);
        }
        if batch.len() == BATCH || (!more && !batch.is_empty()) {
            use rayon::prelude::*;
            let f: Vec<Features> = batch.par_iter().map(|b| detect::analyze(b, w, h, &roi)).collect();
            features.extend(f);
            progress.done.store(features.len(), Ordering::Relaxed);
            spare.append(&mut batch);
        }
        if !more {
            break;
        }
    }

    // Packet timestamps and decoded frames should match one to one. If they don't
    // (missing pts, dropped frames), fall back to a constant frame interval.
    if pts.len() != features.len() {
        let dt = 1.0 / info.avg_fps;
        let t0 = pts.first().copied().unwrap_or(info.start_time);
        pts = (0..features.len()).map(|i| t0 + crate::num::f64_from(i) * dt).collect();
    }
    Ok(AnalysisResult { pts, features })
}
