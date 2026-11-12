//! Alice75 的 VIA Logo 通道。仅设置运行时参数，不发送 EEPROM 保存指令。
use super::Output;
use crate::state::Status;
use anyhow::{Result, bail, ensure};
use std::{marker::PhantomData, rc::Rc};

/// VIA 读回的 Logo 参数；亮度只读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogoState {
    pub brightness: u8,
    pub effect: u8,
    pub speed: u8,
    pub hue: u8,
    pub saturation: u8,
}

trait Transport {
    fn exchange(&mut self, packet: [u8; 32]) -> Result<[u8; 32]>;
}

/// 在创建线程使用的键盘连接；IOKit 回调绑定该线程的运行循环。
pub struct Alice75 {
    transport: Box<dyn Transport>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl Alice75 {
    /// 打开唯一匹配的 Alice75 VIA 接口，不抢占键盘输入接口。
    pub fn open() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self::with_transport(Box::new(macos::Hid::open()?)))
        }
        #[cfg(not(target_os = "macos"))]
        {
            bail!("Alice75 output is supported only on macOS")
        }
    }

    fn with_transport(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            _thread_bound: PhantomData,
        }
    }

    fn command(&mut self, command: u8, value: u8, data: &[u8]) -> Result<[u8; 32]> {
        let mut packet = [0; 32];
        packet[..3].copy_from_slice(&[command, 2, value]);
        packet[3..3 + data.len()].copy_from_slice(data);
        let response = self.transport.exchange(packet)?;
        ensure!(
            response[..3] == packet[..3],
            "Unexpected VIA response: {response:02x?}"
        );
        Ok(response)
    }

    /// 读取 Logo 四项参数；不读取按键或其他灯光通道。
    pub fn read_state(&mut self) -> Result<LogoState> {
        let brightness = self.command(8, 1, &[])?[3];
        let effect = self.command(8, 2, &[])?[3];
        let speed = self.command(8, 3, &[])?[3];
        let color = self.command(8, 4, &[])?;
        Ok(LogoState {
            brightness,
            effect,
            speed,
            hue: color[3],
            saturation: color[4],
        })
    }

    /// 保留亮度；`hue=None` 时同时保留色调与饱和度，写入后完整读回校验。
    pub fn apply(
        &mut self,
        effect: u8,
        speed: u8,
        hue: Option<u8>,
        saturation: u8,
    ) -> Result<LogoState> {
        ensure!(effect <= 6, "Logo effect must be in 0..=6");
        ensure!(speed <= 4, "Logo speed must be in 0..=4");
        let before = self.read_state()?;
        let mut expected = LogoState {
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
        ensure!(
            actual == expected,
            "Logo readback mismatch: expected {expected:?}, received {actual:?}"
        );
        Ok(actual)
    }
}

const BLUE: u32 = 0x0000ff;
const GREEN: u32 = 0x01ff8a;
const RED: u32 = 0xff0000;
const ORANGE: u32 = 0xff6701;

