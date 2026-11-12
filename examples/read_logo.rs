//! 只读检查 Alice75 连接，不改灯效；连接在示例退出时释放。
use anyhow::Result;
use codex_status_light::device::alice75::Alice75;

fn main() -> Result<()> {
    let mut keyboard = Alice75::open()?;
    println!("{:?}", keyboard.read_state()?);
    Ok(())
}
