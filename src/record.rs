//! `ra-jit life <dir> --record out.mp4 --timeline tl.tsv`: render the demo window offline, frame by frame, to a
//! video. No OS window and no screen capture: the real egui layout (the same `App::draw` the window uses) runs on a
//! plain `egui::Context` with a fixed screen size and a fixed 60 fps clock, a small software rasteriser paints
//! egui's tessellated triangles into an RGB buffer, and the raw frames are piped to ffmpeg (H.264, yuv420p).
//!
//! Determinism: the guest's `sleep_ms` runs on a virtual clock that advances 1/60 s per video frame (see
//! `demo::advance_virtual`), egui sees a fake clock, and the scripted edits are typed into the real editor. What is
//! *not* faked is the hot-swap itself: each edit goes through the normal write -> watcher -> rust-analyzer ->
//! Cranelift path and the "edit to running" readout shows the measured time.
//!
//! Timeline format (written by `tools/record_demo.py`), one command per line, fields separated by TAB, with `\n`,
//! `\t` and `\\` escapes inside a field:
//!   wait <frames>              render this many frames
//!   select <context> <target>  select `target` inside the first occurrence of `context` and scroll it to centre
//!   type <text>                type text over the selection (one egui text event, then wait for the swap)

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use eframe::egui::{self, Event, TextureId};

use crate::demo::{self, App};
use crate::theme;

pub const W: usize = 1280;
pub const H: usize = 720;
pub const FPS: u64 = 60;
const PPP: f32 = 1.25;

enum Cmd {
    Wait(u32),
    Select(String, String),
    Type(String),
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

fn parse_timeline(text: &str) -> Result<Vec<Cmd>> {
    let mut v = Vec::new();
    for (i, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty() && !l.starts_with('#')) {
        let f: Vec<&str> = line.split('\t').collect();
        let bad = || anyhow!("timeline line {}: bad command {line:?}", i + 1);
        v.push(match f.as_slice() {
            ["wait", n] => Cmd::Wait(n.parse().map_err(|_| bad())?),
            ["select", c, t] => Cmd::Select(unescape(c), unescape(t)),
            ["type", t] => Cmd::Type(unescape(t)),
            _ => return Err(bad()),
        });
    }
    Ok(v)
}

// ---------------------------------------------------------------- software rasteriser

struct Tex {
    w: usize,
    h: usize,
    /// premultiplied RGBA
    px: Vec<[u8; 4]>,
    linear: bool,
}

impl Tex {
    fn texel(&self, x: i32, y: i32) -> [f32; 4] {
        let x = x.clamp(0, self.w as i32 - 1) as usize;
        let y = y.clamp(0, self.h as i32 - 1) as usize;
        let p = self.px[y * self.w + x];
        [p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32]
    }

    /// `bounds` is the triangle's uv box (u0, v0, u1, v1): the sample point is kept half a texel inside it, so a
    /// glyph quad that does not sit exactly on the pixel grid never bleeds in its atlas neighbours.
    fn sample(&self, u: f32, v: f32, bounds: [f32; 4]) -> [f32; 4] {
        let (fx, fy) = (u * self.w as f32, v * self.h as f32);
        if !self.linear {
            return self.texel(fx.floor() as i32, fy.floor() as i32);
        }
        let keep = |x: f32, lo: f32, hi: f32| {
            let (lo, hi) = (lo + 0.5, hi - 0.5);
            if hi < lo { (lo + hi) / 2.0 } else { x.clamp(lo, hi) }
        };
        let fx = keep(fx, bounds[0] * self.w as f32, bounds[2] * self.w as f32) - 0.5;
        let fy = keep(fy, bounds[1] * self.h as f32, bounds[3] * self.h as f32) - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let (x0, y0) = (x0 as i32, y0 as i32);
        let (a, b, c, d) = (self.texel(x0, y0), self.texel(x0 + 1, y0), self.texel(x0, y0 + 1), self.texel(x0 + 1, y0 + 1));
        let mut o = [0.0; 4];
        for i in 0..4 {
            o[i] = (a[i] * (1.0 - tx) + b[i] * tx) * (1.0 - ty) + (c[i] * (1.0 - tx) + d[i] * tx) * ty;
        }
        o
    }
}

struct Raster {
    fb: Vec<u8>,
    texs: std::collections::HashMap<TextureId, Tex>,
}

impl Raster {
    fn apply(&mut self, delta: &egui::TexturesDelta) {
        for (id, ds) in &delta.set {
            for d in ds {
            let egui::ImageData::Color(img) = &d.image;
            let linear = d.options.magnification == egui::TextureFilter::Linear;
            let px: Vec<[u8; 4]> = img.pixels.iter().map(|c| c.to_array()).collect();
            match d.pos {
                None => {
                    self.texs.insert(*id, Tex { w: img.size[0], h: img.size[1], px, linear });
                }
                Some([x0, y0]) => {
                    if let Some(t) = self.texs.get_mut(id) {
                        for y in 0..img.size[1] {
                            for x in 0..img.size[0] {
                                t.px[(y0 + y) * t.w + x0 + x] = px[y * img.size[0] + x];
                            }
                        }
                    }
                }
            }
            }
        }
        for id in &delta.free {
            self.texs.remove(id);
        }
    }

