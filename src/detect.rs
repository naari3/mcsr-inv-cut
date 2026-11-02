//! Per-frame image features used to classify what is on screen.
//!
//! - Container GUIs (inventory, chest, crafting table, furnace, ...) are drawn on a
//!   flat #C6C6C6 panel with straight, axis-aligned edges. `panel` is the area of
//!   such rectangles relative to the ROI.
//! - Every screen (pause, options, containers, death screen) dims the world with a
//!   ~75% black gradient, including the HUD. The health bar's bright red hearts
//!   therefore vanish whenever any screen is open. `red` is the fraction of
//!   heart-red pixels; `bright` is the fraction of bright pixels outside panels.

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
    pub fn pixels(&self, w: u32, h: u32) -> (usize, usize, usize, usize) {
        let cx = |v: f32| ((v.clamp(0.0, 1.0) * w as f32).round() as usize).min(w as usize);
        let cy = |v: f32| ((v.clamp(0.0, 1.0) * h as f32).round() as usize).min(h as usize);
        let (x0, x1) = (cx(self.x0.min(self.x1)), cx(self.x0.max(self.x1)));
        let (y0, y1) = (cy(self.y0.min(self.y1)), cy(self.y0.max(self.y1)));
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
fn is_panel(r: u8, g: u8, b: u8) -> bool {
    (r as i32 - PANEL).abs() <= PANEL_TOL
        && (g as i32 - PANEL).abs() <= PANEL_TOL
        && (b as i32 - PANEL).abs() <= PANEL_TOL
}

#[inline]
fn is_heart_red(r: u8, g: u8, b: u8) -> bool {
    r >= 190 && g <= 70 && b <= 70
}

#[inline]
fn luma(r: u8, g: u8, b: u8) -> u32 {
    (r as u32 * 3 + g as u32 * 6 + b as u32) / 10
}

/// Detected panel rectangle in ROI pixel coordinates (inclusive bounds).
#[derive(Debug, Clone, Copy)]
pub struct PanelRect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        parent[x as usize] = parent[parent[x as usize] as usize];
        x = parent[x as usize];
    }
    x
}

/// Finds axis-aligned #C6C6C6 rectangles in an ROI-sized mask. `bright` marks
/// bright non-panel pixels, used to check that the world around a panel is dimmed.
pub fn find_panels(mask: &[bool], bright: &[bool], w: usize, h: usize) -> Vec<PanelRect> {
    // Two-pass connected component labeling (4-connectivity) with union-find.
    let mut labels = vec![0u32; w * h];
    let mut parent: Vec<u32> = vec![0];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if !mask[i] {
                continue;
            }
            let up = if y > 0 { labels[i - w] } else { 0 };
            let left = if x > 0 { labels[i - 1] } else { 0 };
            labels[i] = match (up, left) {
                (0, 0) => {
                    let l = parent.len() as u32;
                    parent.push(l);
                    l
                }
                (a, 0) | (0, a) => a,
                (a, b) => {
                    let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
                    if ra != rb {
                        parent[ra.max(rb) as usize] = ra.min(rb);
                    }
                    ra.min(rb)
                }
            };
        }
    }
    let n = parent.len();
    let mut area = vec![0u32; n];
    let mut bb = vec![(usize::MAX, usize::MAX, 0usize, 0usize); n];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if labels[i] == 0 {
                continue;
            }
            let r = find(&mut parent, labels[i]);
            labels[i] = r;
            area[r as usize] += 1;
            let b = &mut bb[r as usize];
            b.0 = b.0.min(x);
            b.1 = b.1.min(y);
            b.2 = b.2.max(x);
            b.3 = b.3.max(y);
        }
    }

    let min_w = (w as f32 * 0.05).max(8.0) as usize;
    let min_h = (h as f32 * 0.05).max(8.0) as usize;
    let mut out = Vec::new();
    for l in 1..n {
        if area[l] == 0 {
            continue;
        }
        let (x0, y0, x1, y1) = bb[l];
        let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
        if bw < min_w || bh < min_h {
            continue;
        }
        // GUI panels sit inside the screen; flat blocks close to the camera usually
        // run off its edge.
        if x0 == 0 || y0 == 0 || x1 == w - 1 || y1 == h - 1 {
            continue;
        }
        // Slots cover most of a chest panel, so the flat part can be well under half.
        let fill = area[l] as f32 / (bw * bh) as f32;
        if fill < 0.12 {
            continue;
        }
        // Straight edges: the rows/columns just inside the bounding box must be
        // almost entirely panel colored. Inset by 1 to tolerate rounded corners
        // and scaling blur. Tooltips can cover two whole edges (and even split the
        // panel), so require one straight horizontal and one straight vertical edge.
        let lab = l as u32;
        let row_cov = |y: usize| {
            (x0 + 2..=x1.saturating_sub(2)).filter(|&x| labels[y * w + x] == lab).count() as f32
                / (bw.saturating_sub(4).max(1)) as f32
        };
        let col_cov = |x: usize| {
            (y0 + 2..=y1.saturating_sub(2)).filter(|&y| labels[y * w + x] == lab).count() as f32
                / (bh.saturating_sub(4).max(1)) as f32
        };
        let top = row_cov(y0 + 1).max(row_cov(y0 + 2));
        let bottom = row_cov(y1 - 1).max(row_cov(y1 - 2));
        let left = col_cov(x0 + 1).max(col_cov(x0 + 2));
        let right = col_cov(x1 - 1).max(col_cov(x1 - 2));
        if std::env::var_os("MCSR_DEBUG").is_some() {
            eprintln!("cand {x0},{y0}-{x1},{y1} fill={fill:.2} t={top:.2} b={bottom:.2} l={left:.2} r={right:.2}");
        }
        if top.max(bottom) < 0.85 || left.max(right) < 0.85 {
            continue;
        }
        // Screens dim the world behind them, so a band just outside the panel is
        // mostly dark. Tooltips, toasts and overlays leave some bright pixels.
        let ring = ring_bright_fraction(bright, w, h, (x0, y0, x1, y1));
        if std::env::var_os("MCSR_DEBUG").is_some() {
            eprintln!("  ring_bright={ring:.3}");
        }
        if ring > 0.15 {
            continue;
        }
        out.push(PanelRect { x0, y0, x1, y1 });
    }
    out
}

