//! 只读检查有线连接的 Halo96 v2 主背光，不改变灯效。
use anyhow::Result;
use codex_status_light::device::halo96::Halo96;

fn main() -> Result<()> {
    let mut keyboard = Halo96::open()?;
    println!("{:#?}", keyboard.read_state()?);
    Ok(())
}