    fn clear(&mut self, c: egui::Color32) {
        for p in self.fb.chunks_exact_mut(3) {
            p.copy_from_slice(&[c.r(), c.g(), c.b()]);
        }
    }

    fn paint(&mut self, prims: &[egui::ClippedPrimitive]) {
        for cp in prims {
            let egui::epaint::Primitive::Mesh(mesh) = &cp.primitive else { continue };
            let Some(tex) = self.texs.get(&mesh.texture_id) else { continue };
            let r = cp.clip_rect;
            let clip = [
                (r.min.x * PPP).round().max(0.0) as i32,
                (r.min.y * PPP).round().max(0.0) as i32,
                (r.max.x * PPP).round().min(W as f32) as i32,
                (r.max.y * PPP).round().min(H as f32) as i32,
            ];
            for t in mesh.indices.chunks_exact(3) {
                let v = [&mesh.vertices[t[0] as usize], &mesh.vertices[t[1] as usize], &mesh.vertices[t[2] as usize]];
                tri(&mut self.fb, tex, clip, v);
            }
        }
    }
}

fn edge(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// Owns an edge shared by two triangles (traversed in opposite directions) for exactly one of them.
fn owns(a: (f32, f32), b: (f32, f32)) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    dy > 0.0 || (dy == 0.0 && dx < 0.0)
}

fn tri(fb: &mut [u8], tex: &Tex, clip: [i32; 4], v: [&egui::epaint::Vertex; 3]) {
    let pt = |i: usize| (v[i].pos.x * PPP, v[i].pos.y * PPP);
    let (a, mut b, mut c) = (pt(0), pt(1), pt(2));
    let (va, mut vb, mut vc) = (v[0], v[1], v[2]);
    let mut area = edge(a, b, c);
    if area == 0.0 {
        return;
    }
    if area < 0.0 {
        std::mem::swap(&mut b, &mut c);
        std::mem::swap(&mut vb, &mut vc);
        area = -area;
    }
    let x0 = (a.0.min(b.0).min(c.0).floor() as i32).max(clip[0]);
    let x1 = (a.0.max(b.0).max(c.0).ceil() as i32).min(clip[2]);
    let y0 = (a.1.min(b.1).min(c.1).floor() as i32).max(clip[1]);
    let y1 = (a.1.max(b.1).max(c.1).ceil() as i32).min(clip[3]);
    let ub = [
        va.uv.x.min(vb.uv.x).min(vc.uv.x),
        va.uv.y.min(vb.uv.y).min(vc.uv.y),
        va.uv.x.max(vb.uv.x).max(vc.uv.x),
        va.uv.y.max(vb.uv.y).max(vc.uv.y),
    ];
    let (oa, ob, oc) = (owns(b, c), owns(c, a), owns(a, b));
    let flat = va.uv == vb.uv && vb.uv == vc.uv && va.color == vb.color && vb.color == vc.color;
    let col = |v: &egui::epaint::Vertex| v.color.to_array().map(|x| x as f32);
    let (ca, cb, cc) = (col(va), col(vb), col(vc));
    for y in y0..y1 {
        for x in x0..x1 {
            let p = (x as f32 + 0.5, y as f32 + 0.5);
            let (w0, w1, w2) = (edge(b, c, p), edge(c, a, p), edge(a, b, p));
            if !((w0 > 0.0 || (w0 == 0.0 && oa)) && (w1 > 0.0 || (w1 == 0.0 && ob)) && (w2 > 0.0 || (w2 == 0.0 && oc))) {
                continue;
            }
            let (l0, l1, l2) = (w0 / area, w1 / area, w2 / area);
            let (u, vv) = if flat {
                (va.uv.x, va.uv.y)
            } else {
                (l0 * va.uv.x + l1 * vb.uv.x + l2 * vc.uv.x, l0 * va.uv.y + l1 * vb.uv.y + l2 * vc.uv.y)
            };
            let t = tex.sample(u, vv, ub);
            let mut s = [0.0f32; 4];
            for i in 0..4 {
                let vcol = l0 * ca[i] + l1 * cb[i] + l2 * cc[i];
                s[i] = t[i] * vcol / 255.0;
            }
            let inv = 1.0 - s[3] / 255.0;
            let o = (y as usize * W + x as usize) * 3;
            for i in 0..3 {
                fb[o + i] = (s[i] + fb[o + i] as f32 * inv).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

// ---------------------------------------------------------------- driver

struct Rec {
    ctx: egui::Context,
    app: App,
    ras: Raster,
    ffmpeg: std::process::Child,
    frame: u64,
    recording: bool,
    bg: egui::Color32,
}

impl Rec {
    fn render(&mut self, events: Vec<Event>) -> Result<()> {
        let us = |f: u64| f * 1_000_000 / FPS;
        if !demo::advance_virtual(us(self.frame + 1) - us(self.frame)) {
            bail!("guest did not park after advancing the virtual clock (frame {})", self.frame);
        }
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(W as f32 / PPP, H as f32 / PPP))),
            time: Some(self.frame as f64 / FPS as f64),
            predicted_dt: 1.0 / FPS as f32,
            events,
            ..Default::default()
        };
        input.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo { native_pixels_per_point: Some(PPP), focused: Some(true), ..Default::default() },
        );
        let out = self.ctx.run_ui(input, |ui| self.app.draw(ui));
        self.ras.apply(&out.textures_delta);
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        self.ras.clear(self.bg);
        self.ras.paint(&prims);
        self.frame += 1;
        if self.recording {
            let stdin = self.ffmpeg.stdin.as_mut().context("ffmpeg stdin closed")?;
            stdin.write_all(&self.ras.fb).context("writing frame to ffmpeg")?;
        }
        Ok(())
    }

    fn wait_frames(&mut self, n: u32) -> Result<()> {
        for _ in 0..n {
            self.render(vec![])?;
        }
        Ok(())
    }

    /// Render one frame with `events`; if the editor wrote the file, wait (real time) for the swap or an error.
    fn render_and_settle(&mut self, events: Vec<Event>) -> Result<()> {
        let before = status_marks();
        self.render(events)?;
        if self.app.wrote {
            self.app.wrote = false;
            let t = Instant::now();
            while status_marks() == before {
                if t.elapsed() > Duration::from_secs(10) {
                    bail!("edit was not picked up within 10 s");
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }
}

fn status_marks() -> (u64, String) {
    let g = demo::STATUS.lock().unwrap();
    g.as_ref().map_or((0, String::new()), |s| (s.edits, s.error.clone()))
}

fn byte_to_char(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

pub fn run(file: std::path::PathBuf, out: &Path, timeline: &Path) -> Result<()> {
    let cmds = parse_timeline(&std::fs::read_to_string(timeline).with_context(|| format!("reading {}", timeline.display()))?)?;

    // Wait for the cold start (rust-analyzer analysing core) and the first presented frame.
    let t = Instant::now();
    while demo::frames_presented() == 0 {
        if let Some(e) = demo::STATUS.lock().unwrap().as_ref().map(|s| s.error.clone()).filter(|e| !e.is_empty()) {
            bail!("guest failed before its first frame: {e}");
        }
        if t.elapsed() > Duration::from_secs(300) {
            bail!("guest never presented a frame (cold start failed?)");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let ffmpeg = Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s", &format!("{W}x{H}"), "-r", &FPS.to_string(), "-i", "-"])
        .args(["-c:v", "libx264", "-preset", "slow", "-crf", "18", "-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(out)
        .stdin(Stdio::piped())
        .spawn()
        .context("spawning ffmpeg")?;

    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(PPP);
    let mut app = App::new(file);
    app.editor_width = Some(640.0);
    let mut rec = Rec {
        ctx,
        app,
        ras: Raster { fb: vec![0; W * H * 3], texs: Default::default() },
        ffmpeg,
        frame: 0,
        recording: false,
        bg: theme::ISLANDS_DARK.window_bg,
    };
    rec.wait_frames(30)?; // settle the launch (focus, scroll pinning, first layout), not recorded
    rec.recording = true;
    for cmd in cmds {
        match cmd {
            Cmd::Wait(n) => rec.wait_frames(n)?,
            Cmd::Select(ctxt, target) => {
                let text = rec.app.text();
                let at = text.find(&ctxt).ok_or_else(|| anyhow!("select: context {ctxt:?} not in source"))?;
                let off = ctxt.find(&target).ok_or_else(|| anyhow!("select: target {target:?} not in context"))?;
                let s = byte_to_char(text, at + off);
                rec.app.pending_select = Some((s, s + target.chars().count()));
                rec.render(vec![])?;
            }
            Cmd::Type(text) => rec.render_and_settle(vec![Event::Text(text)])?,
        }
    }
    drop(rec.ffmpeg.stdin.take());
    let st = rec.ffmpeg.wait().context("waiting for ffmpeg")?;
    if !st.success() {
        bail!("ffmpeg exited with {st}");
    }
    eprintln!("recorded {} frames ({:.1} s) to {}", rec.frame - 30, (rec.frame - 30) as f64 / FPS as f64, out.display());
    Ok(())
}
