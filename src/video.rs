use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Hide console windows of spawned ffmpeg processes on Windows.
pub fn command(program: &str) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub start_time: f64,
    pub duration: f64,
    pub avg_fps: f64,
    pub has_audio: bool,
}

fn parse_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let n: f64 = n.parse().ok()?;
    let d: f64 = d.parse().ok()?;
    (d != 0.0).then(|| n / d)
}

pub fn probe(path: &Path) -> Result<VideoInfo> {
    let out = command("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .context("ffprobe の起動に失敗 (PATH に ffprobe があるか確認)")?;
    if !out.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let streams = v["streams"].as_array().context("no streams")?;
    let vs = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .context("映像ストリームがない")?;
    let has_audio = streams.iter().any(|s| s["codec_type"] == "audio");
    let num = |x: &serde_json::Value| x.as_str().and_then(|s| s.parse::<f64>().ok());
    let avg_fps = vs["avg_frame_rate"]
        .as_str()
        .and_then(parse_rate)
        .filter(|f| *f > 0.0)
        .or_else(|| vs["r_frame_rate"].as_str().and_then(parse_rate))
        .unwrap_or(60.0);
    Ok(VideoInfo {
        width: vs["width"].as_u64().and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
        height: vs["height"].as_u64().and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
        start_time: num(&v["format"]["start_time"]).unwrap_or(0.0),
        duration: num(&v["format"]["duration"]).unwrap_or(0.0),
        avg_fps,
        has_audio,
    })
}

/// Presentation timestamps (seconds, absolute) of every video frame, in display order.
/// Read from packets so no decoding is needed.
pub fn frame_pts(path: &Path) -> Result<Vec<f64>> {
    Ok(frame_index(path)?.0)
}

/// Frame timestamps in display order, and the indices of keyframes among them.
pub fn frame_index(path: &Path) -> Result<(Vec<f64>, Vec<usize>)> {
    let out = command("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,flags",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .context("ffprobe の起動に失敗")?;
    if !out.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let mut packets: Vec<(f64, bool)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.trim().split(',');
            let t = it.next()?.parse().ok()?;
            Some((t, it.next().is_some_and(|f| f.starts_with('K'))))
        })
        .collect();
    packets.sort_by(|a, b| a.0.total_cmp(&b.0));
    let keyframes = packets.iter().enumerate().filter(|(_, p)| p.1).map(|(i, _)| i).collect();
    Ok((packets.into_iter().map(|p| p.0).collect(), keyframes))
}

/// Output size of the analysis decode: width fixed, aspect kept, even height.
pub fn analysis_size(info: &VideoInfo, width: u32) -> (u32, u32) {
    let out_w = width.min(info.width.max(2)) & !1;
    let scaled = f64::from(info.height) * f64::from(out_w) / f64::from(info.width.max(1));
    (out_w, crate::num::to_u32(scaled.round()).max(2) & !1)
}

/// Streams every frame as packed RGB24 at the given size.
pub struct FrameReader {
    child: Child,
    stdout: ChildStdout,
    pub width: u32,
    pub height: u32,
}

impl FrameReader {
    /// Decodes from the start, or from `seek` seconds (relative to the start time;
    /// accurate, so the first output frame is the first one at or after it).
    pub fn open(path: &Path, width: u32, height: u32, seek: Option<f64>) -> Result<Self> {
        let mut cmd = command("ffmpeg");
        cmd.args(["-v", "error", "-nostdin"]);
        if let Some(ss) = seek {
            cmd.args(["-ss", &format!("{:.6}", ss.max(0.0))]);
        }
        let mut child = cmd
            .arg("-i")
            .arg(path)
            .args([
                "-map",
                "0:v:0",
                "-fps_mode",
                "passthrough",
                "-vf",
                &format!("scale={width}:{height}:flags=area"),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("ffmpeg の起動に失敗 (PATH に ffmpeg があるか確認)")?;
        let stdout = child.stdout.take().unwrap();
        Ok(Self { child, stdout, width, height })
    }

    /// Fills `buf` with the next frame. Returns false at end of stream.
    pub fn next_frame(&mut self, buf: &mut Vec<u8>) -> Result<bool> {
        let n = (self.width * self.height * 3) as usize;
        buf.resize(n, 0);
        let mut got = 0;
        while got < n {
            let r = self.stdout.read(&mut buf[got..])?;
            if r == 0 {
                return Ok(false);
            }
            got += r;
        }
        Ok(true)
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Decodes the single frame whose pts is `pts` (absolute seconds).
pub fn grab_frame(path: &Path, info: &VideoInfo, pts: f64, width: u32, height: u32) -> Result<Vec<u8>> {
    // Input seeking is relative to the start time and is accurate (decodes from the
    // previous keyframe and drops frames before the target). Back off slightly so
    // float rounding never skips the target frame.
    let ss = (pts - info.start_time - 0.0005).max(0.0);
    let out = command("ffmpeg")
        .args(["-v", "error", "-nostdin", "-ss", &format!("{ss:.6}"), "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            &format!("scale={width}:{height}:flags=bilinear"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .output()
        .context("ffmpeg の起動に失敗")?;
    let n = (width * height * 3) as usize;
    if out.stdout.len() < n {
        bail!("frame decode failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout[..n].to_vec())
}
