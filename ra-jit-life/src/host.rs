// Stubs for a native `cargo build` of this guest. Under ra-jit this module is skipped and the
// extern declarations in main.rs are bound to the window's own functions.
#[unsafe(no_mangle)]
pub extern "C" fn set_cell(_x: i64, _y: i64, _rgb: i64) {}
#[unsafe(no_mangle)]
pub extern "C" fn present(_w: i64, _h: i64) {}
#[unsafe(no_mangle)]
pub extern "C" fn sleep_ms(ms: i64) {
    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
}
