# Codex Status Light

读取 macOS 版 ChatGPT 桌面应用的本地 Codex 任务状态，将多个会话聚合成单一状态，输出到终端（JSON），也内置了两款键盘的演示：驱动 Feker Alice75 的 Logo 灯、Nuphy Halo96 v2 的背光。

## 构建与运行

环境要求：macOS、Rust 工具链、Xcode Command Line Tools。

```sh
cargo build --locked

# 默认模式：状态变化时向终端输出一行 JSON，Ctrl-C 退出
cargo run --locked --

# 只读取一次当前状态
cargo run --locked -- --once

# 运行指定时长（秒）后退出
cargo run --locked -- --seconds 10

# 驱动 USB 连接的 Alice75 Logo 灯（退出时恢复 wave 二档）
cargo run --locked -- --device alice75

# 驱动 USB 连接的 Halo96 v2 背光（退出时恢复接管前设置）
cargo run --locked -- --device halo96

# 只读检查 Halo96 v2 当前背光配置
cargo run --locked --example read_halo96

# 只读检查 Alice75 当前 Logo 灯配置
cargo run --locked --example read_logo
```

数据目录默认取 `$CODEX_HOME`（未设置则为 `~/.codex`），可通过 `--codex-home /path/to/.codex` 指定。使用设备输出时，请勿同时用 VIA 等工具操作同一灯区。

终端输出示例：

```json
{ "state": "running", "roots": 1 }
```

## 打包与开机自启

如需在后台常驻驱动键盘灯效，可打包为独立目录并通过 `launchd` 管理：

```sh
cargo build --release --locked
python3 scripts/package.py
```

打包结果位于 `target/package/CodexStatusLight/`（同时生成同目录 ZIP）。将该目录放到固定位置后使用内置脚本管理：

- `Start.command`：启动服务并开启登录自启
- `StartHalo96.command`：启动 Halo96 v2 背光服务并开启登录自启；切换设备前先运行 `Stop.command`
- `Stop.command`：停止服务并取消自启；Alice75 恢复 wave 二档，Halo96 恢复接管前背光
- `Status.command`：查看服务状态与最近日志
- 日志路径：`~/Library/Logs/CodexStatusLight/`（`state.log`、`error.log`）
- LaunchAgent：`~/Library/LaunchAgents/local.codex-status-light.plist`

移动或更新程序目录前，请先运行 `Stop.command`。

## 状态优先级与灯效映射

多个任务并存时，按以下优先级从高到低决定当前输出状态：

| 优先级 | 状态                     | 触发条件                         | Alice75 预设 | 颜色     | 速度 |
| ------ | ------------------------ | -------------------------------- | ------------ | -------- | ---- |
| 1      | `Closed`                 | ChatGPT 进程退出（仅关窗口不算） | wave         | 预设默认 | 二档 |
| 2      | `Error`                  | 出现新错误，持续 5 秒            | fixed wave   | 红色     | 五档 |
| 3      | `Waiting`                | 任一任务等待用户输入或审批       | fixed wave   | 橙色     | 五档 |
| 4      | `Completed`              | 主任务完成，持续 3 秒            | fixed wave   | 绿色     | 五档 |
| 5      | `Running { roots: 1 }`   | 1 个主任务运行中                 | fixed wave   | 蓝色     | 二档 |
| 5      | `Running { roots: 2.. }` | 2 个及以上主任务运行中           | fixed wave   | 蓝色     | 三档 |
| 6      | `Idle`                   | 应用运行中，无活跃任务           | light        | 绿色     | -    |

说明：

- **主任务与子代理**：主任务派生子代理不会增加 `roots` 计数；子代理完成不触发绿灯，但子代理的等待审批和错误会正常触发 `Waiting` 或 `Error`。
- **事件去重与计时**：程序启动或应用重启时已有的历史任务仅作为基线，不会补播完成或错误提示。同类新事件会重置倒计时；被更高优先级状态遮盖的短时提示在后台正常计时到期，不会排队补播。

## 实现结构

