//! Per-frame image features used to classify what is on screen.
//!
//! - Container GUIs (inventory, chest, crafting table, furnace, ...) are drawn on a
//!   flat #C6C6C6 panel with straight, axis-aligned edges. `panel` is the area of
//!   such rectangles relative to the ROI.
//! - Every screen (pause, options, containers, death screen) dims the world with a
//!   ~75% black gradient, including the HUD. The health bar's bright red hearts
//!   therefore vanish whenever any screen is open. `red` is the fraction of
//!   heart-red pixels; `bright` is the fraction of bright pixels outside panels.

use crate::num::{f32_from, ratio};
use serde::{Deserialize, Serialize};

/// Region of interest in normalized source coordinates (0..1).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Roi {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Default for Roi {
    fn default() -> Self {
        Self { x0: 0.0, y0: 0.0, x1: 1.0, y1: 1.0 }
    }
}

impl Roi {
    pub fn pixels(&self, width: u32, height: u32) -> (usize, usize, usize, usize) {
        let to_px = |v: f32, size: u32| {
            let size = size as usize;
            crate::num::to_usize(f64::from(v.clamp(0.0, 1.0) * f32_from(size)).round()).min(size)
        };
        let (x0, x1) = (to_px(self.x0.min(self.x1), width), to_px(self.x0.max(self.x1), width));
        let (y0, y1) = (to_px(self.y0.min(self.y1), height), to_px(self.y0.max(self.y1), height));
        (x0, y0, x1.max(x0 + 1), y1.max(y0 + 1))
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Features {
    /// Area of detected #C6C6C6 panels / ROI area.
    pub panel: f32,
    /// Fraction of heart-red pixels in the ROI.
    pub red: f32,
    /// Fraction of bright (luma >= 110) pixels in the ROI that are not panel pixels.
    pub bright: f32,
}

const PANEL: i32 = 198;
const PANEL_TOL: i32 = 14;

#[inline]
fn is_panel(px: [u8; 3]) -> bool {
    px.iter().all(|&c| (i32::from(c) - PANEL).abs() <= PANEL_TOL)
}

#[inline]
fn is_heart_red([red, green, blue]: [u8; 3]) -> bool {
    red >= 190 && green <= 70 && blue <= 70
}

#[inline]
fn luma([red, green, blue]: [u8; 3]) -> u32 {
    (u32::from(red) * 3 + u32::from(green) * 6 + u32::from(blue)) / 10
}

/// Detected panel rectangle in ROI pixel coordinates (inclusive bounds).
#[derive(Debug, Clone, Copy)]
pub struct PanelRect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

fn find(parent: &mut [u32], mut label: u32) -> u32 {
    while parent[label as usize] != label {
        parent[label as usize] = parent[parent[label as usize] as usize];
        label = parent[label as usize];
    }
    label
}

/// Connected components of `mask` (4-connectivity): per-pixel root labels (0 is
/// background), and per label its area and bounding box.
struct Components {
    labels: Vec<u32>,
    area: Vec<u32>,
    bbox: Vec<(usize, usize, usize, usize)>,
}

fn label_components(mask: &[bool], width: usize, height: usize) -> Components {
    // Two-pass labeling with union-find.
    let mut labels = vec![0u32; width * height];
    let mut parent: Vec<u32> = vec![0];
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            if !mask[idx] {
                continue;
            }
            let up = if y > 0 { labels[idx - width] } else { 0 };
            let left = if x > 0 { labels[idx - 1] } else { 0 };
            labels[idx] = match (up, left) {
                (0, 0) => {
                    let new = u32::try_from(parent.len()).expect("fewer than 2^32 components");
                    parent.push(new);
                    new
                }
                (only, 0) | (0, only) => only,
                (up, left) => {
                    let (root_up, root_left) = (find(&mut parent, up), find(&mut parent, left));
                    if root_up != root_left {
                        parent[root_up.max(root_left) as usize] = root_up.min(root_left);
                    }
                    root_up.min(root_left)
                }
            };
        }
    }
    let count = parent.len();
    let mut area = vec![0u32; count];
    let mut bbox = vec![(usize::MAX, usize::MAX, 0usize, 0usize); count];
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            if labels[idx] == 0 {
                continue;
            }
            let root = find(&mut parent, labels[idx]);
            labels[idx] = root;
            area[root as usize] += 1;
            let bb = &mut bbox[root as usize];
            *bb = (bb.0.min(x), bb.1.min(y), bb.2.max(x), bb.3.max(y));
        }
    }
    Components { labels, area, bbox }
}

