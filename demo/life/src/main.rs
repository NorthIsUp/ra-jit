// Conway's Game of Life under ra-jit. Edit this file while it runs: the daemon recompiles only
// the function you touched and the next frame uses it, with the grid intact.
//
// Try:  next_age(): change `n == 3` to `n == 3 || n == 6` (HighLife)
//       color():    change the numbers to repaint every cell
//       step_ms():  make it faster or slower
//
// Live edits are macOS on AArch64 only; elsewhere the window runs and edits need a restart.
use std::time::{Duration, Instant};

use eframe::egui;

const W: usize = 64;
const H: usize = 48;

// ---- The part to edit. -------------------------------------------------------------------

/// Milliseconds between generations.
#[inline(never)]
fn step_ms() -> u64 {
    80
}

/// The rule. `age` is 0 for a dead cell, else how many generations it has lived; `n` is the
/// number of live neighbours. Returns the new age.
#[inline(never)]
fn next_age(age: u32, n: u32) -> u32 {
    if age > 0 {
        if n == 2 || n == 3 { age + 1 } else { 0 }
    } else if n == 3 {
        1
    } else {
        0
    }
}

/// Colour of a live cell as 0xRRGGBB: young cells are bright and old ones fade to blue.
#[inline(never)]
fn color(age: u32) -> u32 {
    if age == 1 {
        0xffffff
    } else if age < 4 {
        0xffd23f
    } else if age < 12 {
        0x3ddc97
    } else {
        0x3a86ff
    }
}

// ---- The host: state that lives across edits. --------------------------------------------

struct Life {
    age: Vec<u32>,
    generation: u64,
    seed: u64,
    next_step: Instant,
}

impl Life {
    fn new() -> Life {
        let mut life = Life { age: vec![0; W * H], generation: 0, seed: 12345, next_step: Instant::now() };
        life.reseed();
        life
    }

    fn rand(&mut self) -> u64 {
        self.seed = (self.seed.wrapping_mul(1103515245).wrapping_add(12345)) % 2147483648;
        (self.seed >> 8) % 100
    }

    fn reseed(&mut self) {
        for i in 0..W * H {
            self.age[i] = if self.rand() < 30 { 1 } else { 0 };
        }
    }

    fn neighbours(&self, x: usize, y: usize) -> u32 {
        let mut n = 0;
        for dy in 0..3 {
            for dx in 0..3 {
                if dx != 1 || dy != 1 {
                    let xx = (x + W + dx - 1) % W;
                    let yy = (y + H + dy - 1) % H;
                    if self.age[yy * W + xx] > 0 {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    fn step(&mut self) {
        let mut next = vec![0; W * H];
        for y in 0..H {
            for x in 0..W {
                next[y * W + x] = next_age(self.age[y * W + x], self.neighbours(x, y));
            }
        }
        self.age = next;
        self.generation += 1;
        if self.age.iter().filter(|a| **a > 0).count() < 25 {
            self.reseed();
        }
    }
}

struct App {
    life: Life,
    live: Option<ra_jit_live::Live>,
    status: String,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Between frames, where nothing of ours is half done.
        if let Some(outcome) = self.live.as_mut().and_then(|it| it.poll()) {
            self.status = outcome.to_string();
        }
        if Instant::now() >= self.life.next_step {
            self.life.step();
            self.life.next_step = Instant::now() + Duration::from_millis(step_ms());
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            let side = (ui.available_width() / W as f32).min((ui.available_height() - 24.0) / H as f32);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(side * W as f32, side * H as f32), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            for y in 0..H {
                for x in 0..W {
                    let age = self.life.age[y * W + x];
                    let rgb = if age > 0 { color(age) } else { 0x101018 };
                    let [_, r, g, b] = rgb.to_be_bytes();
                    let min = rect.min + egui::vec2(x as f32, y as f32) * side;
                    painter.rect_filled(
                        egui::Rect::from_min_size(min, egui::vec2(side - 1.0, side - 1.0)),
                        0.0,
                        egui::Color32::from_rgb(r, g, b),
                    );
                }
            }
            ui.label(format!("generation {}   {}", self.life.generation, self.status));
        });
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

fn main() -> eframe::Result {
    let (live, status) = match ra_jit_live::Live::start() {
        Ok(live) => (Some(live), "live edits on".to_owned()),
        Err(why) => (None, format!("live edits off: {why}")),
    };
    eprintln!("life: {status}");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([768.0, 600.0]).with_title("ra-jit: Game of Life"),
        ..Default::default()
    };
    eframe::run_native(
        "ra-jit life",
        options,
        Box::new(move |_| Ok(Box::new(App { life: Life::new(), live, status }))),
    )
}
