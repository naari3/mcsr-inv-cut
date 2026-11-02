use crate::analysis::{self, AnalysisResult, Progress};
use crate::detect::{self, PanelRect, Roi};
use crate::export::{self, CutMode, EncodeOptions};
use crate::model::{Class, Project, Segment, Stats, fmt_time, segments, stats};
use crate::num::{f32_from, f32_from_f64, ratio, to_usize};
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
    Opened(Box<Project>),
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
            shown_features: detect::Features::default(),
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
        let handle = std::thread::spawn(move || Ok(JobOutput::Opened(Box::new(Project::from_video(&path)?))));
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
        let finished = self.job.as_ref().is_some_and(|j| j.handle.as_ref().is_some_and(std::thread::JoinHandle::is_finished));
        if !finished {
            return;
        }
        let mut job = self.job.take().unwrap();
        match job.handle.take().unwrap().join() {
            Ok(Ok(JobOutput::Opened(p))) => {
                self.set_project(*p);
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
                            self.texture = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR));
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

    fn segment_at(&self, frame: usize) -> Option<Segment> {
        let class = *self.classes.get(frame)?;
        let start = self.classes[..frame].iter().rposition(|&other| other != class).map_or(0, |pos| pos + 1);
        let end = self.classes[frame..].iter().position(|&other| other != class).map_or(self.classes.len(), |pos| frame + pos) - 1;
        Some(Segment { start, end, class })
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
                let progress = &job.progress;
                let done = progress.done.load(Ordering::Relaxed);
                let total = progress.total.load(Ordering::Relaxed);
                ui.label(job.label);
                ui.add(egui::ProgressBar::new(ratio(done, total)).desired_width(240.0).show_percentage());
                if ui.button("キャンセル").clicked() {
                    progress.cancel.store(true, Ordering::Relaxed);
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            ui.label(&self.status);
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            if self.project.is_none() {
                return;
            }
            self.analysis_section(ui);
            ui.separator();
            self.settings_section(ui);
            ui.separator();
            self.frame_section(ui);
            ui.separator();
            self.measure_section(ui);
            ui.separator();
            self.segments_section(ui);
            ui.separator();
            self.corrections_section(ui);
            ui.separator();
            self.export_section(ui);
        });
    }

    fn changed(&mut self) {
        self.recompute();
        self.save();
    }

    fn analysis_section(&mut self, ui: &mut egui::Ui) {
        let idle = self.job.is_none();
        let Some(p) = &mut self.project else { return };
        ui.heading("解析");
        ui.label(format!("{}x{} / {:.3} fps (平均)", p.info.width, p.info.height, p.info.avg_fps));
        ui.label(format!("{} フレーム", p.frame_count()));
        let mut roi_reset = false;
        ui.horizontal(|ui| {
            ui.toggle_value(&mut self.roi_edit, "解析範囲を描く").on_hover_text("ゲーム画面だけを囲う");
            if ui.button("解析範囲を全体に戻す").clicked() {
                p.roi = Roi::default();
                roi_reset = true;
            }
        });
        let analyze_label = if !p.analyzed() {
            "解析する"
        } else if p.analyzed_roi.is_some_and(|roi| roi != p.roi) {
            "解析範囲を反映して再解析"
        } else {
            "再解析"
        };
        let analyze = ui.add_enabled(idle, egui::Button::new(analyze_label)).clicked();
        if roi_reset {
            self.update_shown_detection();
        }
        if analyze {
            self.start_analysis();
        }
    }

    fn settings_section(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &mut self.project else { return };
        let settings = &mut p.settings;
        let mut changed = false;
        ui.heading("判定");
        changed |= ui
            .add(
                egui::Slider::new(&mut settings.panel_threshold, 0.001..=0.2)
                    .logarithmic(true)
                    .text("パネル面積しきい値"),
            )
            .changed();
        changed |= ui.checkbox(&mut settings.detect_other_gui, "その他GUI を判定").changed();
        ui.add_enabled_ui(settings.detect_other_gui, |ui| {
            changed |= ui
                .add(
                    egui::Slider::new(&mut settings.red_threshold, 0.000_001..=0.01)
                        .logarithmic(true)
                        .text("ハート赤しきい値"),
                )
                .changed();
        });
        changed |= ui
            .add(egui::Slider::new(&mut settings.fill_gap_frames, 0..=10).text("コンテナ間の穴埋め (フレーム)"))
            .changed();
        if changed {
            self.changed();
        }
    }

    fn frame_section(&self, ui: &mut egui::Ui) {
        let Some(p) = &self.project else { return };
        let idx = self.current;
        ui.heading("現在フレーム");
        ui.label(format!("#{idx}  {}", fmt_time(p.pts[idx] - p.info.start_time)));
        if let Some(feat) = p.features.get(idx) {
            ui.label(format!("panel {:.4}  red {:.6}  bright {:.3}", feat.panel, feat.red, feat.bright));
            let auto = p.auto_classes()[idx];
            let fin = self.classes[idx];
            ui.colored_label(class_color(fin), fin.label());
            if auto != fin {
                ui.small(format!("✏ 手動修正 (自動判定は{})", auto.label()));
            }
        }
    }

    fn measure_section(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &mut self.project else { return };
        ui.heading("計測");
        let (first, last) = p.range();
        ui.label(format!(
            "#{first} {} 〜 #{last} {}",
            fmt_time(p.pts[first] - p.info.start_time),
            fmt_time(p.frame_end_time(last) - p.info.start_time)
        ));
        let (mut set_start, mut set_end, mut reset) = (false, false, false);
        ui.horizontal(|ui| {
            set_start = ui.button("ここから計測").on_hover_text("I").clicked();
            set_end = ui.button("ここまで計測").on_hover_text("O").clicked();
            reset = ui.button("範囲を全体に戻す").clicked();
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
        if reset {
            p.range = None;
            self.changed();
        } else if set_start || set_end {
            self.set_range_edge(set_start);
        }
    }

    fn segments_section(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &self.project else { return };
        ui.heading("区間");
        ui.horizontal(|ui| {
            for (label, filter) in [
                ("コンテナ", Some(Class::Container)),
                ("その他GUI", Some(Class::OtherGui)),
                ("すべて", None),
            ] {
                ui.selectable_value(&mut self.segment_filter, filter, label);
            }
        });
        let segs: Vec<Segment> = segments(&self.classes, p.range())
            .into_iter()
            .filter(|seg| self.segment_filter.is_none_or(|class| class == seg.class))
            .collect();
        let mut picked = None;
        egui::ScrollArea::vertical().id_salt("segs").max_height(220.0).show_rows(ui, 18.0, segs.len(), |ui, rows| {
            for seg in &segs[rows] {
                let txt = format!(
                    "{}  {}  {:>7.3}s  {}f",
                    fmt_time(p.pts[seg.start] - p.info.start_time),
                    seg.class.label(),
                    p.frame_end_time(seg.end) - p.pts[seg.start],
                    seg.end - seg.start + 1
                );
                let selected = self.selection == Some((seg.start, seg.end));
                let label = egui::RichText::new(txt).monospace().color(class_color(seg.class));
                if ui.add(egui::Button::selectable(selected, label)).clicked() {
                    picked = Some(*seg);
                }
            }
        });
        if let Some(seg) = picked {
            self.selection = Some((seg.start, seg.end));
            self.current = seg.start;
        }
    }

    fn corrections_section(&mut self, ui: &mut egui::Ui) {
        let Some(p) = &mut self.project else { return };
        ui.heading("手動修正");
        if let Some((first, last)) = self.selection {
            ui.label(format!("#{first} 〜 #{last} ({} フレーム)", last - first + 1));
        }
        let mut apply: Option<Option<Class>> = None;
        ui.add_enabled_ui(self.selection.is_some(), |ui| {
            ui.horizontal_wrapped(|ui| {
                for (label, key, class) in [
                    ("コンテナGUIにする", "1", Some(Class::Container)),
                    ("その他GUIにする", "2", Some(Class::OtherGui)),
                    ("ゲーム画面にする", "3", Some(Class::Game)),
                    ("自動判定に戻す", "0", None),
                ] {
                    if ui.button(label).on_hover_text(key).clicked() {
                        apply = Some(class);
                    }
                }
            });
        })
        .response
        .on_disabled_hover_text("タイムラインを Shift+ドラッグ / ダブルクリック");
        let mut remove = None;
        for (pos, o) in p.overrides.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.colored_label(class_color(o.class), format!("#{} 〜 #{}  {}", o.start, o.end, o.class.label()));
                if ui.small_button("取り消す").clicked() {
                    remove = Some(pos);
                }
            });
        }
        if let Some(pos) = remove {
            p.overrides.remove(pos);
            self.changed();
        }
        if let Some(class) = apply {
            self.apply_override(class);
        }
    }

    fn export_section(&mut self, ui: &mut egui::Ui) {
        let idle = self.job.is_none();
        let Some(p) = &self.project else { return };
        ui.heading("書き出し");
        egui::ComboBox::from_label("カット").selected_text(self.cut_mode.label()).show_ui(ui, |ui| {
            for mode in [CutMode::RemoveContainer, CutMode::RemoveAllGui, CutMode::OnlyContainer] {
                ui.selectable_value(&mut self.cut_mode, mode, mode.label());
            }
        });
        ui.horizontal(|ui| {
            ui.add(egui::DragValue::new(&mut self.crf).range(0..=51).prefix("CRF "));
            egui::ComboBox::from_id_salt("preset").selected_text(&self.preset).show_ui(ui, |ui| {
                for preset in ["ultrafast", "veryfast", "fast", "medium", "slow"] {
                    ui.selectable_value(&mut self.preset, preset.to_string(), preset);
                }
            });
        });
        let (mut export_video, mut export_csv) = (None, None);
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
        if let Some(out) = export_csv {
            self.status = match export::write_csv(p, &self.classes, &out) {
                Ok(()) => format!("CSV を書き出した: {}", out.display()),
                Err(e) => format!("CSV 書き出しに失敗: {e:#}"),
            };
        }
        if let Some(out) = export_video {
            self.start_export(out);
        }
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
        let tex_size = tex.size_vec2();
        let scale = (avail.x / tex_size.x).min(avail.y / tex_size.y);
        let (rect, resp) = ui.allocate_exact_size(avail, Sense::drag());
        let img_rect = Rect::from_center_size(rect.center(), tex_size * scale);
        let painter = ui.painter_at(rect);
        painter.image(tex.id(), img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);

        let to_norm = |pos: Pos2| {
            let rel = (pos - img_rect.min) / img_rect.size();
            pos2(rel.x.clamp(0.0, 1.0), rel.y.clamp(0.0, 1.0))
        };
        let from_norm = |nx: f32, ny: f32| img_rect.min + vec2(nx, ny) * img_rect.size();

        let Some(p) = &mut self.project else { return };
        let roi = p.roi;
        painter.rect_stroke(
            Rect::from_min_max(from_norm(roi.x0, roi.y0), from_norm(roi.x1, roi.y1)),
            0.0,
            Stroke::new(1.5, Color32::from_rgb(80, 200, 255)),
            StrokeKind::Inside,
        );
        if let Some(frame) = &self.shown {
            let (fw, fh) = (f32_from(frame.w as usize), f32_from(frame.h as usize));
            for panel in &self.shown_panels {
                let top_left = from_norm(f32_from(panel.x0) / fw, f32_from(panel.y0) / fh);
                let bottom_right = from_norm(f32_from(panel.x1 + 1) / fw, f32_from(panel.y1 + 1) / fh);
                let stroke = Stroke::new(2.0, C_CONTAINER);
                painter.rect_stroke(Rect::from_min_max(top_left, bottom_right), 0.0, stroke, StrokeKind::Outside);
            }
            if frame.index != self.current {
                let font = egui::FontId::proportional(16.0);
                painter.text(rect.left_top() + vec2(8.0, 8.0), egui::Align2::LEFT_TOP, "読み込み中…", font, Color32::YELLOW);
            }
        }

        if self.roi_edit {
            if resp.drag_started() {
                self.roi_drag = resp.interact_pointer_pos();
            }
            if let (Some(start), Some(now)) = (self.roi_drag, resp.interact_pointer_pos()) {
                let stroke = Stroke::new(1.5, Color32::WHITE);
                painter.rect_stroke(Rect::from_two_pos(start, now), 0.0, stroke, StrokeKind::Inside);
                if resp.drag_stopped() {
                    let (from, to) = (to_norm(start), to_norm(now));
                    if (from.x - to.x).abs() > 0.02 && (from.y - to.y).abs() > 0.02 {
                        p.roi = Roi { x0: from.x.min(to.x), y0: from.y.min(to.y), x1: from.x.max(to.x), y1: from.y.max(to.y) };
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
        let width = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 150.0), Sense::click_and_drag());
        let t_min = p.pts[0];
        let t_max = p.frame_end_time(p.frame_count() - 1);
        self.view = zoom_and_pan(ui, &resp, rect, self.view, (t_min, t_max), 30.0 / p.info.avg_fps);
        let axis = TimeAxis { left: rect.left(), width, start: self.view.0, end: self.view.1 };
        self.paint_timeline(&ui.painter_at(rect), rect, &axis);
        self.timeline_input(ui, &resp, &axis);
        resp.on_hover_text(
            "🖱 ドラッグでシーク\n⇧ Shift+ドラッグで範囲選択\n🖱 ダブルクリックで区間を選択\n🔍 ホイールでズーム\n↔ 右ドラッグでスクロール\n⏵ ← → で 1 フレーム、Shift で区間の境界へ",
        );
    }

    fn paint_timeline(&self, painter: &egui::Painter, rect: Rect, axis: &TimeAxis) {
        let Some(p) = &self.project else { return };
        painter.rect_filled(rect, 0.0, Color32::from_gray(20));
        let row = |top: f32, height: f32| Rect::from_min_size(rect.min + vec2(0.0, top), vec2(axis.width, height));
        let (ruler, auto_row, final_row) = (row(0.0, 16.0), row(18.0, 8.0), row(28.0, 26.0));
        let (panel_row, red_row) = (row(58.0, 44.0), row(106.0, 40.0));

        // Ruler ticks.
        let steps = [0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 300.0, 600.0];
        let span = axis.end - axis.start;
        let step = steps.iter().copied().find(|s| span / s < f64::from(axis.width) / 90.0).unwrap_or(1800.0);
        let mut tick = ((axis.start - p.info.start_time) / step).ceil() * step + p.info.start_time;
        while tick < axis.end {
            let x = axis.x_of(tick);
            painter.line_segment([pos2(x, ruler.top()), pos2(x, ruler.bottom())], Stroke::new(1.0, Color32::GRAY));
            let label = fmt_time(tick - p.info.start_time);
            painter.text(pos2(x + 2.0, ruler.top()), egui::Align2::LEFT_TOP, label, egui::FontId::monospace(10.0), Color32::LIGHT_GRAY);
            tick += step;
        }

        // Per pixel column summaries.
        let auto = p.auto_classes();
        let prio = |class: Class| match class {
            Class::Container => 2,
            Class::OtherGui => 1,
            Class::Game => 0,
        };
        let log_red = |red: f32| ((red.max(1e-6).log10() + 6.0) / 4.0).clamp(0.0, 1.0);
        let panel_norm = |panel: f32| (panel / 0.3).clamp(0.0, 1.0).sqrt();
        let t_max = p.frame_end_time(p.frame_count() - 1);
        let bar = |row: Rect, x: f32, frac: f32| Rect::from_min_max(pos2(x, row.bottom() - frac * row.height()), pos2(x + 1.0, row.bottom()));
        for col in 0..to_usize(f64::from(axis.width).ceil()) {
            let x = rect.left() + f32_from(col);
            let (col_start, col_end) = (axis.t_of(x), axis.t_of(x + 1.0));
            if col_start >= t_max {
                break;
            }
            // Every frame overlapping this column; a frame wider than a column
            // paints all the columns it covers.
            let first = p.frame_at_time(col_start);
            let last = p.frame_at_time(col_end).max(first);
            let (mut auto_class, mut final_class) = (auto[first], self.classes[first]);
            let (mut panel_max, mut red_min) = (0.0f32, 1.0f32);
            for (idx, (&a_class, &f_class)) in auto.iter().zip(&self.classes).enumerate().take(last + 1).skip(first) {
                if prio(a_class) > prio(auto_class) {
                    auto_class = a_class;
                }
                if prio(f_class) > prio(final_class) {
                    final_class = f_class;
                }
                if let Some(feat) = p.features.get(idx) {
                    panel_max = panel_max.max(feat.panel);
                    red_min = red_min.min(feat.red);
                }
            }
            painter.rect_filled(bar(auto_row, x, 1.0), 0.0, class_color(auto_class).gamma_multiply(0.7));
            painter.rect_filled(bar(final_row, x, 1.0), 0.0, class_color(final_class));
            if p.analyzed() {
                painter.rect_filled(bar(panel_row, x, panel_norm(panel_max)), 0.0, C_CONTAINER.gamma_multiply(0.8));
                painter.rect_filled(bar(red_row, x, log_red(red_min)), 0.0, Color32::from_rgb(200, 60, 60));
            }
        }

        // Threshold lines.
        let settings = &p.settings;
        let hline = |y: f32| painter.line_segment([pos2(rect.left(), y), pos2(rect.right(), y)], Stroke::new(1.0, Color32::WHITE));
        hline(panel_row.bottom() - panel_norm(settings.panel_threshold) * panel_row.height());
        if settings.detect_other_gui {
            hline(red_row.bottom() - log_red(settings.red_threshold) * red_row.height());
        }
        let small = egui::FontId::monospace(10.0);
        painter.text(panel_row.left_top() + vec2(4.0, 0.0), egui::Align2::LEFT_TOP, "panel", small.clone(), Color32::WHITE);
        painter.text(red_row.left_top() + vec2(4.0, 0.0), egui::Align2::LEFT_TOP, "heart red (log)", small, Color32::WHITE);

        // Measured range shading.
        let (first, last) = p.range();
        let dim = Color32::from_black_alpha(150);
        let x_first = axis.x_of(p.pts[first]);
        let x_last = axis.x_of(p.frame_end_time(last));
        if x_first > rect.left() {
            painter.rect_filled(Rect::from_min_max(rect.min, pos2(x_first.min(rect.right()), rect.bottom())), 0.0, dim);
        }
        if x_last < rect.right() {
            painter.rect_filled(Rect::from_min_max(pos2(x_last.max(rect.left()), rect.top()), rect.max), 0.0, dim);
        }

        // Selection.
        if let Some((sel_first, sel_last)) = self.selection {
            let x_start = axis.x_of(p.pts[sel_first]);
            let x_end = axis.x_of(p.frame_end_time(sel_last)).max(x_start + 1.0);
            let sel = Rect::from_min_max(pos2(x_start, rect.top()), pos2(x_end, rect.bottom()));
            painter.rect_filled(sel, 0.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40));
            painter.rect_stroke(sel, 0.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Inside);
        }

        // Playhead.
        let x_cur = axis.x_of(p.pts[self.current]);
        let x_next = axis.x_of(p.frame_end_time(self.current)).max(x_cur + 1.0);
        let head = Rect::from_min_max(pos2(x_cur, rect.top()), pos2(x_next, rect.bottom()));
        painter.rect_filled(head, 0.0, Color32::from_rgba_unmultiplied(255, 60, 60, 160));
    }

    fn timeline_input(&mut self, ui: &egui::Ui, resp: &egui::Response, axis: &TimeAxis) {
        let Some(p) = &self.project else { return };
        let shift = ui.input(|input| input.modifiers.shift);
        if let Some(pos) = resp.interact_pointer_pos() {
            let frame = p.frame_at_time(axis.t_of(pos.x));
            if resp.double_clicked() {
                if let Some(seg) = self.segment_at(frame) {
                    self.selection = Some((seg.start, seg.end));
                }
            } else if resp.drag_started_by(egui::PointerButton::Primary) || resp.clicked() {
                if shift {
                    self.drag_anchor = Some(frame);
                    self.selection = Some((frame, frame));
                } else {
                    self.drag_anchor = None;
                }
                self.current = frame;
            } else if resp.dragged_by(egui::PointerButton::Primary) {
                if let Some(anchor) = self.drag_anchor {
                    self.selection = Some((anchor.min(frame), anchor.max(frame)));
                }
                self.current = frame;
            }
        }
        if resp.drag_stopped() {
            self.drag_anchor = None;
        }
    }
}

