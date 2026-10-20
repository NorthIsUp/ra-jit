use std::{
    io::Write,
    time::Duration,
};

#[inline(never)]
fn answer() -> u64 {
    1
}

fn main() {
    let mut live = match ra_jit_live::Live::start() {
        Ok(live) => live,
        Err(why) => {
            println!("no live edits: {why}");
            std::process::exit(2);
        }
    };
    println!("ready");
    let mut seen = 0;
    loop {
        if let Some(outcome) = live.poll() {
            println!("outcome: {outcome}");
        }
        let now = answer();
        if now != seen {
            println!("answer {now}");
            seen = now;
        }
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));
    }
}
