//! 将 macOS ChatGPT App 的 Codex 任务状态转换成可供外部设备使用的状态。
//!
//! [`chatgpt`] 负责读取桌面任务，独立的 [`state`] 负责聚合和短时提示，
//! [`device`] 负责输出。状态机不依赖 ChatGPT 的内部 JSON 或键盘协议。

pub mod chatgpt;
pub mod device;
pub mod state;
