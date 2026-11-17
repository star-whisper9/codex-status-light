//! Halo96 v2 主背光输出；退出时恢复接管前的效果、速度与颜色。
use super::{
    Output,
    color::{BLUE, GREEN, ORANGE, RED, via_color},
    hid::{self, Transport},
    via::ViaLighting,
};
use crate::state::Status;
use anyhow::Result;

pub use super::via::LightingState as BacklightState;

/// 仅控制 VIA 通道 3；连接与操作必须在同一线程完成。
pub struct Halo96 {
    lighting: ViaLighting,
    original: BacklightState,
}

impl Halo96 {
    /// 打开有线 USB VIA 接口并读取原状态，不改变灯光。
    pub fn open() -> Result<Self> {
        Self::with_transport(hid::open(0x19f5, 0x3302, "Halo96 v2")?)
    }

    fn with_transport(transport: Box<dyn Transport>) -> Result<Self> {
        let mut lighting = ViaLighting::new(transport, 3, 42, 255, "Halo96 backlight");
        let original = lighting.read_state()?;
        Ok(Self { lighting, original })
    }

    /// 读取主背光参数，不访问侧灯。
    pub fn read_state(&mut self) -> Result<BacklightState> {
        self.lighting.read_state()
    }

    /// 保留当前亮度；`hue=None` 保留颜色，写入后读回校验。
    pub fn apply(
        &mut self,
        effect: u8,
        speed: u8,
        hue: Option<u8>,
        saturation: u8,
    ) -> Result<BacklightState> {
        self.lighting.apply(effect, speed, hue, saturation)
    }

    fn restore(&mut self) -> Result<()> {
        let original = self.original;
        // 用户接管期间可调整亮度，因此恢复时只取原效果、速度和颜色。
        self.apply(
            original.effect,
            original.speed,
            Some(original.hue),
            original.saturation,
        )?;
        Ok(())
    }
}

