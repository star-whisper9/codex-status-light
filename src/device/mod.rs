//! 将应用状态交给具体输出设备，状态层不依赖灯光参数。
pub mod alice75;
mod color;
pub mod halo96;
mod hid;
mod via;

use crate::state::Status;
use anyhow::Result;

/// 输出设备的生命周期；退出时应显式调用 `shutdown` 并处理错误。
pub trait Output {
    fn render(&mut self, status: Status) -> Result<()>;
    fn shutdown(&mut self) -> Result<()>;
}