fn ring_bright_fraction(bright: &[bool], w: usize, h: usize, (x0, y0, x1, y1): (usize, usize, usize, usize)) -> f32 {
    let inner = (w / 320).max(2);
    let outer = inner + (w / 80).max(6);
    let (ox0, oy0) = (x0.saturating_sub(outer), y0.saturating_sub(outer));
    let (ox1, oy1) = ((x1 + outer).min(w - 1), (y1 + outer).min(h - 1));
    let (ix0, iy0) = (x0.saturating_sub(inner), y0.saturating_sub(inner));
    let (ix1, iy1) = ((x1 + inner).min(w - 1), (y1 + inner).min(h - 1));
    let (mut n, mut b) = (0u32, 0u32);
    for y in oy0..=oy1 {
        for x in ox0..=ox1 {
            if (ix0..=ix1).contains(&x) && (iy0..=iy1).contains(&y) {
                continue;
            }
            n += 1;
            b += bright[y * w + x] as u32;
        }
    }
    if n == 0 { 0.0 } else { b as f32 / n as f32 }
}

/// Computes features of one RGB24 frame of size `w`x`h`.
pub fn analyze(rgb: &[u8], w: u32, h: u32, roi: &Roi) -> Features {
    analyze_detail(rgb, w, h, roi).0
}

/// Like [`analyze`], also returning the detected panels in frame pixel coordinates.
pub fn analyze_detail(rgb: &[u8], w: u32, h: u32, roi: &Roi) -> (Features, Vec<PanelRect>) {
    let (x0, y0, x1, y1) = roi.pixels(w, h);
    let (rw, rh) = (x1 - x0, y1 - y0);
    let mut mask = vec![false; rw * rh];
    let mut bright_mask = vec![false; rw * rh];
    let mut red = 0u32;
    let mut bright = 0u32;
    for y in 0..rh {
        let row = &rgb[((y + y0) * w as usize + x0) * 3..((y + y0) * w as usize + x1) * 3];
        for x in 0..rw {
            let (r, g, b) = (row[x * 3], row[x * 3 + 1], row[x * 3 + 2]);
            let p = is_panel(r, g, b);
            mask[y * rw + x] = p;
            if is_heart_red(r, g, b) {
                red += 1;
            }
            if !p && luma(r, g, b) >= 110 {
                bright_mask[y * rw + x] = true;
                bright += 1;
            }
        }
    }
    let total = (rw * rh) as f32;
    let panels: Vec<PanelRect> = find_panels(&mask, &bright_mask, rw, rh)
        .into_iter()
        .map(|p| PanelRect { x0: p.x0 + x0, y0: p.y0 + y0, x1: p.x1 + x0, y1: p.y1 + y0 })
        .collect();
    let panel_area: usize = panels.iter().map(|p| (p.x1 - p.x0 + 1) * (p.y1 - p.y0 + 1)).sum();
    let f = Features {
        panel: panel_area as f32 / total,
        red: red as f32 / total,
        bright: bright as f32 / total,
    };
    (f, panels)
}
