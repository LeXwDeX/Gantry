//! `gantry-worker`：沙盒内的执行者（worker.md §6.1）。
//!
//! ```text
//! gantry-worker run        # 读 $GANTRY_JOB / $GANTRY_WORK / $GANTRY_OUT，按 Worker 协议执行
//! gantry-worker version
//! ```

#[cfg(unix)]
fn main() {
    use std::io;
    use std::sync::atomic::{AtomicPtr, Ordering};

    use gantry_worker::runner::{run, AbortFlag, Exit, RunEnv};

    static SIGNAL_FLAG: AtomicPtr<std::sync::atomic::AtomicBool> =
        AtomicPtr::new(std::ptr::null_mut());

    extern "C" fn on_signal(_: libc::c_int) {
        // 只做原子读写：async-signal-safe。第二次信号同样只是置位。
        let flag = SIGNAL_FLAG.load(Ordering::SeqCst);
        if !flag.is_null() {
            unsafe { (*flag).store(true, Ordering::SeqCst) };
        }
    }

    let command = std::env::args().nth(1).unwrap_or_default();
    match command.as_str() {
        "run" => {
            let abort = AbortFlag::new();
            SIGNAL_FLAG.store(
                abort.atomic() as *const _ as *mut std::sync::atomic::AtomicBool,
                Ordering::SeqCst,
            );
            let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            unsafe {
                libc::signal(libc::SIGTERM, handler);
                libc::signal(libc::SIGINT, handler);
            }
            let env = RunEnv::from_env();
            let code = run(&env, &abort, io::stdout().lock(), io::stderr().lock());
            std::process::exit(code as i32);
        }
        "version" => {
            println!("gantry-worker {}", env!("CARGO_PKG_VERSION"));
        }
        _ => {
            eprintln!("usage: gantry-worker run | version");
            std::process::exit(Exit::BadJob as i32);
        }
    }
}

/// 沙盒只有 Linux；其他平台只为让工作区能编译。
#[cfg(not(unix))]
fn main() {
    eprintln!("gantry-worker only runs inside a unix sandbox");
    std::process::exit(11);
}
