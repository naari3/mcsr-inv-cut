use crate::analysis::{self, AnalysisResult, Progress};
use crate::detect::{self, PanelRect, Roi};
use crate::export::{self, CutMode, EncodeOptions};
use crate::model::{Class, Project, Segment, Stats, fmt_time, segments, stats};
use crate::preview::{Preview, PreviewFrame, Source};
use eframe::egui::{self, Color32, Key, Pos2, Rect, Sense, Stroke, StrokeKind, pos2, vec2};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

const C_GAME: Color32 = Color32::from_rgb(70, 110, 70);
const C_CONTAINER: Color32 = Color32::from_rgb(230, 160, 40);
const C_OTHER: Color32 = Color32::from_rgb(90, 120, 220);

fn class_color(c: Class) -> Color32 {
    match c {
        Class::Game => C_GAME,
        Class::Container => C_CONTAINER,
        Class::OtherGui => C_OTHER,
    }
}

enum JobOutput {
    Opened(Project),
    Analyzed(AnalysisResult),
    Exported(PathBuf),
}

struct Job {
    label: &'static str,
    progress: Arc<Progress>,
    handle: Option<JoinHandle<anyhow::Result<JobOutput>>>,
}

pub struct App {
    project: Option<Project>,
    classes: Vec<Class>,
    stats: Stats,
    current: usize,
    requested: Option<usize>,
    preview: Preview,
    texture: Option<egui::TextureHandle>,
    shown: Option<PreviewFrame>,
    shown_panels: Vec<PanelRect>,
    shown_features: detect::Features,
    job: Option<Job>,
    view: (f64, f64),
    selection: Option<(usize, usize)>,
    drag_anchor: Option<usize>,
    roi_edit: bool,
    roi_drag: Option<Pos2>,
    segment_filter: Option<Class>,
    cut_mode: CutMode,
    crf: u32,
    preset: String,
    status: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_japanese_font(&cc.egui_ctx);
        Self {
            project: None,
            classes: Vec::new(),
            stats: Stats::default(),
            current: 0,
            requested: None,
            preview: Preview::new(cc.egui_ctx.clone()),
            texture: None,
            shown: None,
            shown_panels: Vec::new(),
            shown_features: Default::default(),
            job: None,
            view: (0.0, 1.0),
            selection: None,
            drag_anchor: None,
            roi_edit: false,
            roi_drag: None,
            segment_filter: Some(Class::Container),
            cut_mode: CutMode::RemoveContainer,
            crf: 18,
            preset: "fast".into(),
            status: String::new(),
        }
    }

    fn recompute(&mut self) {
        if let Some(p) = &self.project {
            self.classes = p.final_classes();
            self.stats = stats(p, &self.classes);
        }
    }

    fn save(&mut self) {
        if let Some(p) = &self.project {
            match p.save() {
                Ok(_) => {}
                Err(e) => self.status = format!("保存に失敗: {e:#}"),
            }
        }
    }

    pub fn open_video(&mut self, path: PathBuf) {
        if self.job.is_some() {
            return;
        }
        let pp = Project::project_path(&path);
        if pp.exists() {
            match Project::load(&pp) {
                Ok(mut p) => {
                    p.video = path;
                    self.set_project(p);
                    return;
                }
                Err(e) => self.status = format!("プロジェクトの読み込みに失敗、動画から開き直す: {e:#}"),
            }
        }
        let progress = Progress::new();
        let handle = std::thread::spawn(move || Ok(JobOutput::Opened(Project::from_video(&path)?)));
        self.job = Some(Job { label: "動画を読み込み中", progress, handle: Some(handle) });
    }

    fn set_project(&mut self, p: Project) {
        self.view = (p.pts[0], p.frame_end_time(p.frame_count() - 1));
        self.project = Some(p);
        self.current = 0;
        self.selection = None;
        self.shown = None;
        self.texture = None;
        self.requested = None;
        self.open_preview();
        self.recompute();
    }

    fn open_preview(&mut self) {
        let Some(p) = &self.project else { return };
        self.preview.open(Source {
            path: p.video.clone(),
            info: p.info.clone(),
            pts: Arc::new(p.pts.clone()),
            keyframes: Arc::new(p.keyframes.clone()),
        });
        self.requested = None;
    }

    fn start_analysis(&mut self) {
        let Some(p) = &self.project else { return };
        if self.job.is_some() {
            return;
        }
        let (path, info, roi) = (p.video.clone(), p.info.clone(), p.roi);
        let progress = Progress::new();
        let pr = progress.clone();
        let handle = std::thread::spawn(move || Ok(JobOutput::Analyzed(analysis::run(&path, &info, roi, &pr)?)));
        self.job = Some(Job { label: "解析中", progress, handle: Some(handle) });
    }

    fn start_export(&mut self, out: PathBuf) {
        let Some(p) = &self.project else { return };
        if self.job.is_some() {
            return;
        }
        let p = p.clone();
        let classes = self.classes.clone();
        let opts = EncodeOptions { mode: self.cut_mode, crf: self.crf, preset: self.preset.clone() };
        let progress = Progress::new();
        let pr = progress.clone();
        let handle = std::thread::spawn(move || {
            export::cut_video(&p, &classes, &opts, &out, &pr)?;
            Ok(JobOutput::Exported(out))
        });
        self.job = Some(Job { label: "書き出し中", progress, handle: Some(handle) });
    }

    fn poll_job(&mut self) {
        let finished = self.job.as_ref().is_some_and(|j| j.handle.as_ref().is_some_and(|h| h.is_finished()));
        if !finished {
            return;
        }
        let mut job = self.job.take().unwrap();
        match job.handle.take().unwrap().join() {
            Ok(Ok(JobOutput::Opened(p))) => {
                self.set_project(p);
            }
            Ok(Ok(JobOutput::Analyzed(r))) => {
                if let Some(p) = &mut self.project {
                    p.pts = r.pts;
                    p.features = r.features;
                    p.analyzed_roi = Some(p.roi);
                }
                self.open_preview();
                self.recompute();
                self.save();
            }
            Ok(Ok(JobOutput::Exported(path))) => self.status = format!("書き出し完了: {}", path.display()),
            Ok(Err(e)) => self.status = format!("{}: 失敗: {e:#}", job.label),
            Err(_) => self.status = format!("{}: パニック", job.label),
        }
    }

    fn seek(&mut self, i: usize) {
        if let Some(p) = &self.project {
            self.current = i.min(p.frame_count().saturating_sub(1));
        }
    }

    fn request_preview(&mut self) {
        let Some(p) = &self.project else { return };
        if self.requested == Some(self.current) {
            return;
        }
        let _ = p;
        self.requested = Some(self.current);
        self.preview.request(self.current);
    }

    fn receive_preview(&mut self, ctx: &egui::Context) {
        while let Ok(r) = self.preview.rx.try_recv() {
            match r {
                Ok(f) => {
                    let img = egui::ColorImage::from_rgb([f.w as usize, f.h as usize], &f.rgb);
                    match &mut self.texture {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None => {
                            self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR))
                        }
                    }
                    self.shown = Some(f);
                    self.update_shown_detection();
                }
                Err(e) => self.status = format!("プレビュー失敗: {e:#}"),
            }
        }
    }

    fn update_shown_detection(&mut self) {
        if let (Some(f), Some(p)) = (&self.shown, &self.project) {
            let (feat, panels) = detect::analyze_detail(&f.rgb, f.w, f.h, &p.roi);
            self.shown_features = feat;
            self.shown_panels = panels;
        }
    }

    fn segment_at(&self, i: usize) -> Option<Segment> {
        let p = self.project.as_ref()?;
        let c = *self.classes.get(i)?;
        let mut a = i;
        while a > 0 && self.classes[a - 1] == c {
            a -= 1;
        }
        let mut b = i;
        while b + 1 < p.frame_count() && self.classes[b + 1] == c {
            b += 1;
        }
        Some(Segment { start: a, end: b, class: c })
    }

    fn apply_override(&mut self, class: Option<Class>) {
        let Some((a, b)) = self.selection else { return };
        if let Some(p) = &mut self.project {
            p.set_override(a, b, class);
        }
        self.recompute();
        self.save();
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.project.is_none() {
            return;
        }
        let (left, right, shift, home, end) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.modifiers.shift,
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
            )
        });
        let n = self.project.as_ref().unwrap().frame_count();
        if left {
            if shift {
                let s = self.segment_at(self.current).unwrap();
                let target = if s.start == self.current { s.start.saturating_sub(1) } else { s.start };
                let target = self.segment_at(target).map(|x| if target == s.start { x.start } else { target }).unwrap();
                self.seek(target);
            } else {
                self.seek(self.current.saturating_sub(1));
            }
        }
        if right {
            if shift {
                let s = self.segment_at(self.current).unwrap();
                self.seek((s.end + 1).min(n - 1));
            } else {
                self.seek(self.current + 1);
            }
        }
        if home {
            self.seek(0);
        }
        if end {
            self.seek(n - 1);
        }
        let pressed = |k: Key| ctx.input(|i| i.key_pressed(k));
        if pressed(Key::Num1) {
            self.apply_override(Some(Class::Container));
        }
        if pressed(Key::Num2) {
            self.apply_override(Some(Class::OtherGui));
        }
        if pressed(Key::Num3) {
            self.apply_override(Some(Class::Game));
        }
        if pressed(Key::Num0) || pressed(Key::Delete) {
            self.apply_override(None);
        }
        if pressed(Key::Escape) {
            self.selection = None;
        }
        if pressed(Key::I) {
            self.set_range_edge(true);
        }
        if pressed(Key::O) {
            self.set_range_edge(false);
        }
    }

    fn set_range_edge(&mut self, start: bool) {
        let Some(p) = self.project.as_mut() else { return };
        let (a, b) = p.range();
        let c = self.current;
        p.range = Some(if start { (c, b.max(c)) } else { (a.min(c), c) });
        self.recompute();
        self.save();
    }

    fn pick_video(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("video", &["mp4", "mkv", "mov", "flv", "webm", "avi", "ts"])
            .pick_file()
        {
            self.open_video(path);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let idle = self.job.is_none();
            if ui.add_enabled(idle, egui::Button::new("動画を開く")).clicked() {
                self.pick_video();
            }
            let has = self.project.is_some();
            if ui.add_enabled(has && idle, egui::Button::new("保存")).clicked() {
                self.save();
            }
            ui.separator();
            if let Some(job) = &self.job {
                let p = &job.progress;
                let (d, t) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
                let frac = if t > 0 { d as f32 / t as f32 } else { 0.0 };
                ui.label(job.label);
                ui.add(egui::ProgressBar::new(frac).desired_width(240.0).show_percentage());
                if ui.button("キャンセル").clicked() {
                    p.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            ui.label(&self.status);
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let idle = self.job.is_none();
            let Some(p) = &mut self.project else { return };
            let mut changed = false;
            let mut roi_changed = false;

            ui.heading("解析");
            ui.label(format!("{}x{} / {:.3} fps (平均)", p.info.width, p.info.height, p.info.avg_fps));
            ui.label(format!("{} フレーム", p.frame_count()));
            ui.horizontal(|ui| {
                ui.toggle_value(&mut self.roi_edit, "解析範囲を描く").on_hover_text("ゲーム画面だけを囲う");
                if ui.button("解析範囲を全体に戻す").clicked() {
                    p.roi = Roi::default();
                    roi_changed = true;
                }
            });
            let analyze_label = if !p.analyzed() {
                "解析する"
            } else if p.analyzed_roi.is_some_and(|r| r != p.roi) {
                "解析範囲を反映して再解析"
            } else {
                "再解析"
            };
            let analyze_clicked = ui.add_enabled(idle, egui::Button::new(analyze_label)).clicked();

            ui.separator();
            ui.heading("判定");
            let s = &mut p.settings;
            changed |= ui
                .add(egui::Slider::new(&mut s.panel_threshold, 0.001..=0.2).logarithmic(true).text("パネル面積しきい値"))
                .changed();
            changed |= ui.checkbox(&mut s.detect_other_gui, "その他GUI を判定").changed();
            ui.add_enabled_ui(s.detect_other_gui, |ui| {
                changed |= ui
                    .add(
                        egui::Slider::new(&mut s.red_threshold, 0.000001..=0.01)
                            .logarithmic(true)
                            .text("ハート赤しきい値"),
                    )
                    .changed();
            });
            changed |= ui
                .add(egui::Slider::new(&mut s.fill_gap_frames, 0..=10).text("コンテナ間の穴埋め (フレーム)"))
                .changed();

            ui.separator();
            ui.heading("現在フレーム");
            let i = self.current;
            ui.label(format!("#{i}  {}", fmt_time(p.pts[i] - p.info.start_time)));
            if let Some(f) = p.features.get(i) {
                ui.label(format!("panel {:.4}  red {:.6}  bright {:.3}", f.panel, f.red, f.bright));
                let auto = p.auto_classes()[i];
                let fin = self.classes[i];
                ui.colored_label(class_color(fin), fin.label());
                if auto != fin {
                    ui.small(format!("✏ 手動修正 (自動判定は{})", auto.label()));
                }
            }

            ui.separator();
            ui.heading("計測");
            let (a, b) = p.range();
            ui.label(format!(
                "#{a} {} 〜 #{b} {}",
                fmt_time(p.pts[a] - p.info.start_time),
                fmt_time(p.frame_end_time(b) - p.info.start_time)
            ));
            let (mut range_start, mut range_end) = (false, false);
            ui.horizontal(|ui| {
                range_start = ui.button("ここから計測").on_hover_text("I").clicked();
                range_end = ui.button("ここまで計測").on_hover_text("O").clicked();
                if ui.button("範囲を全体に戻す").clicked() {
                    p.range = None;
                    changed = true;
                }
            });
            let st = &self.stats;
            let text = format!(
                "全体: {}\nコンテナGUI: {} ({} 回)\nコンテナGUI以外: {}\n  うちその他GUI: {}\n  うちゲーム画面: {}",
                fmt_time(st.total),
                fmt_time(st.container),
                st.container_count,
                fmt_time(st.non_container()),
                fmt_time(st.other_gui),
                fmt_time(st.game),
            );
            ui.label(egui::RichText::new(&text).monospace().size(15.0));
            if ui.button("結果をコピー").clicked() {
                ui.ctx().copy_text(text);
            }

            ui.separator();
            ui.heading("区間");
            ui.horizontal(|ui| {
                for (label, f) in [
                    ("コンテナ", Some(Class::Container)),
                    ("その他GUI", Some(Class::OtherGui)),
                    ("すべて", None),
                ] {
                    ui.selectable_value(&mut self.segment_filter, f, label);
                }
            });
            let segs: Vec<Segment> = segments(&self.classes, p.range())
                .into_iter()
                .filter(|s| self.segment_filter.is_none_or(|c| c == s.class))
                .collect();
            let mut seek_to = None;
            egui::ScrollArea::vertical().id_salt("segs").max_height(220.0).show_rows(
                ui,
                18.0,
                segs.len(),
                |ui, rows| {
                    for s in &segs[rows] {
                        let d = p.frame_end_time(s.end) - p.pts[s.start];
                        let txt = format!(
                            "{}  {}  {:>7.3}s  {}f",
                            fmt_time(p.pts[s.start] - p.info.start_time),
                            s.class.label(),
                            d,
                            s.end - s.start + 1
                        );
                        let sel = self.selection == Some((s.start, s.end));
                        if ui
                            .add(egui::Button::selectable(sel, egui::RichText::new(txt).monospace().color(class_color(s.class))))
                            .clicked()
                        {
                            seek_to = Some(*s);
                        }
                    }
                },
            );
            if let Some(s) = seek_to {
                self.selection = Some((s.start, s.end));
                self.current = s.start;
            }

            ui.separator();
            ui.heading("手動修正");
            let mut apply: Option<Option<Class>> = None;
            if let Some((a, b)) = self.selection {
                ui.label(format!("#{a} 〜 #{b} ({} フレーム)", b - a + 1));
            }
            ui.add_enabled_ui(self.selection.is_some(), |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (label, key, c) in [
                        ("コンテナGUIにする", "1", Some(Class::Container)),
                        ("その他GUIにする", "2", Some(Class::OtherGui)),
                        ("ゲーム画面にする", "3", Some(Class::Game)),
                        ("自動判定に戻す", "0", None),
                    ] {
                        if ui.button(label).on_hover_text(key).clicked() {
                            apply = Some(c);
                        }
                    }
                });
            })
            .response
            .on_disabled_hover_text("タイムラインを Shift+ドラッグ / ダブルクリック");
            let mut remove = None;
            for (k, o) in p.overrides.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.colored_label(class_color(o.class), format!("#{} 〜 #{}  {}", o.start, o.end, o.class.label()));
                    if ui.small_button("取り消す").clicked() {
                        remove = Some(k);
                    }
                });
            }
            if let Some(k) = remove {
                p.overrides.remove(k);
                changed = true;
            }

            ui.separator();
            ui.heading("書き出し");
            egui::ComboBox::from_label("カット")
                .selected_text(self.cut_mode.label())
                .show_ui(ui, |ui| {
                    for m in [CutMode::RemoveContainer, CutMode::RemoveAllGui, CutMode::OnlyContainer] {
                        ui.selectable_value(&mut self.cut_mode, m, m.label());
                    }
                });
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut self.crf).range(0..=51).prefix("CRF "));
                egui::ComboBox::from_id_salt("preset").selected_text(&self.preset).show_ui(ui, |ui| {
                    for x in ["ultrafast", "veryfast", "fast", "medium", "slow"] {
                        ui.selectable_value(&mut self.preset, x.to_string(), x);
                    }
                });
            });
            let mut export_video = None;
            let mut export_csv = None;
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("動画を書き出し")).clicked() {
                    export_video = rfd::FileDialog::new()
                        .set_file_name(default_name(&p.video, "_cut.mp4"))
                        .add_filter("mp4", &["mp4"])
                        .save_file();
                }
                if ui.button("CSV を書き出し").clicked() {
                    export_csv = rfd::FileDialog::new()
                        .set_file_name(default_name(&p.video, "_segments.csv"))
                        .add_filter("csv", &["csv"])
                        .save_file();
                }
            });

            if roi_changed {
                self.update_shown_detection();
            }
            if range_start || range_end {
                self.set_range_edge(range_start);
            }
            if let Some(c) = apply {
                self.apply_override(c);
            }
            if analyze_clicked {
                self.start_analysis();
            }
            if changed {
                self.recompute();
                self.save();
            }
            if let Some(out) = export_video {
                self.start_export(out);
            }
            if let Some(out) = export_csv {
                let p = self.project.as_ref().unwrap();
                self.status = match export::write_csv(p, &self.classes, &out) {
                    Ok(()) => format!("CSV を書き出した: {}", out.display()),
                    Err(e) => format!("CSV 書き出しに失敗: {e:#}"),
                };
            }
        });
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        let Some(tex) = &self.texture else {
            if self.project.is_none() && self.job.is_none() {
                ui.centered_and_justified(|ui| {
                    if ui.button(egui::RichText::new("動画を開く").size(20.0)).clicked() {
                        self.pick_video();
                    }
                });
            }
            return;
        };
        let ts = tex.size_vec2();
        let scale = (avail.x / ts.x).min(avail.y / ts.y);
        let size = ts * scale;
        let (rect, resp) = ui.allocate_exact_size(avail, Sense::drag());
        let img_rect = Rect::from_center_size(rect.center(), size);
        let painter = ui.painter_at(rect);
        painter.image(tex.id(), img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);

        let to_norm = |p: Pos2| {
            let v = (p - img_rect.min) / img_rect.size();
            pos2(v.x.clamp(0.0, 1.0), v.y.clamp(0.0, 1.0))
        };
        let from_norm = |x: f32, y: f32| img_rect.min + vec2(x, y) * img_rect.size();

        let Some(p) = &mut self.project else { return };
        let roi = p.roi;
        painter.rect_stroke(
            Rect::from_min_max(from_norm(roi.x0, roi.y0), from_norm(roi.x1, roi.y1)),
            0.0,
            Stroke::new(1.5, Color32::from_rgb(80, 200, 255)),
            StrokeKind::Inside,
        );
        if let Some(f) = &self.shown {
            for r in &self.shown_panels {
                let a = from_norm(r.x0 as f32 / f.w as f32, r.y0 as f32 / f.h as f32);
                let b = from_norm((r.x1 + 1) as f32 / f.w as f32, (r.y1 + 1) as f32 / f.h as f32);
                painter.rect_stroke(Rect::from_min_max(a, b), 0.0, Stroke::new(2.0, C_CONTAINER), StrokeKind::Outside);
            }
            if f.index != self.current {
                painter.text(rect.left_top() + vec2(8.0, 8.0), egui::Align2::LEFT_TOP, "読み込み中…", egui::FontId::proportional(16.0), Color32::YELLOW);
            }
        }

        if self.roi_edit {
            if resp.drag_started() {
                self.roi_drag = resp.interact_pointer_pos();
            }
            if let (Some(start), Some(now)) = (self.roi_drag, resp.interact_pointer_pos()) {
                painter.rect_stroke(Rect::from_two_pos(start, now), 0.0, Stroke::new(1.5, Color32::WHITE), StrokeKind::Inside);
                if resp.drag_stopped() {
                    let (a, b) = (to_norm(start), to_norm(now));
                    if (a.x - b.x).abs() > 0.02 && (a.y - b.y).abs() > 0.02 {
                        p.roi = Roi { x0: a.x.min(b.x), y0: a.y.min(b.y), x1: a.x.max(b.x), y1: a.y.max(b.y) };
                        self.roi_edit = false;
                    }
                    self.roi_drag = None;
                    self.update_shown_detection();
                }
            }
        }
    }

    fn timeline_ui(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &self.project else { return };
        let n = p.frame_count();
        let t_min = p.pts[0];
        let t_max = p.frame_end_time(n - 1);
        let width = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 150.0), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(20));

        // Zoom with the wheel around the pointer, pan with right/middle drag.
        let (mut v0, mut v1) = self.view;
        if let Some(hover) = resp.hover_pos() {
            let scroll = ui.input(|i| i.smooth_scroll_delta);
            let tp = v0 + (hover.x - rect.left()) as f64 / width as f64 * (v1 - v0);
            if scroll.y != 0.0 {
                let k = (-scroll.y as f64 / 200.0).exp();
                let len = ((v1 - v0) * k).clamp(30.0 / p.info.avg_fps, t_max - t_min);
                v0 = tp - (tp - v0) / (v1 - v0) * len;
                v1 = v0 + len;
            }
            if scroll.x != 0.0 {
                let d = -scroll.x as f64 / width as f64 * (v1 - v0);
                v0 += d;
                v1 += d;
            }
        }
        if resp.dragged_by(egui::PointerButton::Secondary) || resp.dragged_by(egui::PointerButton::Middle) {
            let d = -resp.drag_delta().x as f64 / width as f64 * (v1 - v0);
            v0 += d;
            v1 += d;
        }
        let len = v1 - v0;
        if v0 < t_min {
            (v0, v1) = (t_min, t_min + len);
        }
        if v1 > t_max {
            (v0, v1) = ((t_max - len).max(t_min), t_max);
        }
        self.view = (v0, v1);
        let x_of = |t: f64| rect.left() + ((t - v0) / (v1 - v0) * width as f64) as f32;
        let t_of = |x: f32| v0 + (x - rect.left()) as f64 / width as f64 * (v1 - v0);

        // Rows.
        let ruler = Rect::from_min_size(rect.min, vec2(width, 16.0));
        let auto_row = Rect::from_min_size(rect.min + vec2(0.0, 18.0), vec2(width, 8.0));
        let final_row = Rect::from_min_size(rect.min + vec2(0.0, 28.0), vec2(width, 26.0));
        let panel_row = Rect::from_min_size(rect.min + vec2(0.0, 58.0), vec2(width, 44.0));
        let red_row = Rect::from_min_size(rect.min + vec2(0.0, 106.0), vec2(width, 40.0));

        // Ruler ticks.
        let steps = [0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 300.0, 600.0];
        let step = *steps.iter().find(|s| (v1 - v0) / **s < width as f64 / 90.0).unwrap_or(&1800.0);
        let mut t = ((v0 - p.info.start_time) / step).ceil() * step + p.info.start_time;
        while t < v1 {
            let x = x_of(t);
            painter.line_segment([pos2(x, ruler.top()), pos2(x, ruler.bottom())], Stroke::new(1.0, Color32::GRAY));
            painter.text(pos2(x + 2.0, ruler.top()), egui::Align2::LEFT_TOP, fmt_time(t - p.info.start_time), egui::FontId::monospace(10.0), Color32::LIGHT_GRAY);
            t += step;
        }

        // Per pixel column summaries.
        let auto = p.auto_classes();
        let prio = |c: Class| match c {
            Class::Container => 2,
            Class::OtherGui => 1,
            Class::Game => 0,
        };
        let log_red = |r: f32| ((r.max(1e-6).log10() + 6.0) / 4.0).clamp(0.0, 1.0);
        let panel_norm = |v: f32| (v / 0.3).clamp(0.0, 1.0).sqrt();
        let cols = width.ceil() as usize;
        for col in 0..cols {
            let ct0 = t_of(rect.left() + col as f32);
            let ct1 = t_of(rect.left() + col as f32 + 1.0);
            if ct0 >= t_max {
                break;
            }
            // Every frame overlapping this column; a frame wider than a column
            // paints all the columns it covers.
            let fa = p.frame_at_time(ct0);
            let fb = p.frame_at_time(ct1).max(fa);
            let (mut ca, mut cf) = (auto[fa], self.classes[fa]);
            let (mut pmax, mut rmin) = (0.0f32, 1.0f32);
            for i in fa..=fb {
                if prio(auto[i]) > prio(ca) {
                    ca = auto[i];
                }
                if prio(self.classes[i]) > prio(cf) {
                    cf = self.classes[i];
                }
                if let Some(f) = p.features.get(i) {
                    pmax = pmax.max(f.panel);
                    rmin = rmin.min(f.red);
                }
            }
            let x = rect.left() + col as f32;
            let colr = |r: Rect| Rect::from_min_max(pos2(x, r.top()), pos2(x + 1.0, r.bottom()));
            painter.rect_filled(colr(auto_row), 0.0, class_color(ca).gamma_multiply(0.7));
            painter.rect_filled(colr(final_row), 0.0, class_color(cf));
            if p.analyzed() {
                let h = panel_norm(pmax) * panel_row.height();
                painter.rect_filled(Rect::from_min_max(pos2(x, panel_row.bottom() - h), pos2(x + 1.0, panel_row.bottom())), 0.0, C_CONTAINER.gamma_multiply(0.8));
                let h = log_red(rmin) * red_row.height();
                painter.rect_filled(Rect::from_min_max(pos2(x, red_row.bottom() - h), pos2(x + 1.0, red_row.bottom())), 0.0, Color32::from_rgb(200, 60, 60));
            }
        }
        // Threshold lines.
        let s = &p.settings;
        let y = panel_row.bottom() - panel_norm(s.panel_threshold) * panel_row.height();
        painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(1.0, Color32::WHITE));
        if s.detect_other_gui {
            let y = red_row.bottom() - log_red(s.red_threshold) * red_row.height();
            painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(1.0, Color32::WHITE));
        }
        painter.text(panel_row.left_top() + vec2(4.0, 0.0), egui::Align2::LEFT_TOP, "panel", egui::FontId::monospace(10.0), Color32::WHITE);
        painter.text(red_row.left_top() + vec2(4.0, 0.0), egui::Align2::LEFT_TOP, "heart red (log)", egui::FontId::monospace(10.0), Color32::WHITE);

        // Measured range shading.
        let (ra, rb) = p.range();
        let dim = Color32::from_black_alpha(150);
        let xa = x_of(p.pts[ra]);
        let xb = x_of(p.frame_end_time(rb));
        if xa > rect.left() {
            painter.rect_filled(Rect::from_min_max(rect.min, pos2(xa.min(rect.right()), rect.bottom())), 0.0, dim);
        }
        if xb < rect.right() {
            painter.rect_filled(Rect::from_min_max(pos2(xb.max(rect.left()), rect.top()), rect.max), 0.0, dim);
        }

        // Selection.
        if let Some((a, b)) = self.selection {
            let r = Rect::from_min_max(pos2(x_of(p.pts[a]), rect.top()), pos2(x_of(p.frame_end_time(b)).max(x_of(p.pts[a]) + 1.0), rect.bottom()));
            painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40));
            painter.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Inside);
        }

        // Playhead.
        let xc = x_of(p.pts[self.current]);
        let xe = x_of(p.frame_end_time(self.current)).max(xc + 1.0);
        painter.rect_filled(Rect::from_min_max(pos2(xc, rect.top()), pos2(xe, rect.bottom())), 0.0, Color32::from_rgba_unmultiplied(255, 60, 60, 160));

        // Interaction.
        let shift = ui.input(|i| i.modifiers.shift);
        let frame_at = |x: f32| p.frame_at_time(t_of(x));
        if let Some(pos) = resp.interact_pointer_pos() {
            let f = frame_at(pos.x);
            if resp.double_clicked() {
                if let Some(s) = self.segment_at(f) {
                    self.selection = Some((s.start, s.end));
                }
            } else if resp.drag_started_by(egui::PointerButton::Primary) || resp.clicked() {
                if shift {
                    self.drag_anchor = Some(f);
                    self.selection = Some((f, f));
                } else {
                    self.drag_anchor = None;
                }
                self.current = f;
            } else if resp.dragged_by(egui::PointerButton::Primary) {
                if let Some(a) = self.drag_anchor {
                    self.selection = Some((a.min(f), a.max(f)));
                }
                self.current = f;
            }
        }
        if resp.drag_stopped() {
            self.drag_anchor = None;
        }
        resp.on_hover_text(
            "🖱 ドラッグでシーク\n⇧ Shift+ドラッグで範囲選択\n🖱 ダブルクリックで区間を選択\n🔍 ホイールでズーム\n↔ 右ドラッグでスクロール\n⏵ ← → で 1 フレーム、Shift で区間の境界へ",
        );
    }
}

