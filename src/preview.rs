//! Frame decoding for the preview.
//!
//! Spawning ffmpeg costs ~0.3 s and a seek into the middle of a GOP ~0.5 s more,
//! so a single long-lived ffmpeg process is kept and read forward. Jumps restart it
//! at the keyframe before the target, and every decoded frame is cached so stepping
//! backwards within the GOP is immediate.

use crate::analysis::ANALYSIS_WIDTH;
use crate::video::{self, FrameReader, VideoInfo};
use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

/// Frames kept in memory (~1.5 MB each at 960x540). A bit more than one GOP of a
/// typical OBS recording (2 s at 60 fps).
const CACHE_FRAMES: usize = 150;
/// Reading forward is preferred over restarting at a keyframe up to this distance.
const MAX_READ_AHEAD: usize = 120;

pub struct Source {
    pub path: PathBuf,
    pub info: VideoInfo,
    pub pts: Arc<Vec<f64>>,
    pub keyframes: Arc<Vec<usize>>,
}

pub struct PreviewFrame {
    pub index: usize,
    pub rgb: Arc<Vec<u8>>,
    pub w: u32,
    pub h: u32,
}

enum Msg {
    Open(Source),
    Frame(usize),
}

pub struct Preview {
    tx: Sender<Msg>,
    pub rx: Receiver<Result<PreviewFrame>>,
}

impl Preview {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (tx, req_rx) = crossbeam_channel::unbounded();
        let (res_tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || Worker::new(req_rx, res_tx, ctx).run());
        Self { tx, rx }
    }

    pub fn open(&self, source: Source) {
        let _ = self.tx.send(Msg::Open(source));
    }

    pub fn request(&self, index: usize) {
        let _ = self.tx.send(Msg::Frame(index));
    }
}

struct Worker {
    rx: Receiver<Msg>,
    tx: Sender<Result<PreviewFrame>>,
    ctx: eframe::egui::Context,
    source: Option<Source>,
    size: (u32, u32),
    reader: Option<FrameReader>,
    /// Index of the frame the reader returns next.
    next: usize,
    cache: HashMap<usize, Arc<Vec<u8>>>,
    order: VecDeque<usize>,
    target: Option<usize>,
}

impl Worker {
    fn new(rx: Receiver<Msg>, tx: Sender<Result<PreviewFrame>>, ctx: eframe::egui::Context) -> Self {
        Self {
            rx,
            tx,
            ctx,
            source: None,
            size: (0, 0),
            reader: None,
            next: 0,
            cache: HashMap::new(),
            order: VecDeque::new(),
            target: None,
        }
    }

    fn run(mut self) {
        loop {
            if self.target.is_none() {
                match self.rx.recv() {
                    Ok(m) => self.handle(m),
                    Err(_) => return,
                }
            }
            if !self.drain() {
                return;
            }
            if let Some(t) = self.target {
                if let Err(e) = self.produce(t) {
                    self.target = None;
                    self.reader = None;
                    self.send(Err(e));
                }
            }
        }
    }

    /// Applies queued messages. Returns false when the app is gone.
    fn drain(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(m) => self.handle(m),
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Open(mut s) => {
                if s.keyframes.is_empty() {
                    // Projects saved before keyframes were recorded.
                    if let Ok((_, k)) = video::frame_index(&s.path) {
                        s.keyframes = Arc::new(k);
                    }
                }
                self.size = video::analysis_size(&s.info, ANALYSIS_WIDTH);
                self.source = Some(s);
                self.reader = None;
                self.cache.clear();
                self.order.clear();
                self.target = None;
            }
            Msg::Frame(i) => self.target = Some(i),
        }
    }

    fn send(&self, r: Result<PreviewFrame>) {
        let _ = self.tx.send(r);
        self.ctx.request_repaint();
    }

    fn frame(&self, index: usize, rgb: Arc<Vec<u8>>) -> PreviewFrame {
        PreviewFrame { index, rgb, w: self.size.0, h: self.size.1 }
    }

    fn insert(&mut self, index: usize, rgb: Arc<Vec<u8>>) {
        if self.cache.insert(index, rgb).is_none() {
            self.order.push_back(index);
            while self.order.len() > CACHE_FRAMES {
                if let Some(old) = self.order.pop_front() {
                    self.cache.remove(&old);
                }
            }
        }
    }

    /// Decodes until frame `t` is available, giving up early when a newer request
    /// arrives (the next loop iteration continues from where the reader is).
    fn produce(&mut self, t: usize) -> Result<()> {
        if let Some(rgb) = self.cache.get(&t).cloned() {
            self.target = None;
            self.send(Ok(self.frame(t, rgb)));
            return Ok(());
        }
        let Some(src) = &self.source else {
            self.target = None;
            return Ok(());
        };
        let (w, h) = self.size;
        let keyframe = match src.keyframes.partition_point(|&k| k <= t) {
            0 => None,
            n => Some(src.keyframes[n - 1]),
        };
        let Some(k) = keyframe else {
            // No keyframe information: decode the frame on its own.
            let rgb = Arc::new(video::grab_frame(&src.path, &src.info, src.pts[t], w, h)?);
            self.insert(t, rgb.clone());
            self.target = None;
            self.send(Ok(self.frame(t, rgb)));
            return Ok(());
        };

        let reuse = self.reader.is_some() && self.next <= t && (self.next >= k || t - self.next <= MAX_READ_AHEAD);
        let restarted = !reuse;
        if restarted {
            let ss = src.pts[k] - src.info.start_time - 0.0005;
            self.reader = Some(FrameReader::open(&src.path, w, h, Some(ss))?);
            self.next = k;
        }
        let mut buf = Vec::new();
        while self.next <= t {
            let Some(reader) = self.reader.as_mut() else { bail!("reader closed") };
            if !reader.next_frame(&mut buf)? {
                self.reader = None;
                bail!("フレーム #{t} を読めなかった");
            }
            let i = self.next;
            self.next += 1;
            let rgb = Arc::new(std::mem::take(&mut buf));
            self.insert(i, rgb.clone());
            // After a jump, show the keyframe right away while decoding onwards.
            if restarted && i == k && i != t {
                self.send(Ok(self.frame(i, rgb)));
            }
            if i == t {
                self.target = None;
                self.send(Ok(self.frame(t, self.cache[&t].clone())));
                return Ok(());
            }
            if !self.rx.is_empty() {
                return Ok(());
            }
        }
        Ok(())
    }
}