| 模块           | 说明                                                          |
| -------------- | ------------------------------------------------------------- |
| `src/chatgpt/` | 读取本地 SQLite 与 IPC 流，合并增量状态并输出快照             |
| `src/state.rs` | 纯状态机：处理任务计数、优先级判定、事件去重与倒计时          |
| `src/device/`  | `Output` 输出接口及 Alice75、Halo96 v2（IOKit HID / VIA）实现 |
| `src/main.rs`  | 命令行参数解析、主循环与退出清理                              |

### ChatGPT 本地数据源

`src/chatgpt/` 直接只读访问桌面应用的本地文件与 Socket，无需 API Key：

- **SQLite**：只读查询 `state_5.sqlite` 与 `thread_history_1.sqlite` 发现本地任务。
- **IPC**：以 follower 身份连接 `ipc/ipc.sock`（4 字节小端长度前缀 + JSON），订阅 `thread-stream-state-changed` 事件流并合并 Immer patches；数据修订号（revision）断档时自动请求完整快照重同步。兼容性依据必要字段、状态值与增量结构校验，不限制客户端或消息版本号。
- **状态判定**：普通工具调用不计入 `Waiting`，仅明确的输入/审批请求会触发；普通命令非零退出或手动停止不计入 `Error`，仅任务失败、`systemError` 或连续自动审核拒绝（`autoReviewInterruptionWarning`）会触发。

> 注意：以上均为 ChatGPT 客户端内部接口，客户端更新后可能需要同步调整解析逻辑。

### Alice75 硬件协议与自定义设备

Alice75 适配器通过 macOS IOKit 连接设备（VID `0x36B0`、PID `0x3016`，VIA usage page `0xFF60`、usage `0x61`），仅修改 Logo 灯所在的通道 `2`（效果、速度、按 [VIA 算法](https://github.com/the-via/app/blob/main/src/utils/color-math.ts)转换的 8 位色相与饱和度），每次写入后读回校验，不改键盘背光与亮度，也不写入 EEPROM。

### Halo96 v2 背光

根据 `nuphy-halo96-v2-via.json`，设备 VID 为 `0x19F5`、PID 为 `0x3302`，背光使用通道 `3`。`Solid Color` 编号为 `1`，`Breathing` 为 `4`；速度取值为 `0..255`，不沿用 Alice75 的五档编号。

| 状态                | 背光效果                         | 颜色      | 速度值 |
| ------------------- | -------------------------------- | --------- | ------ |
| `Idle`              | Solid Color 常亮                 | `#01ff8a` | 无关   |
| 单主任务运行        | Breathing 呼吸                   | `#0000ff` | 64     |
| 多主任务运行        | Breathing 呼吸                   | `#0000ff` | 128    |
| `Waiting`           | Breathing 呼吸                   | `#ff6701` | 255    |
| `Completed`         | Breathing 呼吸，3 秒             | `#01ff8a` | 255    |
| `Error`             | Breathing 呼吸，5 秒             | `#ff0000` | 255    |
| `Closed` / 程序退出 | 恢复连接时读取的效果、速度和颜色 | 原色      | 原值   |

只控制 JSON 开放的 Backlight，保持亮度，不写 EEPROM。侧灯自定义键码仅提供切换动作，没有可直接设置颜色、速度的 VIA 通道，因此此适配器不控制侧灯。原态以每次连接时的读值为准。

需要键盘有线 USB 模式（[NuPhy VIA 指南](https://nuphy.com/pages/via-usage-guide-for-nuphy-keyboards)）。2.4G 接收器的设备 ID 为 `0x19F5:0x3247`，厂商接口为 `0xFF31/0x74`；当前适配器不支持该接口。公开的 [Halo96 v2 无线代码](https://github.com/nuphy-src/qmk_firmware/blob/nuphy-keyboards/keyboards/nuphy/halo96_v2/ansi/rf.c)没有 VIA 转发分支；增加无线控制需要另行取得或解析接收器协议。

如需接入其他硬件（如串口指示灯、菜单栏组件等），实现 `device::Output` 并在 `src/main.rs` 中挂载即可：

```rust
use anyhow::Result;
use codex_status_light::{device::Output, state::Status};

struct MyDevice;

impl Output for MyDevice {
    fn render(&mut self, status: Status) -> Result<()> {
        println!("{status:?}");
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}
```
