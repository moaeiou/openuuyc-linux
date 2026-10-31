//! 每个桌面登录会话只运行一个控制中心，不依赖可执行文件名称。

/// 第二个进程要不要把已经在跑的控制中心叫到前台，以及顺带交给它的东西。
#[derive(Clone, Copy)]
pub enum Activation<'a> {
    /// 再点一次图标：把窗口叫出来就行。
    Show,
    /// 系统按 `uuremote://` 协议注册拉起的协助链接。链接原样转发，由接手的
    /// 那个进程解析——窗口消息只负责搬运。
    Link(&'a str),
    /// 后台启动，不打扰已经在跑的实例。
    Quiet,
}

mod platform {
    use super::Activation;
    use anyhow::{Context, Result};
    use std::fs::{File, OpenOptions};
    use std::path::PathBuf;

    /// 协作式文件锁随句柄存活，句柄关闭后锁文件本身仍保留。
    pub struct Instance(#[allow(dead_code)] File);

    fn lock_path() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        // 按用户区分，回退到共享 /tmp 目录时也不会在不同账户间冲突。
        base.join(format!("openuuyc-control-center-{}.lock", unsafe {
            libc::getuid()
        }))
    }

    /// 抢实例锁。抢不到就把这次启动的来意交给已经在跑的那个实例，然后自己去退出。
    pub fn acquire(activation: Activation<'_>) -> Result<Option<Instance>> {
        let path = lock_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("创建程序实例保护失败：{}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Instance(file))),
            Err(std::fs::TryLockError::WouldBlock) => match activation {
                // 运行的实例可能是服务，也可能只是收在托盘里，先把它的窗口叫出来。
                // 唤醒失败必须保留原因，不能把“没有窗口”当作成功启动。
                Activation::Show => {
                    crate::ui::control::activate().with_context(|| {
                        format!(
                            "OpenUUYC 实例锁已被占用，但无法唤起已有窗口（锁文件：{}）。\n\
                             请用 pgrep -a -x openuuyc 和 lslocks 检查运行中的实例；\
                             不要删除锁文件，这可能导致两个实例同时运行",
                            path.display()
                        )
                    })?;
                    Ok(None)
                }
                Activation::Quiet => {
                    eprintln!(
                        "OpenUUYC 已在运行，本次后台启动退出（锁文件：{}）。\n\
                         已有实例可能由桌面或终端启动，不属于此 systemd 服务；\
                         请运行 openuuyc 打开窗口，用 pgrep -a -x openuuyc 和 lslocks 检查实例。\n\
                         如需改由服务管理，请先从托盘退出已有实例，再运行 \
                         systemctl --user start openuuyc.service。不要删除锁文件。",
                        path.display()
                    );
                    Ok(None)
                }
                // 链接是用户点出来的，交不出去就得说出来，不能悄悄丢掉。
                Activation::Link(uri) => {
                    crate::ui::control::deliver_link(uri)?;
                    Ok(None)
                }
            },
            Err(std::fs::TryLockError::Error(error)) => {
                eprintln!("无法启动 OpenUUYC：{error}");
                Err(anyhow::Error::new(error).context("锁定程序实例保护失败"))
            }
        }
    }

    /// X11 和 Wayland 没有通用的窗口属性握手机制；实例占用由锁文件决定，
    /// 此处不改动已运行的窗口。
    pub(crate) fn register_window(_window: &winit::window::Window) -> Result<()> {
        Ok(())
    }

    /// 只有原有 Windows 更新流程才会请求已运行的控制中心退出。
    pub(crate) fn take_update_exit() -> bool {
        false
    }
}

pub use platform::{Instance, acquire};

pub(crate) use platform::{register_window, take_update_exit};
