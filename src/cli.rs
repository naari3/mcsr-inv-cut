//! Headless commands: `analyze`, `report`, `csv`, `cut`.

use crate::analysis::{self, Progress};
use crate::detect::Roi;
use crate::export::{self, CutMode, EncodeOptions};
use crate::model::{Class, Project, fmt_time, segments, stats};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

pub const USAGE: &str = "\
usage:
  mcsr-inv-cut [video]                         GUI を起動
  mcsr-inv-cut analyze <video> [--roi x0,y0,x1,y1]
  mcsr-inv-cut report <video>
  mcsr-inv-cut csv <video> <out.csv>
  mcsr-inv-cut cut <video> <out.mp4> [--mode remove-container|remove-gui|only-container] [--crf N] [--preset P]";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn with_progress<T: Send>(label: &str, f: impl FnOnce(&Progress) -> Result<T> + Send) -> Result<T> {
    let progress = Progress::new();
    std::thread::scope(|s| {
        let h = s.spawn(|| f(&progress));
        while !h.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let (d, t) = (progress.done.load(Ordering::Relaxed), progress.total.load(Ordering::Relaxed));
            if t > 0 {
                eprint!("\r{label}: {:5.1}%", d as f64 / t as f64 * 100.0);
            }
        }
        eprintln!();
        h.join().unwrap()
    })
}

pub fn run(args: &[String]) -> Result<()> {
    let cmd = args[0].as_str();
    let video = PathBuf::from(args.get(1).context(USAGE)?);
    match cmd {
        "analyze" => {
            let mut p = Project::open(&video)?;
            if let Some(r) = flag(args, "--roi") {
                let v: Vec<f32> = r.split(',').map(|x| x.trim().parse()).collect::<Result<_, _>>()?;
                anyhow::ensure!(v.len() == 4, "--roi は x0,y0,x1,y1 (0..1)");
                p.roi = Roi { x0: v[0], y0: v[1], x1: v[2], y1: v[3] };
            }
            let t = std::time::Instant::now();
            let r = with_progress("解析", |pr| analysis::run(&p.video, &p.info, p.roi, pr))?;
            p.pts = r.pts;
            p.features = r.features;
            p.analyzed_roi = Some(p.roi);
            let path = p.save()?;
            eprintln!("{} フレームを {:.1} 秒で解析: {}", p.frame_count(), t.elapsed().as_secs_f64(), path.display());
            print_report(&p);
        }
        "report" => print_report(&load_analyzed(&video)?),
        "csv" => {
            let p = load_analyzed(&video)?;
            let out = PathBuf::from(args.get(2).context(USAGE)?);
            export::write_csv(&p, &p.final_classes(), &out)?;
        }
        "cut" => {
            let p = load_analyzed(&video)?;
            let out = PathBuf::from(args.get(2).context(USAGE)?);
            let mode = match flag(args, "--mode").as_deref() {
                None | Some("remove-container") => CutMode::RemoveContainer,
                Some("remove-gui") => CutMode::RemoveAllGui,
                Some("only-container") => CutMode::OnlyContainer,
                Some(m) => bail!("unknown mode: {m}"),
            };
            let opts = EncodeOptions {
                mode,
                crf: flag(args, "--crf").map(|x| x.parse()).transpose()?.unwrap_or(18),
                preset: flag(args, "--preset").unwrap_or_else(|| "fast".into()),
            };
            let classes = p.final_classes();
            with_progress("書き出し", |pr| export::cut_video(&p, &classes, &opts, &out, pr))?;
            eprintln!("書き出した: {}", out.display());
        }
        _ => bail!("{USAGE}"),
    }
    Ok(())
}

fn load_analyzed(video: &std::path::Path) -> Result<Project> {
    let p = Project::open(video)?;
    anyhow::ensure!(p.analyzed(), "未解析。先に analyze を実行する");
    Ok(p)
}

fn print_report(p: &Project) {
    let classes = p.final_classes();
    let s = stats(p, &classes);
    println!("全体            {}", fmt_time(s.total));
    println!("コンテナGUI     {} ({} 回)", fmt_time(s.container), s.container_count);
    println!("コンテナGUI以外 {}", fmt_time(s.non_container()));
    println!("  その他GUI     {}", fmt_time(s.other_gui));
    println!("  ゲーム画面    {}", fmt_time(s.game));
    println!();
    for seg in segments(&classes, p.range()).iter().filter(|x| x.class == Class::Container) {
        println!(
            "{} - {}  {:7.3}s  ({}f)",
            fmt_time(p.pts[seg.start] - p.info.start_time),
            fmt_time(p.frame_end_time(seg.end) - p.info.start_time),
            p.frame_end_time(seg.end) - p.pts[seg.start],
            seg.end - seg.start + 1
        );
    }
}
