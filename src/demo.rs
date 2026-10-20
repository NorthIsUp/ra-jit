//! `ra-jit life`: a window with the running Game of Life (left) and a source editor (right).
//! Every edit in the editor is written to disk, ra-jit's watcher recompiles the changed
//! functions and the next frame uses them; the header shows how long that took.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::theme::{self, Theme};

const MAXW: usize = 128;
const MAXH: usize = 128;

static PIX: [AtomicU32; MAXW * MAXH] = [const { AtomicU32::new(0) }; MAXW * MAXH];
static DIM: AtomicU32 = AtomicU32::new(0);
static FRAMES: AtomicU64 = AtomicU64::new(0);

/// Guest `set_cell(x, y, 0xRRGGBB)`.
pub extern "C" fn host_set_cell(x: i64, y: i64, rgb: i64) {
    if (0..MAXW as i64).contains(&x) && (0..MAXH as i64).contains(&y) {
        PIX[y as usize * MAXW + x as usize].store(rgb as u32, Ordering::Relaxed);
    }
}

/// Guest `present(w, h)`: the frame is complete, show a w x h picture.
pub extern "C" fn host_present(w: i64, h: i64) {
    let (w, h) = (w.clamp(1, MAXW as i64) as u32, h.clamp(1, MAXH as i64) as u32);
    DIM.store(w << 16 | h, Ordering::Relaxed);
    FRAMES.fetch_add(1, Ordering::Relaxed);
}

#[derive(Default)]
pub struct Status {
    pub cold_ms: f64,
    pub edits: u64,
    /// keystroke (or file write) to new code running, ms
    pub last_ms: f64,
    /// wall time of the `cargo build` that gated the last edit (rustc mode), ms
    pub cargo_ms: f64,
    pub recompiled: Vec<String>,
    pub total_fns: usize,
    pub error: String,
    /// Last host note, e.g. "state reset: layout changed".
    pub note: String,
}

/// "rustc mode": before applying an edit the watcher runs a real `cargo build` of the guest and waits for it.
pub static RUSTC_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static WARM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub static STATUS: Mutex<Option<Status>> = Mutex::new(None);
/// Set by the editor right before it writes the file; taken by the watcher after the swap.
pub static EDIT_START: Mutex<Option<Instant>> = Mutex::new(None);

/// Virtual guest clock for the offline recorder (`src/record.rs`): `sleep_ms` then waits for the recorder to advance
/// time instead of sleeping, so simulation speed is a fixed step per video frame and the clip is reproducible.
pub static VIRTUAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static VT_US: AtomicU64 = AtomicU64::new(0);
/// Virtual wake time of the guest while it is parked in `sleep_ms` (0 = running).
static PARK_WAKE_US: AtomicU64 = AtomicU64::new(0);
static GUEST_VT_US: AtomicU64 = AtomicU64::new(0);
static VT_LOCK: Mutex<()> = Mutex::new(());
static VT_CV: std::sync::Condvar = std::sync::Condvar::new();

/// Guest `sleep_ms(ms)`.
pub extern "C" fn host_sleep_ms(ms: i64) {
    let ms = ms.max(0) as u64;
    if !VIRTUAL.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(ms));
        return;
    }
    let wake = GUEST_VT_US.fetch_add(ms * 1000, Ordering::Relaxed) + ms * 1000;
    let mut g = VT_LOCK.lock().unwrap();
    while VT_US.load(Ordering::Relaxed) < wake {
        PARK_WAKE_US.store(wake, Ordering::Release);
        g = VT_CV.wait(g).unwrap();
    }
    PARK_WAKE_US.store(0, Ordering::Release);
}

