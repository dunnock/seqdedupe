//! Non-inlined entry points so `scripts/check_lockfree.sh` can inspect the
//! machine code of the hot paths.
use seqdedupe::*;
use std::hint::black_box;

#[inline(never)]
pub fn probe_register(w: &mut Worker, s: u64) -> Registration {
    w.register(s)
}

#[inline(never)]
pub fn probe_snapshot(w: &mut Worker) -> Snapshot {
    w.snapshot()
}

fn main() {
    let windows = [
        Window::WatermarkOnly,
        Window::Bits64,
        Window::Bits128,
        Window::Bits256,
        Window::Bits512,
    ];
    for w in windows {
        let mut ws = new_workers(w, std::num::NonZeroUsize::new(1).unwrap(), 0, 0).unwrap();
        for s in 0..black_box(1000) {
            black_box(probe_register(&mut ws[0], s));
        }
        black_box(probe_snapshot(&mut ws[0]));
    }
}
