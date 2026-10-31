//! SIGTERM/SIGINT 走和托盘「退出」同一条收尾路径。
//!
//! `systemctl --user stop openuuyc.service` 发的是 SIGTERM，直接死掉的话本机在线
//! 状态要等服务器超时才掉，正在进行的被控会话也不会走正常收尾；`Restart=on-failure`
//! 的单元更该看到一次干净的退出。winit 不装信号处理，事件循环也没有注册文件描述符
//! 的接口，所以按老办法：处理函数往自建管道里写一个字节（异步信号安全），一个后台
//! 线程读到就通过窗口管理器请求退出。
//!
//! 只有首次信号会请求正常退出。处理函数运行时将两个信号恢复为默认行为，
//! 因而即使退出被拒绝（后台关闭失败）或挂起，下一次 SIGTERM 或 Ctrl+C
//! 仍能直接结束进程，不必只能等待 systemd 的停止超时。
//!
//! 处理函数会串联先前安装的处理函数；signal-hook-registry（tokio 的信号支持）
//! 也会为后续安装的处理函数做同样的事，因此进程内观看端的
//! `tokio::signal::ctrl_c` 仍能收到 Ctrl+C。
//! 对于后台任务或 `nohup` 等场景继承的忽略状态，则继续忽略该信号。
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

const SIGNALS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGINT];

/// 自建管道的写端。信号处理函数只读这个原子量。
static WAKE_WRITE: AtomicI32 = AtomicI32::new(-1);
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// 保存安装本模块前各信号的处理方式；在注册处理函数前只写一次，
/// 之后仅供读取，处理函数也遵循此规则。
struct Previous(std::cell::UnsafeCell<[Option<libc::sigaction>; 2]>);
// SAFETY: 仅由 `install` 在处理函数可能运行之前写入，之后只读。
unsafe impl Sync for Previous {}
static PREVIOUS: Previous = Previous(std::cell::UnsafeCell::new([None, None]));
/// 本模块实际处理的 [`SIGNALS`] 成员。
static HANDLED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

extern "C" fn on_signal(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    // SAFETY: errno 是线程局部变量；下方的 sigaction、write 等操作均为异步信号安全，
    // 并在返回前为被中断的代码恢复 errno。
    let errno = unsafe { *libc::__errno_location() };
    for (index, other) in SIGNALS.into_iter().enumerate() {
        if HANDLED[index].load(Ordering::Relaxed) {
            // SAFETY: 全零的 sigaction 表示 SIG_DFL，且信号掩码为空。
            unsafe {
                let mut default: libc::sigaction = std::mem::zeroed();
                default.sa_sigaction = libc::SIG_DFL;
                libc::sigaction(other, &default, std::ptr::null_mut());
            }
        }
    }
    let fd = WAKE_WRITE.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: 从有效地址向非阻塞管道写入一个字节；
        // 管道已满说明其中已经有待处理的请求。
        unsafe {
            libc::write(fd, std::ptr::from_ref(&byte).cast(), 1);
        }
    }
    if let Some(index) = SIGNALS.iter().position(|&known| known == signal) {
        // SAFETY: 处理函数开始运行后，PREVIOUS 不会再被写入。
        if let Some(previous) = unsafe { (*PREVIOUS.0.get())[index] } {
            // SAFETY: 按原处理函数声明的标志，使用内核传给本处理函数的参数调用它。
            unsafe { chain(&previous, signal, info, context) };
        }
    }
    // SAFETY: 安全依据同上。
    unsafe { *libc::__errno_location() = errno };
}

/// 若先前安装的是函数处理器，则调用该函数。
unsafe fn chain(
    previous: &libc::sigaction,
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let handler = previous.sa_sigaction;
    if handler == libc::SIG_DFL || handler == libc::SIG_IGN {
        return;
    }
    if previous.sa_flags & libc::SA_SIGINFO != 0 {
        // SAFETY: SA_SIGINFO 处理函数接收这三个参数。
        let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
            unsafe { std::mem::transmute(handler) };
        handler(signal, info, context);
    } else {
        // SAFETY: 普通信号处理函数只接收信号编号。
        let handler: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(handler) };
        handler(signal);
    }
}

/// 让 SIGTERM、SIGINT 变成一次正常退出。进程里只会装一次。
pub(crate) fn install() -> Result<()> {
    install_with(|| super::window_manager::send(super::window_manager::Request::Exit).is_ok())
}