/// Recorder: advance virtual time by `us` and wait until the guest has run every frame due by then and parked again.
pub fn advance_virtual(us: u64) -> bool {
    let now = {
        let _g = VT_LOCK.lock().unwrap();
        let now = VT_US.fetch_add(us, Ordering::Relaxed) + us;
        VT_CV.notify_all();
        now
    };
    let t = Instant::now();
    while PARK_WAKE_US.load(Ordering::Acquire) <= now {
        if t.elapsed() > Duration::from_secs(5) {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

pub fn frames_presented() -> u64 {
    FRAMES.load(Ordering::Relaxed)
}

pub fn with_status(f: impl FnOnce(&mut Status)) {
    let mut g = STATUS.lock().unwrap();
    f(g.get_or_insert_with(Status::default));
}

pub fn take_edit_start() -> Option<Instant> {
    EDIT_START.lock().unwrap().take()
}

pub(crate) struct App {
    file: PathBuf,
    text: String,
    on_disk: String,
    last_edit: Instant,
    last_poll: Instant,
    tex: Option<egui::TextureHandle>,
    fps_at: f64,
    start: Instant,
    fps_frames: u64,
    fps: f64,
    hl: crate::highlight::Highlighter,
    theme: &'static Theme,
    focused_once: bool,
    launch_frames: u8,
    /// Recorder: select this char range in the editor (and scroll it into view) on the next frame.
    pub(crate) pending_select: Option<(usize, usize)>,
    /// Recorder: set by `write()`, taken by the recorder to wait for the swap.
    pub(crate) wrote: bool,
    /// Recorder: fixed editor panel width in points (None = theme default).
    pub(crate) editor_width: Option<f32>,
}

impl App {
    pub(crate) fn new(file: PathBuf) -> Self {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        App {
            file,
            on_disk: text.clone(),
            text,
            last_edit: Instant::now() - Duration::from_secs(10),
            last_poll: Instant::now(),
            tex: None,
            fps_at: 0.0,
            start: Instant::now(),
            fps_frames: 0,
            fps: 0.0,
            hl: crate::highlight::Highlighter::default(),
            theme: &theme::ISLANDS_DARK,
            focused_once: false,
            launch_frames: 8,
            pending_select: None,
            wrote: false,
            editor_width: None,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Seconds on the clock the fps readout uses: virtual while recording, wall time otherwise.
    fn now_s(&self) -> f64 {
        if VIRTUAL.load(Ordering::Relaxed) { VT_US.load(Ordering::Relaxed) as f64 / 1e6 } else { self.start.elapsed().as_secs_f64() }
    }

    fn write(&mut self) {
        // Rename so the watcher never sees a half-written file.
        let tmp = self.file.with_extension("rs.tmp");
        *EDIT_START.lock().unwrap() = Some(Instant::now());
        if std::fs::write(&tmp, &self.text).is_ok() && std::fs::rename(&tmp, &self.file).is_ok() {
            self.on_disk = self.text.clone();
            self.wrote = true;
        }
    }

    fn picture(&mut self, ctx: &egui::Context) -> egui::TextureHandle {
        let d = DIM.load(Ordering::Relaxed);
        let (w, h) = (((d >> 16) as usize).max(1), ((d & 0xffff) as usize).max(1));
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let p = PIX[y * MAXW + x].load(Ordering::Relaxed);
                rgba.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8, 255]);
            }
        }
        let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
        match &mut self.tex {
            Some(t) => {
                t.set(img, egui::TextureOptions::NEAREST);
                t.clone()
            }
            None => {
                let t = ctx.load_texture("life", img, egui::TextureOptions::NEAREST);
                self.tex = Some(t.clone());
                t
            }
        }
    }
}

/// Islands Dark: lighter window behind, darker rounded islands separated by a small gap.
fn island(t: &Theme, inner_v: i8, inner_h: i8) -> egui::Frame {
    egui::Frame::new()
        .fill(t.island_bg)
        .corner_radius(t.metrics.island_radius)
        .inner_margin(egui::Margin::symmetric(inner_h, inner_v))
        .outer_margin(egui::Margin::same(t.metrics.island_gap))
}

fn apply_theme(ctx: &egui::Context, t: &Theme) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = t.window_bg;
    v.window_fill = t.island_bg;
    v.extreme_bg_color = t.island_bg;
    v.override_text_color = Some(t.fg);
    v.selection.bg_fill = t.selection;
    v.selection.stroke = egui::Stroke::NONE;
    v.text_cursor.stroke = egui::Stroke::new(t.metrics.caret_width, t.caret);
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(t.metrics.font_size));
    });
}

/// Index of the bracket matching the one at (or just before) `cursor`, as (this, other) char indices.
fn match_bracket(chars: &[char], cursor: usize) -> Option<(usize, usize)> {
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
    for at in [cursor, cursor.wrapping_sub(1)] {
        let Some(&c) = chars.get(at) else { continue };
        for (o, cl) in PAIRS {
            if c == o {
                let mut depth = 0i32;
                for (i, &d) in chars.iter().enumerate().skip(at) {
                    depth += (d == o) as i32 - (d == cl) as i32;
                    if depth == 0 {
                        return Some((at, i));
                    }
                }
            } else if c == cl {
                let mut depth = 0i32;
                for i in (0..=at).rev() {
                    depth += (chars[i] == cl) as i32 - (chars[i] == o) as i32;
                    if depth == 0 {
                        return Some((at, i));
                    }
                }
            }
        }
    }
    None
}

