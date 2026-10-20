use std::{
    io::Write,
    time::{Duration, SystemTime, UNIX_EPOCH},
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
            // When it ran, for live_latency.py to line up with the daemon's log.
            let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
            println!("outcome: {outcome} at_us={at}");
        }
        let now = answer();
        if now != seen {
            println!("answer {now}");
            seen = now;
        }
        std::io::stdout().flush().unwrap();
        // A frame of a program that polls often: what a patch waits for here is not the daemon's.
        std::thread::sleep(Duration::from_millis(1));
    }
}