/// Finds axis-aligned #C6C6C6 rectangles in an ROI-sized mask. `bright` marks
/// bright non-panel pixels, used to check that the world around a panel is dimmed.
pub fn find_panels(mask: &[bool], bright: &[bool], width: usize, height: usize) -> Vec<PanelRect> {
    let comps = label_components(mask, width, height);
    let min_w = (width / 20).max(8);
    let min_h = (height / 20).max(8);
    let debug = std::env::var_os("MCSR_DEBUG").is_some();
    let mut out = Vec::new();
    for (lab, (&area, &(x0, y0, x1, y1))) in (0u32..).zip(comps.area.iter().zip(&comps.bbox)).skip(1) {
        if area == 0 {
            continue;
        }
        let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
        if bw < min_w || bh < min_h {
            continue;
        }
        // GUI panels sit inside the screen; flat blocks close to the camera usually
        // run off its edge.
        if x0 == 0 || y0 == 0 || x1 == width - 1 || y1 == height - 1 {
            continue;
        }
        // Slots cover most of a chest panel, so the flat part can be well under half.
        let fill = ratio(area as usize, bw * bh);
        if fill < 0.12 {
            continue;
        }
        // Straight edges: the rows/columns just inside the bounding box must be
        // almost entirely panel colored. Inset by 1 to tolerate rounded corners
        // and scaling blur. Tooltips can cover two whole edges (and even split the
        // panel), so require one straight horizontal and one straight vertical edge.
        let labels = &comps.labels;
        let row_cov = |row: usize| {
            let hits = (x0 + 2..=x1.saturating_sub(2)).filter(|&col| labels[row * width + col] == lab).count();
            ratio(hits, bw.saturating_sub(4).max(1))
        };
        let col_cov = |col: usize| {
            let hits = (y0 + 2..=y1.saturating_sub(2)).filter(|&row| labels[row * width + col] == lab).count();
            ratio(hits, bh.saturating_sub(4).max(1))
        };
        let top = row_cov(y0 + 1).max(row_cov(y0 + 2));
        let bottom = row_cov(y1 - 1).max(row_cov(y1 - 2));
        let left = col_cov(x0 + 1).max(col_cov(x0 + 2));
        let right = col_cov(x1 - 1).max(col_cov(x1 - 2));
        if debug {
            eprintln!("cand {x0},{y0}-{x1},{y1} fill={fill:.2} t={top:.2} b={bottom:.2} l={left:.2} r={right:.2}");
        }
        if top.max(bottom) < 0.85 || left.max(right) < 0.85 {
            continue;
        }
        // Screens dim the world behind them, so a band just outside the panel is
        // mostly dark. Tooltips, toasts and overlays leave some bright pixels.
        let ring = ring_bright_fraction(bright, width, height, (x0, y0, x1, y1));
        if debug {
            eprintln!("  ring_bright={ring:.3}");
        }
        if ring > 0.15 {
            continue;
        }
        out.push(PanelRect { x0, y0, x1, y1 });
    }
    out
}

fn ring_bright_fraction(bright: &[bool], width: usize, height: usize, (x0, y0, x1, y1): (usize, usize, usize, usize)) -> f32 {
    let inner = (width / 320).max(2);
    let outer = inner + (width / 80).max(6);
    let (ox0, oy0) = (x0.saturating_sub(outer), y0.saturating_sub(outer));
    let (ox1, oy1) = ((x1 + outer).min(width - 1), (y1 + outer).min(height - 1));
    let (ix0, iy0) = (x0.saturating_sub(inner), y0.saturating_sub(inner));
    let (ix1, iy1) = ((x1 + inner).min(width - 1), (y1 + inner).min(height - 1));
    let (mut total, mut lit) = (0usize, 0usize);
    for row in oy0..=oy1 {
        for col in ox0..=ox1 {
            if (ix0..=ix1).contains(&col) && (iy0..=iy1).contains(&row) {
                continue;
            }
            total += 1;
            lit += usize::from(bright[row * width + col]);
        }
    }
    ratio(lit, total)
}

/// Computes features of one RGB24 frame of size `width`x`height`.
pub fn analyze(rgb: &[u8], width: u32, height: u32, roi: &Roi) -> Features {
    analyze_detail(rgb, width, height, roi).0
}

/// Like [`analyze`], also returning the detected panels in frame pixel coordinates.
pub fn analyze_detail(rgb: &[u8], width: u32, height: u32, roi: &Roi) -> (Features, Vec<PanelRect>) {
    let (x0, y0, x1, y1) = roi.pixels(width, height);
    let stride = width as usize;
    let (rw, rh) = (x1 - x0, y1 - y0);
    let mut mask = vec![false; rw * rh];
    let mut bright_mask = vec![false; rw * rh];
    let mut red = 0usize;
    let mut bright = 0usize;
    for row in 0..rh {
        let line = &rgb[((row + y0) * stride + x0) * 3..((row + y0) * stride + x1) * 3];
        for (col, px) in line.chunks_exact(3).enumerate() {
            let px = [px[0], px[1], px[2]];
            let panel = is_panel(px);
            mask[row * rw + col] = panel;
            if is_heart_red(px) {
                red += 1;
            }
            if !panel && luma(px) >= 110 {
                bright_mask[row * rw + col] = true;
                bright += 1;
            }
        }
    }
    let total = rw * rh;
    let panels: Vec<PanelRect> = find_panels(&mask, &bright_mask, rw, rh)
        .into_iter()
        .map(|p| PanelRect { x0: p.x0 + x0, y0: p.y0 + y0, x1: p.x1 + x0, y1: p.y1 + y0 })
        .collect();
    let panel_area: usize = panels.iter().map(|p| (p.x1 - p.x0 + 1) * (p.y1 - p.y0 + 1)).sum();
    let features = Features { panel: ratio(panel_area, total), red: ratio(red, total), bright: ratio(bright, total) };
    (features, panels)
}