/// Maps between timeline time (seconds) and screen x.
struct TimeAxis {
    left: f32,
    width: f32,
    start: f64,
    end: f64,
}

impl TimeAxis {
    fn x_of(&self, t: f64) -> f32 {
        self.left + f32_from_f64((t - self.start) / (self.end - self.start) * f64::from(self.width))
    }

    fn t_of(&self, x: f32) -> f64 {
        self.start + f64::from(x - self.left) / f64::from(self.width) * (self.end - self.start)
    }
}

/// Zooms with the wheel around the pointer and pans with horizontal scroll or
/// right/middle drag. Returns the new visible time range, kept within `bounds`.
fn zoom_and_pan(
    ui: &egui::Ui,
    resp: &egui::Response,
    rect: Rect,
    (mut start, mut end): (f64, f64),
    (t_min, t_max): (f64, f64),
    min_span: f64,
) -> (f64, f64) {
    let width = f64::from(rect.width());
    if let Some(hover) = resp.hover_pos() {
        let scroll = ui.input(|input| input.smooth_scroll_delta);
        let pointer = start + f64::from(hover.x - rect.left()) / width * (end - start);
        if scroll.y != 0.0 {
            let factor = (f64::from(-scroll.y) / 200.0).exp();
            let span = ((end - start) * factor).clamp(min_span, t_max - t_min);
            start = pointer - (pointer - start) / (end - start) * span;
            end = start + span;
        }
        if scroll.x != 0.0 {
            let shift = f64::from(-scroll.x) / width * (end - start);
            start += shift;
            end += shift;
        }
    }
    if resp.dragged_by(egui::PointerButton::Secondary) || resp.dragged_by(egui::PointerButton::Middle) {
        let shift = f64::from(-resp.drag_delta().x) / width * (end - start);
        start += shift;
        end += shift;
    }
    let span = end - start;
    if start < t_min {
        (start, end) = (t_min, t_min + span);
    }
    if end > t_max {
        (start, end) = ((t_max - span).max(t_min), t_max);
    }
    (start, end)
}

fn default_name(video: &Path, suffix: &str) -> String {
    let stem = video.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{stem}{suffix}")
}

fn install_japanese_font(ctx: &egui::Context) {
    let dir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("Fonts");
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