fn default_name(video: &Path, suffix: &str) -> String {
    let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{stem}{suffix}")
}

fn install_japanese_font(ctx: &egui::Context) {
    let dir = std::env::var_os("WINDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("Fonts");
    for name in ["YuGothM.ttc", "meiryo.ttc", "msgothic.ttc"] {
        if let Ok(data) = std::fs::read(dir.join(name)) {
            ctx.add_font(egui::epaint::text::FontInsert::new(
                "jp",
                // Japanese system fonts render ~0.28 em above center in egui's line
                // box (measured: 3.5 px high in a 23 px button at body size).
                egui::FontData::from_owned(data).tweak(egui::FontTweak { y_offset_factor: 0.28, ..Default::default() }),
                vec![
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Proportional,
                        priority: egui::epaint::text::FontPriority::Highest,
                    },
                    egui::epaint::text::InsertFontFamily {
                        family: egui::FontFamily::Monospace,
                        priority: egui::epaint::text::FontPriority::Lowest,
                    },
                ],
            ));
            return;
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job();
        self.receive_preview(&ctx);
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if let Some(path) = dropped.into_iter().next() {
            self.open_video(path);
        }
        self.handle_keys(&ctx);

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("timeline").resizable(false).show(ui, |ui| self.timeline_ui(ui));
        egui::Panel::right("side").default_size(360.0).show(ui, |ui| self.side_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.preview_ui(ui));
        self.request_preview();
    }
}
