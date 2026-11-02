use crate::model::{Class, Project, Segment, fmt_time, segments, stats};
use crate::video;
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutMode {
    /// Remove container GUI frames.
    RemoveContainer,
    /// Remove container and other GUI frames (gameplay only).
    RemoveAllGui,
    /// Keep only container GUI frames.
    OnlyContainer,
}

impl CutMode {
    pub fn label(self) -> &'static str {
        match self {
            CutMode::RemoveContainer => "コンテナGUIを除去",
            CutMode::RemoveAllGui => "GUIをすべて除去",
            CutMode::OnlyContainer => "コンテナGUIのみ残す",
        }
    }

    fn keep(self, c: Class) -> bool {
        match self {
            CutMode::RemoveContainer => c != Class::Container,
            CutMode::RemoveAllGui => c == Class::Game,
            CutMode::OnlyContainer => c == Class::Container,
        }
    }
}

/// Kept time intervals [start, end) in absolute seconds, merged.
pub fn keep_intervals(p: &Project, classes: &[Class], mode: CutMode) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    for s in segments(classes, p.range()) {
        if !mode.keep(s.class) {
            continue;
        }
        let (a, b) = (p.pts[s.start], p.frame_end_time(s.end));
        match out.last_mut() {
            Some(last) if (last.1 - a).abs() < 1e-9 => last.1 = b,
            _ => out.push((a, b)),
        }
    }
    out
}

pub fn write_csv(p: &Project, classes: &[Class], path: &Path) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    // BOM so that Excel opens UTF-8 correctly.
    f.write_all(b"\xEF\xBB\xBF")?;
    writeln!(f, "class,start_frame,end_frame,start,end,duration_sec")?;
    for Segment { start, end, class } in segments(classes, p.range()) {
        let (a, b) = (p.pts[start] - p.info.start_time, p.frame_end_time(end) - p.info.start_time);
        writeln!(f, "{},{start},{end},{},{},{:.6}", class.label(), fmt_time(a), fmt_time(b), b - a)?;
    }
    let s = stats(p, classes);
    writeln!(f)?;
    writeln!(f, "total,,,,,{:.6}", s.total)?;
    writeln!(f, "container,,,,,{:.6}", s.container)?;
    writeln!(f, "non_container,,,,,{:.6}", s.non_container())?;
    writeln!(f, "other_gui,,,,,{:.6}", s.other_gui)?;
    writeln!(f, "game,,,,,{:.6}", s.game)?;
    Ok(())
}

fn is_cfr(pts: &[f64], fps: f64) -> bool {
    let dt = 1.0 / fps;
    pts.windows(2).all(|w| ((w[1] - w[0]) - dt).abs() < dt * 0.05)
}

/// ffmpeg 7.1 added `-/option file` to read an option value from a file and
/// deprecated `-filter_complex_script`.
fn supports_file_option_prefix() -> bool {
    let Ok(out) = video::command("ffmpeg").arg("-version").output() else {
        return false;
    };
    let s = String::from_utf8_lossy(&out.stdout);
    let Some(ver) = s.split_whitespace().nth(2) else {
        return false;
    };
    // Git builds ("N-12345-g...") are newer than any release that lacks it.
    if ver.starts_with('N') {
        return true;
    }
    let mut it = ver.split(|c: char| !c.is_ascii_digit()).filter_map(|x| x.parse::<u32>().ok());
    let (major, minor) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    (major, minor) >= (7, 1)
}

pub struct EncodeOptions {
    pub mode: CutMode,
    pub crf: u32,
    pub preset: String,
}

/// Re-encodes the kept intervals into one video. Cuts are frame accurate: the
/// source is decoded and the frames are selected by timestamp.
pub fn cut_video(
    p: &Project,
    classes: &[Class],
    opts: &EncodeOptions,
    out: &Path,
    progress: &crate::analysis::Progress,
) -> Result<()> {
    let keep = keep_intervals(p, classes, opts.mode);
    if keep.is_empty() {
        bail!("残る区間がない");
    }
    let total: f64 = keep.iter().map(|(a, b)| b - a).sum();
    progress.total.store((total * 1000.0) as usize, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    // Timestamps sit exactly on frame boundaries, so test against the middle of the
    // gap to be robust to rounding: a frame at pts t is kept iff a <= t < b.
    let half = 0.5 / p.info.avg_fps;
    let expr = keep
        .iter()
        .map(|(a, b)| format!("between(t,{:.6},{:.6})", a - half, b - half))
        .collect::<Vec<_>>()
        .join("+");
    let fps = p.info.avg_fps;
    // Constant frame rate sources go through untouched so that exactly the kept
    // frames come out. Variable frame rate sources are resampled to the average
    // rate first, since the output timestamps are rebuilt as N/FRAME_RATE.
    let resample = if is_cfr(&p.pts, fps) { String::new() } else { format!("fps={fps:.6},") };
    let mut graph = format!(
        "[0:v:0]{resample}select='{expr}',setpts=N/FRAME_RATE/TB[v]"
    );
    if p.info.has_audio {
        // Audio frames hold ~1024 samples; split them small so cuts land within ~1 ms.
        graph.push_str(&format!(
            ";[0:a:0]asetnsamples=n=32:p=0,aselect='{expr}',asetpts=N/SR/TB[a]"
        ));
    }

    let script = out.with_extension("filter.txt");
    std::fs::write(&script, &graph)?;
    let mut cmd = video::command("ffmpeg");
    cmd.args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(&p.video)
        .arg(if supports_file_option_prefix() { "-/filter_complex" } else { "-filter_complex_script" })
        .arg(&script)
        .args(["-map", "[v]"]);
    if p.info.has_audio {
        cmd.args(["-map", "[a]", "-c:a", "aac", "-b:a", "192k"]);
    }
    // select drops the stream's frame rate; without -r the encoder falls back to
    // 25 fps and silently discards frames.
    cmd.args(["-r", &format!("{fps:.6}"), "-fps_mode", "cfr"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", &opts.preset, "-crf", &opts.crf.to_string()])
        .args(["-progress", "pipe:1", "-nostats"])
        .arg(out)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("ffmpeg の起動に失敗")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut s);
        s
    });
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if progress.cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            break;
        }
        if let Some(v) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = v.trim().parse::<i64>() {
                progress.done.store((us.max(0) / 1000) as usize, Ordering::Relaxed);
            }
        }
    }
    let status = child.wait()?;
    let err = err_thread.join().unwrap_or_default();
    let _ = std::fs::remove_file(&script);
    if progress.cancel.load(Ordering::Relaxed) {
        bail!("キャンセルされた");
    }
    if !status.success() {
        bail!("ffmpeg failed: {err}");
    }
    Ok(())
}
