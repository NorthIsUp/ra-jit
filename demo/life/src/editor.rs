//! The source editor on the right of the window: line-number gutter, caret-row and
//! matching-bracket highlight, Rust highlighting, Tab as four spaces. Every edit is written to
//! the file (and Cmd/Ctrl+S writes it again); the launcher sees the file change, the daemon
//! builds, and the window applies the patch on its next frame.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::highlight::Highlighter;
use crate::theme::Theme;

pub struct Editor {
    /// Where the edits go; `None` when the source was not found.
    pub file: Option<PathBuf>,
    pub text: String,
    on_disk: String,
    last_edit: Instant,
    last_poll: Instant,
    /// When the text last reached the file.
    pub wrote_at: Option<Instant>,
    /// Edits written so far.
    pub writes: u64,
    /// Why the last write failed.
    pub write_error: Option<String>,
    hl: Highlighter,
    focused_once: bool,
}

impl Editor {
    pub fn new(file: Option<PathBuf>, fallback: &str) -> Editor {
        let text = file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .unwrap_or_else(|| fallback.to_owned());
        Editor {
            file,
            on_disk: text.clone(),
            text,
            last_edit: Instant::now() - Duration::from_secs(10),
            last_poll: Instant::now(),
            wrote_at: None,
            writes: 0,
            write_error: None,
            hl: Highlighter::default(),
            focused_once: false,
        }
    }

    /// Writes the text out, by rename so that the build never sees a half-written file.
    fn write(&mut self) {
        let Some(file) = &self.file else { return };
        if self.text == self.on_disk {
            return;
        }
        let tmp = file.with_extension("rs.tmp");
        match std::fs::write(&tmp, &self.text).and_then(|()| std::fs::rename(&tmp, file)) {
            Ok(()) => {
                self.on_disk = self.text.clone();
                self.wrote_at = Some(Instant::now());
                self.writes += 1;
                self.write_error = None;
            }
            Err(why) => {
                self.write_error = Some(format!("could not save {}: {why}", file.display()))
            }
        }
    }

    /// Picks up edits made outside the window (another editor), unless we are typing.
    pub fn reload(&mut self) {
        let Some(file) = &self.file else { return };
        if self.last_poll.elapsed() < Duration::from_millis(250)
            || self.last_edit.elapsed() < Duration::from_secs(1)
        {
            return;
        }
        self.last_poll = Instant::now();
        if let Ok(t) = std::fs::read_to_string(file)
            && t != self.on_disk
        {
            self.text = t.clone();
            self.on_disk = t;
        }
    }

    fn edited(&mut self) {
        self.last_edit = Instant::now();
        self.write();
    }

    pub fn show(&mut self, ui: &mut egui::Ui, t: &Theme) {
        let m = &t.metrics;
        let id = egui::Id::unique("life-source");
        let font = egui::TextStyle::Monospace.resolve(ui.style());

        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
            self.on_disk.clear();
            self.edited();
        }

        // Tab inserts spaces (egui would insert a literal tab).
        if ui.memory(|mem| mem.has_focus(id))
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab))
            && let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), id)
            && let Some(r) = st.cursor.char_range()
        {
            let at = r.primary.index.0;
            let byte = self.text.char_indices().nth(at).map_or(self.text.len(), |(b, _)| b);
            self.text.insert_str(byte, &" ".repeat(m.tab_spaces));
            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(at + m.tab_spaces),
            )));
            st.store(ui.ctx(), id);
            self.edited();
        }

        let chars: Vec<char> = self.text.chars().collect();
        let cursor = egui::TextEdit::load_state(ui.ctx(), id)
            .and_then(|st| st.cursor.char_range())
            .map(|r| r.primary.index.0);
        let cur_line = cursor.map_or(0, |c| chars.iter().take(c).filter(|&&c| c == '\n').count());
        let lines = chars.iter().filter(|&&c| c == '\n').count() + 1;
        let w = lines.to_string().len().max(m.gutter_min_digits);
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // The caret-row band goes under the gutter and the text, so it is painted first.
                let bg = ui.painter().add(egui::Shape::Noop);
                let mut g = egui::text::LayoutJob::default();
                for i in 0..lines {
                    let color = if i == cur_line { t.gutter_fg_current } else { t.gutter_fg };
                    let nl = if i + 1 < lines { "\n" } else { "" };
                    g.append(
                        &format!("{:>w$}{nl}", i + 1),
                        0.0,
                        egui::TextFormat {
                            font_id: font.clone(),
                            color,
                            line_height: Some(m.line_height),
                            ..Default::default()
                        },
                    );
                }
                ui.add_space(m.gutter_left);
                let gal = ui.fonts_mut(|f| f.layout_job(g));
                let (rect, _) = ui.allocate_exact_size(gal.size(), egui::Sense::hover());
                ui.painter().galley(rect.min, gal, t.gutter_fg);
                ui.add_space(m.gutter_right);

                let hl = &mut self.hl;
                // No wrapping: one source line is one row, so the gutter lines up (long lines scroll).
                let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
                    let font = egui::TextStyle::Monospace.resolve(ui.style());
                    let job = hl.job(t, buf.as_str(), font, f32::INFINITY);
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
                if let Some(r) = out.cursor_range {
                    let origin = out.galley_pos.to_vec2();
                    let row = out.galley.pos_from_cursor(r.primary).translate(origin);
                    let full = egui::Rect::from_x_y_ranges(
                        ui.min_rect().left() - m.current_line_bleed..=ui.max_rect().right(),
                        row.top()..=row.bottom(),
                    );
                    let mut shapes =
                        vec![egui::Shape::rect_filled(full, m.current_line_radius, t.current_line)];
                    if r.primary == r.secondary {
                        let glyph = ui.fonts_mut(|f| f.glyph_width(&font, ' '));
                        if let Some((a, b)) = match_bracket(&chars, r.primary.index.0) {
                            for i in [a, b] {
                                let p = out
                                    .galley
                                    .pos_from_cursor(egui::text::CCursor::new(i))
                                    .translate(origin);
                                let cell =
                                    egui::Rect::from_min_size(p.min, egui::vec2(glyph, p.height()));
                                shapes.push(egui::Shape::rect_filled(
                                    cell,
                                    m.bracket_radius,
                                    t.bracket_match,
                                ));
                            }
                        }
                    }
                    ui.painter().set(bg, egui::Shape::Vec(shapes));
                }
                if !self.focused_once {
                    out.response.request_focus();
                    let mut st = out.state.clone();
                    st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(0),
                    )));
                    st.store(ui.ctx(), id);
                    self.focused_once = true;
                }
                if out.response.changed() {
                    self.edited();
                }
            });
        });
    }
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
                    depth += i32::from(d == o) - i32::from(d == cl);
                    if depth == 0 {
                        return Some((at, i));
                    }
                }
            } else if c == cl {
                let mut depth = 0i32;
                for i in (0..=at).rev() {
                    depth += i32::from(chars[i] == cl) - i32::from(chars[i] == o);
                    if depth == 0 {
                        return Some((at, i));
                    }
                }
            }
        }
    }
    None
}