impl App {
    /// Line-number gutter, current-line + matching-bracket highlight and the highlighted TextEdit.
    fn editor(&mut self, ui: &mut egui::Ui) {
        let t = self.theme;
        let m = &t.metrics;
        let id = egui::Id::unique("src");
        let font = egui::TextStyle::Monospace.resolve(ui.style());

        // Tab inserts spaces (egui would insert a literal tab).
        if ui.memory(|mem| mem.has_focus(id)) && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
            if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), id) {
                if let Some(r) = st.cursor.char_range() {
                    let at = r.primary.index.0 as usize;
                    let byte = self.text.char_indices().nth(at).map_or(self.text.len(), |(b, _)| b);
                    self.text.insert_str(byte, &" ".repeat(m.tab_spaces));
                    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(at + m.tab_spaces))));
                    st.store(ui.ctx(), id);
                    self.last_edit = Instant::now();
                    self.write();
                }
            }
        }

        let chars: Vec<char> = self.text.chars().collect();
        let cursor = egui::TextEdit::load_state(ui.ctx(), id).and_then(|st| st.cursor.char_range()).map(|r| r.primary.index.0 as usize);
        let cur_line = cursor.map_or(0, |c| chars.iter().take(c).filter(|&&c| c == '\n').count());
        let lines = chars.iter().filter(|&&c| c == '\n').count() + 1;
        let w = lines.to_string().len().max(m.gutter_min_digits);
        // Launch: the initial focus/caret placement scrolls the area; pin it to the top-left for the first frames.
        let mut area = egui::ScrollArea::both().auto_shrink(false);
        if self.launch_frames > 0 {
            self.launch_frames -= 1;
            area = area.scroll_offset(egui::Vec2::ZERO);
        }
        area.show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let bg = ui.painter().add(egui::Shape::Noop); // behind the gutter numbers and the text
                let mut g = egui::text::LayoutJob::default();
                for i in 0..lines {
                    let color = if i == cur_line { t.gutter_fg_current } else { t.gutter_fg };
                    let nl = if i + 1 < lines { "\n" } else { "" };
                    g.append(
                        &format!("{:>w$}{nl}", i + 1),
                        0.0,
                        egui::TextFormat { font_id: font.clone(), color, line_height: Some(m.line_height), ..Default::default() },
                    );
                }
                ui.add_space(m.gutter_left);
                let gal = ui.fonts_mut(|f| f.layout_job(g));
                let (rect, _) = ui.allocate_exact_size(gal.size(), egui::Sense::hover());
                ui.painter().galley(rect.min, gal, t.gutter_fg);
                ui.add_space(m.gutter_right);

                let hl = &mut self.hl;
                let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                    let font = egui::TextStyle::Monospace.resolve(ui.style());
                    let job = hl.job(t, buf.as_str(), font, f32::INFINITY); // never wrap: the gutter numbers logical lines
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let out = egui::TextEdit::multiline(&mut self.text)
                    .id(id)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::ZERO)
                    .layouter(&mut layouter)
                    .desired_width(f32::INFINITY)
                    .desired_rows(lines.max(m.editor_min_rows))
                    .lock_focus(true)
                    .show(ui);
                if let Some((a, b)) = self.pending_select.take() {
                    let mut st = out.state.clone();
                    st.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(a), egui::text::CCursor::new(b))));
                    st.store(ui.ctx(), id);
                    let row = out.galley.pos_from_cursor(egui::text::CCursor::new(a)).translate(out.galley_pos.to_vec2());
                    ui.scroll_to_rect(row, Some(egui::Align::Center));
                    out.response.request_focus();
                }
                if let Some(r) = out.cursor_range {
                    let origin = out.galley_pos.to_vec2();
                    let row = out.galley.pos_from_cursor(r.primary).translate(origin);
                    let full = egui::Rect::from_x_y_ranges(ui.min_rect().left() - m.current_line_bleed..=ui.max_rect().right(), row.top()..=row.bottom());
                    let mut shapes = vec![egui::Shape::rect_filled(full, m.current_line_radius, t.current_line)];
                    if r.primary == r.secondary {
                        let glyph = ui.fonts_mut(|f| f.glyph_width(&font, ' '));
                        if let Some((a, b)) = match_bracket(&chars, r.primary.index.0 as usize) {
                            for i in [a, b] {
                                let p = out.galley.pos_from_cursor(egui::text::CCursor::new(i)).translate(origin);
                                let cell = egui::Rect::from_min_size(p.min, egui::vec2(glyph, p.height()));
                                shapes.push(egui::Shape::rect_filled(cell, m.bracket_radius, t.bracket_match));
                            }
                        }
                    }
                    ui.painter().set(bg, egui::Shape::Vec(shapes));
                }
                if !self.focused_once {
                    out.response.request_focus();
                    let mut st = out.state.clone();
                    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(0))));
                    st.store(ui.ctx(), id);
                    self.focused_once = true;
                }
                if out.response.changed() {
                    self.last_edit = Instant::now();
                    self.write();
                }
            });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(root);
    }
}

