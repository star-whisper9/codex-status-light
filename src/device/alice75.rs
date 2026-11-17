//! Alice75 的 VIA Logo 通道。仅设置运行时参数，不发送 EEPROM 保存指令。
use super::{
    Output,
    color::{BLUE, GREEN, ORANGE, RED, via_color},
    hid::{self, Transport},
    via::ViaLighting,
};
use crate::state::Status;
use anyhow::Result;

pub use super::via::LightingState as LogoState;

/// 在创建线程使用的键盘连接；IOKit 回调绑定该线程的运行循环。
pub struct Alice75 {
    lighting: ViaLighting,
}

impl Alice75 {
    /// 打开唯一匹配的 Alice75 VIA 接口，不抢占键盘输入接口。
    pub fn open() -> Result<Self> {
        Ok(Self::with_transport(hid::open(0x36b0, 0x3016, "Alice75")?))
    }

    fn with_transport(transport: Box<dyn Transport>) -> Self {
        Self {
            lighting: ViaLighting::new(transport, 2, 6, 4, "Logo"),
        }
    }

    /// 读取 Logo 四项参数；不读取按键或其他灯光通道。
    pub fn read_state(&mut self) -> Result<LogoState> {
        self.lighting.read_state()
    }

    /// 保留亮度；`hue=None` 时同时保留色调与饱和度，写入后校验效果、速度与颜色。
    pub fn apply(
        &mut self,
        effect: u8,
        speed: u8,
        hue: Option<u8>,
        saturation: u8,
    ) -> Result<LogoState> {
        self.lighting.apply(effect, speed, hue, saturation)
    }
}

fn parameters(status: Status) -> (u8, u8, Option<u32>) {
    match status {
        Status::Closed => (1, 1, None),
        Status::Idle => (5, 0, Some(GREEN)),
        Status::Running { roots } => (2, if roots >= 2 { 2 } else { 1 }, Some(BLUE)),
        Status::Waiting => (2, 4, Some(ORANGE)),
        Status::Completed => (2, 4, Some(GREEN)),
        Status::Error => (2, 4, Some(RED)),
    }
}

impl Output for Alice75 {
    fn render(&mut self, status: Status) -> Result<()> {
        let (effect, speed, color) = parameters(status);
        let (hue, saturation) = color
            .map(via_color)
            .map(|(hue, saturation)| (Some(hue), saturation))
            .unwrap_or((None, 255));
        self.apply(effect, speed, hue, saturation)?;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.render(Status::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    struct Fake {
        state: LogoState,
        writes: Rc<RefCell<Vec<[u8; 32]>>>,
        ignore: bool,
        wrong: bool,
    }
    impl Transport for Fake {
        fn exchange(&mut self, mut packet: [u8; 32]) -> Result<[u8; 32]> {
            assert_eq!(packet[1], 2);
            assert!(matches!(packet[0], 7 | 8));
            if packet[0] == 7 {
                self.writes.borrow_mut().push(packet);
                assert!(matches!(packet[2], 2..=4));
                if !self.ignore {
                    match packet[2] {
                        2 => self.state.effect = packet[3],
                        3 => self.state.speed = packet[3],
                        4 => {
                            self.state.hue = packet[3];
                            self.state.saturation = packet[4];
                        }
                        _ => unreachable!(),
                    }
                }
            }
            match packet[2] {
                1 => packet[3] = self.state.brightness,
                2 => packet[3] = self.state.effect,
                3 => packet[3] = self.state.speed,
                4 => {
                    packet[3] = self.state.hue;
                    packet[4] = self.state.saturation;
                }
                _ => unreachable!(),
            }
            if self.wrong {
                packet[0] = 0xff;
            }
            Ok(packet)
        }
    }
    fn fake(ignore: bool, wrong: bool) -> Fake {
        Fake {
            state: LogoState {
                brightness: 180,
                effect: 2,
                speed: 4,
                hue: 171,
                saturation: 200,
            },
            writes: Rc::new(RefCell::new(Vec::new())),
            ignore,
            wrong,
        }
    }
    #[test]
    fn preserves_brightness_and_color_when_unspecified() {
        let fake = fake(false, false);
        let writes = fake.writes.clone();
        let mut keyboard = Alice75::with_transport(Box::new(fake));
        let before = keyboard.read_state().unwrap();
        let actual = keyboard.apply(1, 1, None, 255).unwrap();
        assert_eq!(
            actual,
            LogoState {
                effect: 1,
                speed: 1,
                ..before
            }
        );
        keyboard.render(Status::Idle).unwrap();
        assert_eq!(
            keyboard.read_state().unwrap(),
            LogoState {
                effect: 5,
                speed: 0,
                hue: 108,
                saturation: 254,
                ..before
            }
        );
        assert!(
            writes
                .borrow()
                .iter()
                .all(|p| p[0] == 7 && p[1] == 2 && matches!(p[2], 2..=4))
        );
    }
    #[test]
    fn failures_are_reported() {
        let mut keyboard = Alice75::with_transport(Box::new(fake(true, false)));
        assert!(
            keyboard
                .apply(5, 0, Some(85), 255)
                .unwrap_err()
                .to_string()
                .contains("readback mismatch")
        );
        let mut keyboard = Alice75::with_transport(Box::new(fake(false, true)));
        assert!(
            keyboard
                .read_state()
                .unwrap_err()
                .to_string()
                .contains("Unexpected VIA response")
        );
    }
    #[test]
    fn invalid_input_never_writes() {
        let fake = fake(false, false);
        let writes = fake.writes.clone();
        let mut keyboard = Alice75::with_transport(Box::new(fake));
        assert!(keyboard.apply(7, 1, None, 255).is_err());
        assert!(keyboard.apply(1, 5, None, 255).is_err());
        assert!(writes.borrow().is_empty());
    }
    #[test]
    fn palette_matches_via_hex_input() {
        assert_eq!(via_color(0x0000ff), (170, 255));
        assert_eq!(via_color(0x01ff8a), (108, 254));
        assert_eq!(via_color(0xff0000), (0, 255));
        assert_eq!(via_color(0xff6701), (17, 254));
        let mut keyboard = Alice75::with_transport(Box::new(fake(false, false)));
        keyboard.render(Status::Waiting).unwrap();
        let state = keyboard.read_state().unwrap();
        assert_eq!((state.hue, state.saturation), (17, 254));
        assert_eq!(state.brightness, 180);
    }

    #[test]
    fn status_mapping_matches_logo_policy() {
        assert_eq!(parameters(Status::Closed), (1, 1, None));
        assert_eq!(parameters(Status::Idle), (5, 0, Some(GREEN)));
        assert_eq!(parameters(Status::Running { roots: 1 }), (2, 1, Some(BLUE)));
        assert_eq!(parameters(Status::Running { roots: 2 }), (2, 2, Some(BLUE)));
        assert_eq!(parameters(Status::Waiting), (2, 4, Some(ORANGE)));
        assert_eq!(parameters(Status::Completed), (2, 4, Some(GREEN)));
        assert_eq!(parameters(Status::Error), (2, 4, Some(RED)));
    }
}