impl Output for Halo96 {
    fn render(&mut self, status: Status) -> Result<()> {
        let (effect, speed, color) = match status {
            Status::Closed => return self.restore(),
            Status::Idle => (1, 0, GREEN),
            Status::Running { roots } => (4, if roots >= 2 { 128 } else { 64 }, BLUE),
            Status::Waiting => (4, 255, ORANGE),
            Status::Completed => (4, 255, GREEN),
            Status::Error => (4, 255, RED),
        };
        let (hue, saturation) = via_color(color);
        self.apply(effect, speed, Some(hue), saturation)?;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.restore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    struct Memory {
        state: BacklightState,
        packets: Vec<[u8; 32]>,
        ignore_writes: bool,
        wrong_reply: bool,
        changing_brightness: bool,
    }

    struct Fake(Rc<RefCell<Memory>>);

    impl Transport for Fake {
        fn exchange(&mut self, mut packet: [u8; 32]) -> Result<[u8; 32]> {
            let mut memory = self.0.borrow_mut();
            memory.packets.push(packet);
            if packet[0] == 8 && packet[2] == 1 && memory.changing_brightness {
                memory.state.brightness = memory.state.brightness.wrapping_add(1);
            }
            assert_eq!(packet[1], 3);
            assert!(matches!(packet[0], 7 | 8));
            assert!(matches!(packet[2], 1..=4));
            if packet[0] == 7 {
                assert!(matches!(packet[2], 2..=4));
                if !memory.ignore_writes {
                    match packet[2] {
                        2 => memory.state.effect = packet[3],
                        3 => memory.state.speed = packet[3],
                        4 => {
                            memory.state.hue = packet[3];
                            memory.state.saturation = packet[4];
                        }
                        _ => unreachable!(),
                    }
                }
            }
            match packet[2] {
                1 => packet[3] = memory.state.brightness,
                2 => packet[3] = memory.state.effect,
                3 => packet[3] = memory.state.speed,
                4 => {
                    packet[3] = memory.state.hue;
                    packet[4] = memory.state.saturation;
                }
                _ => unreachable!(),
            }
            if memory.wrong_reply {
                packet[0] = 0xff;
            }
            Ok(packet)
        }
    }

    fn memory() -> Rc<RefCell<Memory>> {
        Rc::new(RefCell::new(Memory {
            state: BacklightState {
                brightness: 180,
                effect: 12,
                speed: 73,
                hue: 35,
                saturation: 192,
            },
            packets: Vec::new(),
            ignore_writes: false,
            wrong_reply: false,
            changing_brightness: false,
        }))
    }

    #[test]
    fn opening_reads_only_and_restores_original_with_current_brightness() {
        let memory = memory();
        let original = memory.borrow().state;
        let mut keyboard = Halo96::with_transport(Box::new(Fake(memory.clone()))).unwrap();
        assert_eq!(memory.borrow().packets.len(), 4);
        assert!(memory.borrow().packets.iter().all(|p| p[0] == 8));
        keyboard.render(Status::Running { roots: 2 }).unwrap();
        memory.borrow_mut().state.brightness = 97;
        keyboard.render(Status::Closed).unwrap();
        assert_eq!(
            keyboard.read_state().unwrap(),
            BacklightState {
                brightness: 97,
                ..original
            }
        );
        keyboard.render(Status::Error).unwrap();
        keyboard.shutdown().unwrap();
        assert_eq!(
            keyboard.read_state().unwrap(),
            BacklightState {
                brightness: 97,
                ..original
            }
        );
    }

    #[test]
    fn status_colors_and_speeds_match_policy() {
        let memory = memory();
        let mut keyboard = Halo96::with_transport(Box::new(Fake(memory.clone()))).unwrap();
        for (status, effect, speed, hue, saturation) in [
            (Status::Idle, 1, 0, 108, 254),
            (Status::Running { roots: 1 }, 4, 64, 170, 255),
            (Status::Running { roots: 2 }, 4, 128, 170, 255),
            (Status::Waiting, 4, 255, 17, 254),
            (Status::Completed, 4, 255, 108, 254),
            (Status::Error, 4, 255, 0, 255),
        ] {
            keyboard.render(status).unwrap();
            assert_eq!(
                keyboard.read_state().unwrap(),
                BacklightState {
                    brightness: 180,
                    effect,
                    speed,
                    hue,
                    saturation,
                }
            );
        }
    }

    #[test]
    fn channel_ids_and_effect_speed_boundaries() {
        let memory = memory();
        let mut keyboard = Halo96::with_transport(Box::new(Fake(memory.clone()))).unwrap();
        keyboard.apply(0, 0, None, 255).unwrap();
        let state = keyboard.apply(42, 255, Some(255), 0).unwrap();
        assert_eq!(
            (state.effect, state.speed, state.hue, state.saturation),
            (42, 255, 255, 0)
        );
        assert_eq!(state.brightness, 180);
        let count = memory.borrow().packets.len();
        assert!(keyboard.apply(43, 0, None, 255).is_err());
        assert_eq!(memory.borrow().packets.len(), count);
        assert!(
            memory
                .borrow()
                .packets
                .iter()
                .all(|p| p[1] == 3 && (p[0] == 8 || (p[0] == 7 && matches!(p[2], 2..=4))))
        );
    }

    #[test]
    fn readback_and_unknown_responses_fail_fast() {
        let memory = memory();
        let mut keyboard = Halo96::with_transport(Box::new(Fake(memory.clone()))).unwrap();
        memory.borrow_mut().ignore_writes = true;
        assert!(
            keyboard
                .apply(1, 0, Some(170), 255)
                .unwrap_err()
                .to_string()
                .contains("readback mismatch")
        );
        memory.borrow_mut().wrong_reply = true;
        assert!(
            keyboard
                .read_state()
                .unwrap_err()
                .to_string()
                .contains("Unexpected VIA response")
        );
        assert!(Halo96::with_transport(Box::new(Fake(memory))).is_err());
    }
    #[test]
    fn changing_readonly_brightness_does_not_fail_color_write() {
        let memory = memory();
        memory.borrow_mut().changing_brightness = true;
        let mut keyboard = Halo96::with_transport(Box::new(Fake(memory.clone()))).unwrap();
        keyboard.render(Status::Running { roots: 1 }).unwrap();
        assert_eq!(memory.borrow().state.hue, 170);
    }
}
