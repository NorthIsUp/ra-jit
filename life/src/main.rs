// Conway's Game of Life, running live under ra-jit. Edit this
// file while it runs: every keystroke recompiles only the
// function you touched and the next frame uses it. The grid is
// owned by the host (it calls `init` once, then `frame(&mut
// state)` every frame), so the cells keep their state.
//
// Try:  next_age(): change `n == 3` to `n == 3 || n == 6`
//       color():    change the numbers to repaint every cell
//       STEP_MS:    make it faster or slower
mod host;

unsafe extern "C" {
    safe fn set_cell(x: i64, y: i64, rgb: i64);
    safe fn present(w: i64, h: i64);
    safe fn sleep_ms(ms: i64);
}

const W: usize = 64;
const H: usize = 48;
const STEP_MS: i64 = 40;

struct Life {
    age: [[i64; W]; H],
    tick: i64,
    seed: i64,
}

// Initial state. The host calls this once and keeps the result;
// it runs again (resetting the grid) only if you change the
// layout of `Life`.
fn init() -> Life {
    let mut l = Life { age: [[0; W]; H], tick: 0, seed: 12345 };
    reseed(&mut l);
    l
}

// The rule. `age` is 0 for dead, else how many generations the
// cell has been alive. `n` is the number of live neighbours.
// Return the new age.
fn next_age(age: i64, n: i64) -> i64 {
    if age > 0 {
        if n == 2 || n == 3 { age + 1 } else { 0 }
    } else {
        if n == 3 { 1 } else { 0 }
    }
}

// Colour of a live cell as 0xRRGGBB. Young cells are bright,
// old ones fade to blue.
fn color(age: i64) -> i64 {
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

fn rand(l: &mut Life) -> i64 {
    l.seed = (l.seed * 1103515245 + 12345) % 2147483648;
    (l.seed >> 8) % 100
}

fn reseed(l: &mut Life) {
    for y in 0..H {
        for x in 0..W {
            l.age[y][x] = if rand(l) < 30 { 1 } else { 0 };
        }
    }
}

fn neighbours(l: &Life, x: usize, y: usize) -> i64 {
    let mut n = 0;
    for dy in 0..3 {
        for dx in 0..3 {
            if dx != 1 || dy != 1 {
                let xx = (x + W + dx - 1) % W;
                let yy = (y + H + dy - 1) % H;
                if l.age[yy][xx] > 0 {
                    n += 1;
                }
            }
        }
    }
    n
}

fn step(l: &mut Life) {
    let mut next = [[0i64; W]; H];
    for y in 0..H {
        for x in 0..W {
            next[y][x] = next_age(l.age[y][x], neighbours(l, x, y));
        }
    }
    l.age = next;
    l.tick += 1;
}

fn draw(l: &Life) -> i64 {
    let mut alive = 0;
    for y in 0..H {
        for x in 0..W {
            let a = l.age[y][x];
            if a > 0 {
                alive += 1;
                set_cell(x as i64, y as i64, color(a));
            } else {
                set_cell(x as i64, y as i64, 0x101018);
            }
        }
    }
    alive
}

// Called by the host once per frame with the state it owns.
fn frame(l: &mut Life) {
    step(l);
    if draw(l) < 25 {
        reseed(l);
    }
    present(W as i64, H as i64);
    sleep_ms(STEP_MS);
}