impl App {
    /// One frame of the whole window; shared by the eframe window and the offline recorder.
    pub(crate) fn draw(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;
        ctx.request_repaint_after(Duration::from_millis(16));
        let t = self.theme;
        let m = &t.metrics;
        apply_theme(ctx, t);

        // Pick up edits made outside the window (another editor), unless we are typing.
        if self.last_poll.elapsed() > Duration::from_millis(250) && self.last_edit.elapsed() > Duration::from_secs(1) {
            self.last_poll = Instant::now();
            if let Ok(t) = std::fs::read_to_string(&self.file) {
                if t != self.on_disk {
                    self.text = t.clone();
                    self.on_disk = t;
                }
            }
        }

        let frames = FRAMES.load(Ordering::Relaxed);
        let now = self.now_s();
        if now - self.fps_at > 0.5 {
            self.fps = (frames - self.fps_frames) as f64 / (now - self.fps_at);
            self.fps_frames = frames;
            self.fps_at = now;
        }
        let (cold_ms, edits, last_ms, cargo_ms, recompiled, total_fns, error, note) = {
            let g = STATUS.lock().unwrap();
            match g.as_ref() {
                Some(s) => (s.cold_ms, s.edits, s.last_ms, s.cargo_ms, s.recompiled.clone(), s.total_fns, s.error.clone(), s.note.clone()),
                None => (0.0, 0, 0.0, 0.0, vec![], 0, String::new(), String::new()),
            }
        };
        let mut rustc = RUSTC_MODE.load(Ordering::Relaxed);

        egui::Panel::top("head").frame(island(t, m.header_pad_v, m.header_pad_h)).show(root, |ui| {
            ui.horizontal(|ui| {
                ui.label("compiler:");
                let a = ui.selectable_value(&mut rustc, false, "ra-jit (rust-analyzer MIR to Cranelift)");
                let b = ui.selectable_value(&mut rustc, true, "rustc (real `cargo build` before each swap)");
                if a.changed() || b.changed() {
                    RUSTC_MODE.store(rustc, Ordering::Relaxed);
                    WARM.store(rustc, Ordering::Relaxed);
                }
            });
            ui.horizontal_wrapped(|ui| {
                if edits == 0 {
                    ui.heading(egui::RichText::new("edit the code on the right").color(t.fg));
                } else {
                    ui.heading(egui::RichText::new(format!("edit to running: {last_ms:.1} ms")).color(t.function));
                }
                if rustc && edits > 0 {
                    ui.colored_label(t.comment, format!("   (the `cargo build` alone took {cargo_ms:.0} ms; a real rustc hot reload would also relink and reload)"));
                } else {
                    ui.colored_label(t.comment, "   vs incremental `cargo build` of the same file: ~170 ms (one run, dev-aws-8). Flip the toggle to feel it.");
                }
            });
            let which = if recompiled.is_empty() { String::new() } else { format!("recompiled {} of {total_fns} fns: {}", recompiled.len(), recompiled.join(", ")) };
            if !note.is_empty() {
                ui.colored_label(t.error, note.as_str());
            }
            ui.colored_label(t.number, format!("{:.0} fps   |   {edits} edits   |   cold start {cold_ms:.0} ms   |   {which}", self.fps));
        });

        egui::Panel::right("editor").frame(island(t, 0, 0)).default_size(self.editor_width.unwrap_or(m.editor_panel_width)).resizable(true).show(root, |ui| {
            // Fixed-height footer so the editor never jumps when an error appears or clears.
            egui::Panel::bottom("err").frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(m.footer_pad_h, m.footer_pad_v))).exact_size(m.footer_height).show(ui, |ui| {
                if error.is_empty() {
                    ui.colored_label(t.comment, "no errors");
                } else {
                    ui.colored_label(t.error, "keeping the old code for:");
                    egui::ScrollArea::vertical().id_salt("errs").show(ui, |ui| {
                        ui.monospace(&error);
                    });
                }
            });
            egui::Frame::new().inner_margin(egui::Margin::symmetric(0, m.editor_pad_v)).show(ui, |ui| {
                self.editor(ui);
            });
        });

        egui::CentralPanel::default().frame(island(t, m.life_pad, m.life_pad)).show(root, |ui| {
            if FRAMES.load(Ordering::Relaxed) == 0 {
                ui.heading("compiling Rust with rust-analyzer (a few seconds, once)...");
                return;
            }
            let tex = self.picture(ctx);
            let avail = ui.available_size();
            let s = tex.size_vec2();
            let k = (avail.x / s.x).min(avail.y / s.y);
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::from_texture(&tex).fit_to_exact_size(s * k));
            });
        });
    }
}

pub fn run(file: PathBuf) -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size(theme::ISLANDS_DARK.metrics.window_size).with_title("ra-jit: Game of Life, live"),
        ..Default::default()
    };
    eframe::run_native("ra-jit live", opts, Box::new(|_cc| Ok(Box::new(App::new(file)))))
}
