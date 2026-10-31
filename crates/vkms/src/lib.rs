//! OpenUUYC 的虚拟显示器：一半是主程序用的客户端，一半是 root 跑的辅助程序。
//!
//! Linux 上凭空多出一块显示器只有一条路：内核的 VKMS 驱动经 configfs 现造一个 DRM
//! 设备，合成器把它当成新插上的显示器接管。写 configfs 要 root，主程序不能是 root，
//! 所以中间隔一个只做这一件事的小程序——它连分辨率以外的东西都不接受。
//!
//! 特权端在 `server/`，编入主程序，由 `openuuyc vkms` 在独立进程中运行。
//! 客户端和特权端只通过 [`protocol`] 约定交互。
pub mod client;
pub mod protocol;
pub mod server;

pub use client::{Client, Disconnected, SOCKET, installed, probe};
pub use protocol::{Reply, Request, Status, Unsupported};
