//! Futures made by one compiler and polled or dropped by the other.
//!
//! `difftest.py mixfutures --bin runner --crate futs --mixed N` leaves a random half of these
//! functions to rustc. Nothing keeps a future and what polls it on one side (unless
//! `RA_JIT_SPLIT_FUTURES=1`), so a future rustc's code makes is polled by ra-jit's and the
//! other way round, which is right only where both lay the future out alike. Each future here keeps something across
//! an await: a value with a destructor, a borrow of its own local, a value moved out on one path
//! only (rustc keeps a drop flag for it), a future it waits for; some are dropped while
//! suspended or before they start, and one unwinds.

use std::cell::RefCell;
use std::future::Future;
use std::mem::size_of_val;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

thread_local! {
    static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub fn log(line: String) {
    LOG.with(|it| it.borrow_mut().push(line));
}

pub struct Loud(pub &'static str);

impl Drop for Loud {
    fn drop(&mut self) {
        log(format!("drop {}", self.0));
    }
}

/// Pending once, then ready.
pub struct Pause(bool);

impl Future for Pause {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

pub fn pause() -> Pause {
    Pause(false)
}

struct Noop;

impl Wake for Noop {
    fn wake(self: Arc<Self>) {}
}

#[inline(never)]
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut polls = 0;
    loop {
        polls += 1;
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            log(format!("ready after {polls} polls"));
            return value;
        }
    }
}

/// Polls `future` `times` times and drops it where it is.
#[inline(never)]
pub fn poll_then_drop<F: Future>(future: F, times: usize) {
    let mut future = Box::pin(future);
    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    for _ in 0..times {
        if future.as_mut().poll(&mut cx).is_ready() {
            log("ready before it was dropped".to_owned());
            return;
        }
    }
    drop(future);
}

pub async fn held(s: String) -> usize {
    let a = Loud("held a");
    pause().await;
    let b = format!("{s}-{}", a.0);
    pause().await;
    drop(a);
    pause().await;
    b.len()
}

pub async fn borrowed(n: u64) -> u64 {
    let values = [n, n + 1, n + 2];
    let first = &values[0];
    pause().await;
    let sum: u64 = values.iter().sum();
    pause().await;
    *first + sum
}

pub async fn moved_on_one_path(flag: bool) -> &'static str {
    let x = Loud("moved x");
    if flag {
        drop(x);
    }
    pause().await;
    "done"
}

async fn inner(n: u32) -> u32 {
    let l = Loud("inner");
    pause().await;
    drop(l);
    n * 2
}

pub async fn nested(n: u32) -> u32 {
    let a = held(format!("n{n}")).await as u32;
    let b = borrowed(u64::from(n)).await as u32;
    a + b + inner(n).await
}

pub async fn panics(n: u32) -> u32 {
    let _kept = Loud("kept over a panic");
    pause().await;
    if n > 0 {
        panic!("boom");
    }
    n
}

/// A function that makes a future and is not the `async fn` itself.
#[inline(never)]
pub fn make_held() -> impl Future<Output = usize> {
    held("made".to_owned())
}

#[inline(never)]
pub fn make_block(tag: &'static str) -> impl Future<Output = String> {
    let owned = Loud(tag);
    async move {
        pause().await;
        let name = owned.0.to_owned();
        pause().await;
        name
    }
}

pub fn run() {
    println!("{}", block_on(held("x".to_owned())));
    println!("{}", block_on(borrowed(5)));
    println!("{}", block_on(moved_on_one_path(true)));
    println!("{}", block_on(moved_on_one_path(false)));
    println!("{}", block_on(nested(3)));
    println!("{}", block_on(make_held()));
    println!("{}", block_on(make_block("block")));
    poll_then_drop(held("dropped".to_owned()), 2);
    poll_then_drop(nested(4), 3);
    poll_then_drop(moved_on_one_path(false), 1);
    poll_then_drop(make_block("dropped block"), 2);
    poll_then_drop(held("never polled".to_owned()), 0);
    let caught = std::panic::catch_unwind(|| block_on(panics(1)));
    println!("panicked: {}", caught.is_err());
    let sizes = [
        size_of_val(&held(String::new())),
        size_of_val(&borrowed(0)),
        size_of_val(&moved_on_one_path(false)),
        size_of_val(&nested(0)),
        size_of_val(&make_block("size")),
    ];
    println!("sizes {sizes:?}");
    for line in LOG.with(|it| std::mem::take(&mut *it.borrow_mut())) {
        println!("{line}");
    }
}