/// 与 VIA 十六进制颜色输入相同，转换为 0..255 的色相、饱和度并四舍五入。
/// 亮度是独立设置，不取 RGB 的 value 覆盖键盘当前亮度。
/// 算法来源：<https://github.com/the-via/app/blob/main/src/utils/color-math.ts>。
fn via_color(rgb: u32) -> (u8, u8) {
    let r = ((rgb >> 16) & 255) as f64;
    let g = ((rgb >> 8) & 255) as f64;
    let b = (rgb & 255) as f64;
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    let hue = if delta == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    let saturation = if max == 0.0 { 0.0 } else { delta / max };
    (
        (255.0 * hue / 6.0).round() as u8,
        (255.0 * saturation).round() as u8,
    )
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

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::{
        cell::UnsafeCell,
        collections::VecDeque,
        ffi::{c_char, c_void},
        ptr,
        time::{Duration, Instant},
    };
    type Ref = *mut c_void;
    type Callback = unsafe extern "C" fn(Ref, i32, Ref, i32, u32, *mut u8, isize);
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: Ref;
        fn CFStringCreateWithCString(alloc: Ref, text: *const c_char, encoding: u32) -> Ref;
        fn CFRelease(value: Ref);
        fn CFRetain(value: Ref) -> Ref;
        fn CFNumberGetValue(value: Ref, kind: i32, output: *mut c_void) -> bool;
        fn CFSetGetCount(set: Ref) -> isize;
        fn CFSetGetValues(set: Ref, values: *mut Ref);
        fn CFRunLoopGetCurrent() -> Ref;
        fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: bool) -> i32;
    }
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDManagerCreate(alloc: Ref, options: u32) -> Ref;
        fn IOHIDManagerSetDeviceMatching(manager: Ref, matching: Ref);
        fn IOHIDManagerCopyDevices(manager: Ref) -> Ref;
        fn IOHIDDeviceGetProperty(device: Ref, key: Ref) -> Ref;
        fn IOHIDDeviceOpen(device: Ref, options: u32) -> i32;
        fn IOHIDDeviceClose(device: Ref, options: u32) -> i32;
        fn IOHIDDeviceScheduleWithRunLoop(device: Ref, run_loop: Ref, mode: Ref);
        fn IOHIDDeviceUnscheduleFromRunLoop(device: Ref, run_loop: Ref, mode: Ref);
        fn IOHIDDeviceRegisterInputReportCallback(
            device: Ref,
            buffer: *mut u8,
            length: isize,
            callback: Callback,
            context: Ref,
        );
        fn IOHIDDeviceSetReport(
            device: Ref,
            kind: i32,
            id: isize,
            report: *const u8,
            length: isize,
        ) -> i32;
    }

    struct Owned(Ref);
    impl Owned {
        fn new(value: Ref, operation: &str) -> Result<Self> {
            ensure!(!value.is_null(), "{operation} returned null");
            Ok(Self(value))
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }

    struct Inbox {
        buffer: [u8; 64],
        responses: VecDeque<Result<[u8; 32]>>,
    }

    pub(super) struct Hid {
        device: Owned,
        _manager: Owned,
        inbox: Box<UnsafeCell<Inbox>>,
        run_loop: Ref,
        mode: Ref,
    }

    unsafe extern "C" fn receive(
        context: Ref,
        result: i32,
        _: Ref,
        _: i32,
        _: u32,
        data: *mut u8,
        length: isize,
    ) {
        // context 与报告缓冲区都在 Box 内，注销和关闭设备之后才释放。
        let response = if result != 0 {
            Err(anyhow::anyhow!("HID read failed: 0x{:08x}", result as u32))
        } else if length != 32 || data.is_null() {
            Err(anyhow::anyhow!(
                "Expected 32-byte VIA response, received {length}"
            ))
        } else {
            let mut bytes = [0; 32];
            unsafe { ptr::copy_nonoverlapping(data, bytes.as_mut_ptr(), 32) };
            Ok(bytes)
        };
        // 报告指针可能指向 Inbox.buffer；复制完成后才借用整个 Inbox。
        let inbox = unsafe { &mut *context.cast::<Inbox>() };
        inbox.responses.push_back(response);
    }

    fn property(device: Ref, name: &std::ffi::CStr) -> Result<Option<i32>> {
        let key = Owned::new(
            unsafe { CFStringCreateWithCString(ptr::null_mut(), name.as_ptr(), 0x08000100) },
            "CFStringCreateWithCString",
        )?;
        let value = unsafe { IOHIDDeviceGetProperty(device, key.0) };
        if value.is_null() {
            return Ok(None);
        }
        let mut number: i32 = 0;
        ensure!(
            unsafe { CFNumberGetValue(value, 9, (&mut number as *mut i32).cast()) },
            "Invalid numeric HID property"
        );
        Ok(Some(number))
    }

    impl Hid {
        pub(super) fn open() -> Result<Self> {
            let manager = Owned::new(
                unsafe { IOHIDManagerCreate(ptr::null_mut(), 0) },
                "IOHIDManagerCreate",
            )?;
            unsafe { IOHIDManagerSetDeviceMatching(manager.0, ptr::null_mut()) };
            let set = unsafe { IOHIDManagerCopyDevices(manager.0) };
            ensure!(!set.is_null(), "Alice75 VIA USB interface not found");
            let set = Owned(set);
            let count = unsafe { CFSetGetCount(set.0) };
            ensure!(count >= 0, "Invalid HID device count");
            let mut values = vec![ptr::null_mut(); count as usize];
            unsafe { CFSetGetValues(set.0, values.as_mut_ptr()) };
            let mut matches = Vec::new();
            for device in values {
                if property(device, c"VendorID")? == Some(0x36b0)
                    && property(device, c"ProductID")? == Some(0x3016)
                    && property(device, c"PrimaryUsagePage")? == Some(0xff60)
                    && property(device, c"PrimaryUsage")? == Some(0x61)
                {
                    matches.push(device);
                }
            }
            ensure!(
                matches.len() == 1,
                "Expected one Alice75 VIA USB interface, found {}",
                matches.len()
            );
            let device = Owned::new(unsafe { CFRetain(matches[0]) }, "CFRetain")?;
            let result = unsafe { IOHIDDeviceOpen(device.0, 0) };
            ensure!(
                result == 0,
                "Cannot open Alice75 HID: 0x{:08x}",
                result as u32
            );
            let hid = Self {
                device,
                _manager: manager,
                inbox: Box::new(UnsafeCell::new(Inbox {
                    buffer: [0; 64],
                    responses: VecDeque::new(),
                })),
                run_loop: unsafe { CFRunLoopGetCurrent() },
                mode: unsafe { kCFRunLoopDefaultMode },
            };
            let context = hid.inbox.get().cast();
            unsafe {
                IOHIDDeviceRegisterInputReportCallback(
                    hid.device.0,
                    (*hid.inbox.get()).buffer.as_mut_ptr(),
                    64,
                    receive,
                    context,
                );
                IOHIDDeviceScheduleWithRunLoop(hid.device.0, hid.run_loop, hid.mode);
            }
            Ok(hid)
        }
    }

    impl Transport for Hid {
        fn exchange(&mut self, packet: [u8; 32]) -> Result<[u8; 32]> {
            unsafe { CFRunLoopRunInMode(self.mode, 0.001, false) };
            unsafe { (*self.inbox.get()).responses.clear() };
            let result = unsafe { IOHIDDeviceSetReport(self.device.0, 1, 0, packet.as_ptr(), 32) };
            ensure!(result == 0, "HID write failed: 0x{:08x}", result as u32);
            let deadline = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < deadline {
                unsafe { CFRunLoopRunInMode(self.mode, 0.02, true) };
                if let Some(response) = unsafe { (*self.inbox.get()).responses.pop_front() } {
                    return response;
                }
            }
            bail!("Alice75 VIA response timed out; close other VIA controllers and check USB")
        }
    }

    impl Drop for Hid {
        fn drop(&mut self) {
            // 先取消调度再关闭；Drop 只释放资源，灯效恢复由可返回错误的 shutdown 完成。
            unsafe {
                IOHIDDeviceUnscheduleFromRunLoop(self.device.0, self.run_loop, self.mode);
                IOHIDDeviceClose(self.device.0, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
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