/// 安装信号处理函数；每收到一个信号，后台线程就调用一次 `notify`，
/// 没有可通知的接收者时返回 false。
fn install_with(mut notify: impl FnMut() -> bool + Send + 'static) -> Result<()> {
    anyhow::ensure!(
        !INSTALLED.swap(true, Ordering::AcqRel),
        "退出信号处理已经装过了"
    );
    let mut fds = [0i32; 2];
    // SAFETY: 传的是两个 i32 的有效地址，失败会返回负值。
    anyhow::ensure!(
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0,
        "创建退出信号管道失败：{}",
        std::io::Error::last_os_error()
    );
    let [read, write] = fds;
    // 处理函数绝不能因为管道已满而阻塞。
    // SAFETY: `write` 是刚创建的管道写入端。
    anyhow::ensure!(
        unsafe { libc::fcntl(write, libc::F_SETFL, libc::O_NONBLOCK) } == 0,
        "设置退出信号管道失败：{}",
        std::io::Error::last_os_error()
    );
    WAKE_WRITE.store(write, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("openuuyc-signal".into())
        .spawn(move || {
            let mut byte = 0u8;
            // SAFETY: 阻塞读一个字节；管道写端一直开着。
            while unsafe { libc::read(read, std::ptr::from_mut(&mut byte).cast(), 1) } == 1 {
                if !notify() {
                    break;
                }
            }
        })
        .context("启动退出信号线程失败")?;
    for (index, signal) in SIGNALS.into_iter().enumerate() {
        // SAFETY: 将当前信号处理方式查询到已清零的结构体中。
        let current = unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            anyhow::ensure!(
                libc::sigaction(signal, std::ptr::null(), &mut current) == 0,
                "读取信号处理失败：{}",
                std::io::Error::last_os_error()
            );
            current
        };
        if current.sa_sigaction == libc::SIG_IGN {
            tracing::info!(signal, "信号被启动方设为忽略，保持忽略");
            continue;
        }
        // SAFETY: 下方设置 HANDLED 之前，本模块的处理函数不会运行。
        unsafe { (*PREVIOUS.0.get())[index] = Some(current) };
        HANDLED[index].store(true, Ordering::Relaxed);
        // SAFETY: 处理函数只执行异步信号安全的操作；运行期间屏蔽这两个信号，
        // 避免其中一个信号中断另一个的处理。
        let installed = unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            for blocked in SIGNALS {
                libc::sigaddset(&mut action.sa_mask, blocked);
            }
            libc::sigaction(signal, &action, std::ptr::null_mut())
        };
        anyhow::ensure!(
            installed == 0,
            "安装信号处理失败：{}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! 信号处理方式属于进程级状态，因此每个场景都在子进程中运行：
    //! 再次启动本测试程序，筛选到当前测试，并通过环境变量传递场景。
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::mpsc;
    use std::time::Duration;

    const SCENARIO: &str = "OPENUUYC_SIGNAL_SCENARIO";

    fn disposition(signal: libc::c_int) -> libc::sighandler_t {
        // SAFETY: 查询结果写入已清零的结构体。
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            libc::sigaction(signal, std::ptr::null(), &mut current);
            current.sa_sigaction
        }
    }

    fn raise(signal: libc::c_int) {
        // SAFETY: 触发当前场景已处理或预期会终止进程的信号。
        unsafe { libc::raise(signal) };
    }

    /// 在子进程中运行。
    fn scenario(name: &str) {
        let (sender, notified) = mpsc::channel();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let wait = |stream: &mut tokio::signal::unix::Signal| {
            runtime
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(2), stream.recv()).await
                })
                .is_ok()
        };
        match name {
            // 进程内观看端的 tokio 监听器在本模块之后注册。
            "tokio-after" => {
                install_with(move || sender.send(()).is_ok()).unwrap();
                let mut tokio_int = runtime
                    .block_on(async {
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    })
                    .unwrap();
                raise(libc::SIGINT);
                notified.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(wait(&mut tokio_int), "tokio missed SIGINT");
                assert_eq!(disposition(libc::SIGINT), libc::SIG_DFL);
                assert_eq!(disposition(libc::SIGTERM), libc::SIG_DFL);
                println!("first-signal-handled");
                raise(libc::SIGINT);
            }
            // tokio 监听器在本模块之前注册。
            "tokio-before" => {
                let mut tokio_term = runtime
                    .block_on(async {
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    })
                    .unwrap();
                install_with(move || sender.send(()).is_ok()).unwrap();
                raise(libc::SIGTERM);
                notified.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(wait(&mut tokio_term), "tokio missed SIGTERM");
                println!("first-signal-handled");
                raise(libc::SIGTERM);
            }
            // 继承的忽略状态保持不变，不发出退出请求。
            "ignored" => {
                // SAFETY: 仅在当前子进程中将 SIGINT 设置为忽略。
                unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
                install_with(move || sender.send(()).is_ok()).unwrap();
                raise(libc::SIGINT);
                assert!(notified.recv_timeout(Duration::from_millis(300)).is_err());
                assert_eq!(disposition(libc::SIGINT), libc::SIG_IGN);
                println!("first-signal-handled");
                return;
            }
            other => panic!("unknown scenario {other}"),
        }
        println!("survived-second-signal");
    }

    fn run_child(name: &str) -> (std::process::ExitStatus, String) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ui::shutdown::tests::the_first_signal_exits_cleanly_and_the_second_kills",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SCENARIO, name)
            .output()
            .unwrap();
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status, text)
    }

    #[test]
    fn the_first_signal_exits_cleanly_and_the_second_kills() {
        if let Ok(name) = std::env::var(SCENARIO) {
            scenario(&name);
            return;
        }
        for (name, signal) in [
            ("tokio-after", libc::SIGINT),
            ("tokio-before", libc::SIGTERM),
        ] {
            let (status, stdout) = run_child(name);
            assert!(stdout.contains("first-signal-handled"), "{name}: {stdout}");
            assert!(
                !stdout.contains("survived-second-signal"),
                "{name}: {stdout}"
            );
            assert_eq!(status.signal(), Some(signal), "{name}: {status:?}");
        }
        let (status, stdout) = run_child("ignored");
        assert!(stdout.contains("first-signal-handled"), "ignored: {stdout}");
        assert!(status.success(), "ignored: {status:?}");
    }
}
