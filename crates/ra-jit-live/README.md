# ra-jit-live

Edit Rust and the running program has the new code about a millisecond later. Its state stays.

The runtime half of [ra-jit](https://github.com/indexable-inc/ra-jit): link it, poll once per
frame, and run your program under the ra-jit daemon.

```toml
[dependencies]
ra-jit-live = "0.1"
```

```rust
let mut live = ra_jit_live::Live::start().ok();
loop {
    if let Some(outcome) = live.as_mut().and_then(|it| it.poll()) {
        eprintln!("ra-jit: {outcome}");
    }
    // ...your frame
}
```

macOS on Apple silicon only, for now; elsewhere `Live::start` says so and you still get
ra-jit's fast builds. MIT or Apache-2.0.
