use std::{
    io::Write,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[inline(never)]
fn answer() -> u64 {
    1
}

fn main() {
    let live = match ra_jit_live::Live::start() {
        Ok(live) => live,
        Err(why) => {
            println!("no live edits: {why}");
            std::process::exit(2);
        }
    };
    // Applied the moment it arrives, as the Game of Life window does.
    live.apply_as_they_come(|outcome| {
        // When it ran, for live_latency.py to line up with the daemon's log.
        let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
        println!("outcome: {outcome} at_us={at}");
        std::io::stdout().flush().unwrap();
    });
    println!("ready");
    let mut seen = 0;
    loop {
        let now = answer();
        if now != seen {
            let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
            println!("answer {now} at_us={at}");
            seen = now;
        }
        std::io::stdout().flush().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}
