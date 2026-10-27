//! 零化测试（secrets.md §12.3）：用测试用全局分配器在每次释放时检查内存块，
//! 断言本 crate 的 put / get / stage / commit 路径释放的堆内存里没有残留明文。
//!
//! 测试代码允许 `unsafe`（全局分配器），库代码 `forbid(unsafe_code)`。
//! 本文件只有一个测试，避免并行测试互相干扰。

#![cfg(unix)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

mod common;

use common::{ctx, fresh, name};
use gantry_core::ports::secrets::{Expect, SecretSink, SecretSource, SecretValue};
use gantry_core::secret::Consumer;

/// 哨兵：32 字节，测试中逐字节构造（不以字面量出现在常量区以外的地方也无妨：
/// 分配器只检查堆块）。
const PATTERN_LEN: usize = 32;
static PATTERN: [AtomicUsize; PATTERN_LEN] = [const { AtomicUsize::new(0) }; PATTERN_LEN];
static ARMED: AtomicBool = AtomicBool::new(false);
static LEAKS: AtomicUsize = AtomicUsize::new(0);

struct Checking;

unsafe impl GlobalAlloc for Checking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ARMED.load(Ordering::Relaxed) && layout.size() >= PATTERN_LEN {
            let block = unsafe { std::slice::from_raw_parts(ptr, layout.size()) };
            let first = PATTERN[0].load(Ordering::Relaxed) as u8;
            'outer: for start in 0..=(block.len() - PATTERN_LEN) {
                if block[start] != first {
                    continue;
                }
                for (i, expected) in PATTERN.iter().enumerate() {
                    if block[start + i] != expected.load(Ordering::Relaxed) as u8 {
                        continue 'outer;
                    }
                }
                LEAKS.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Checking = Checking;

fn pattern_value() -> SecretValue {
    let bytes: Vec<u8> = PATTERN
        .iter()
        .map(|b| b.load(Ordering::Relaxed) as u8)
        .collect();
    SecretValue::from_bytes(bytes)
}

#[test]
fn freed_memory_contains_no_plaintext() {
    let seed = std::process::id() as usize ^ 0x5a;
    for (i, slot) in PATTERN.iter().enumerate() {
        slot.store(0x80 | ((seed + i * 37) & 0x7f), Ordering::Relaxed);
    }
    let (_tmp, _key, secrets) = fresh(&[Consumer::Forge]);
    let s = secrets.scoped(Consumer::Forge).unwrap();
    let c = ctx();
    let n = name("k");

    ARMED.store(true, Ordering::SeqCst);
    s.put(&n, pattern_value(), Expect::Any, &c).unwrap();
    for _ in 0..3 {
        let got = s.get(&n).unwrap().unwrap();
        assert_eq!(got.value.expose().len(), PATTERN_LEN);
        drop(got);
    }
    let staged = s.stage(&n, pattern_value(), Expect::Any, &c).unwrap();
    s.commit(&staged, &c).unwrap();
    let old = s.get_version(&n, staged.version).unwrap().unwrap();
    drop(old);
    ARMED.store(false, Ordering::SeqCst);

    assert_eq!(LEAKS.load(Ordering::SeqCst), 0, "释放的堆块中残留了明文");

    // 对照：不零化的普通 Vec 会被检出，证明检查本身有效。
    ARMED.store(true, Ordering::SeqCst);
    let plain: Vec<u8> = PATTERN
        .iter()
        .map(|b| b.load(Ordering::Relaxed) as u8)
        .collect();
    drop(std::hint::black_box(plain));
    ARMED.store(false, Ordering::SeqCst);
    assert_eq!(LEAKS.load(Ordering::SeqCst), 1);
}
