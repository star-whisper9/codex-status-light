//! VIA 灯光协议，只允许设置效果、速度和颜色；亮度只读，不参与写入校验。
use super::hid::Transport;
use anyhow::{Result, ensure};
use std::{marker::PhantomData, rc::Rc};

/// VIA 读回的灯光参数；亮度只读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LightingState {
    pub brightness: u8,
    pub effect: u8,
    pub speed: u8,
    pub hue: u8,
    pub saturation: u8,
}

pub(super) struct ViaLighting {
    transport: Box<dyn Transport>,
    channel: u8,
    max_effect: u8,
    max_speed: u8,
    name: &'static str,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ViaLighting {
    pub(super) fn new(
        transport: Box<dyn Transport>,
        channel: u8,
        max_effect: u8,
        max_speed: u8,
        name: &'static str,
    ) -> Self {
        Self {
            transport,
            channel,
            max_effect,
            max_speed,
            name,
            _thread_bound: PhantomData,
        }
    }
    fn command(&mut self, command: u8, value: u8, data: &[u8]) -> Result<[u8; 32]> {
        let mut packet = [0; 32];
        packet[..3].copy_from_slice(&[command, self.channel, value]);
        packet[3..3 + data.len()].copy_from_slice(data);
        let response = self.transport.exchange(packet)?;
        ensure!(
            response[..3] == packet[..3],
            "Unexpected VIA response: {response:02x?}"
        );
        Ok(response)
    }

    /// 读取当前灯光通道四项参数；不读取按键或其他通道。
    pub fn read_state(&mut self) -> Result<LightingState> {
        let brightness = self.command(8, 1, &[])?[3];
        let effect = self.command(8, 2, &[])?[3];
        let speed = self.command(8, 3, &[])?[3];
        let color = self.command(8, 4, &[])?;
        Ok(LightingState {
            brightness,
            effect,
            speed,
            hue: color[3],
            saturation: color[4],
        })
    }

    /// 保留亮度；`hue=None` 时同时保留色调与饱和度，写入后校验效果、速度与颜色。
    pub fn apply(
        &mut self,
        effect: u8,
        speed: u8,
        hue: Option<u8>,
        saturation: u8,
    ) -> Result<LightingState> {
        ensure!(
            effect <= self.max_effect,
            "{} effect must be in 0..={}",
            self.name,
            self.max_effect
        );
        ensure!(
            speed <= self.max_speed,
            "{} speed must be in 0..={}",
            self.name,
            self.max_speed
        );
        let before = self.read_state()?;
        let mut expected = LightingState {
            effect,
            speed,
            ..before
        };
        if let Some(hue) = hue {
            expected.hue = hue;
            expected.saturation = saturation;
            if (hue, saturation) != (before.hue, before.saturation) {
                self.command(7, 4, &[hue, saturation])?;
            }
        }
        if speed != before.speed {
            self.command(7, 3, &[speed])?;
        }
        if effect != before.effect {
            self.command(7, 2, &[effect])?;
        }
        let actual = self.read_state()?;
        // 亮度未写入，可能随灯效或用户操作变化，不作为本次写入的确认条件。
        ensure!(
            actual.effect == expected.effect
                && actual.speed == expected.speed
                && actual.hue == expected.hue
                && actual.saturation == expected.saturation,
            "{} readback mismatch: expected {expected:?}, received {actual:?}",
            self.name
        );
        Ok(actual)
    }
}
