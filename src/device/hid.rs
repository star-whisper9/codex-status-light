//! 两款键盘共用的 VIA Raw HID 传输；不包含灯效策略。
use anyhow::Result;

pub(super) trait Transport {
    fn exchange(&mut self, packet: [u8; 32]) -> Result<[u8; 32]>;
}

pub(super) fn open(
    vendor_id: u16,
    product_id: u16,
    name: &'static str,
) -> Result<Box<dyn Transport>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::Hid::open(vendor_id, product_id, name)?))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (vendor_id, product_id);
        anyhow::bail!("{name} output is supported only on macOS")
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use anyhow::{bail, ensure};
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
        name: &'static str,
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
        pub(super) fn open(vendor_id: u16, product_id: u16, name: &'static str) -> Result<Self> {
            let manager = Owned::new(
                unsafe { IOHIDManagerCreate(ptr::null_mut(), 0) },
                "IOHIDManagerCreate",
            )?;
            unsafe { IOHIDManagerSetDeviceMatching(manager.0, ptr::null_mut()) };
            let set = unsafe { IOHIDManagerCopyDevices(manager.0) };
            ensure!(!set.is_null(), "{name} VIA USB interface not found");
            let set = Owned(set);
            let count = unsafe { CFSetGetCount(set.0) };
            ensure!(count >= 0, "Invalid HID device count");
            let mut values = vec![ptr::null_mut(); count as usize];
            unsafe { CFSetGetValues(set.0, values.as_mut_ptr()) };
            let mut matches = Vec::new();
            for device in values {
                if property(device, c"VendorID")? == Some(i32::from(vendor_id))
                    && property(device, c"ProductID")? == Some(i32::from(product_id))
                    && property(device, c"PrimaryUsagePage")? == Some(0xff60)
                    && property(device, c"PrimaryUsage")? == Some(0x61)
                {
                    matches.push(device);
                }
            }
            ensure!(
                matches.len() == 1,
                "Expected one {name} VIA USB interface, found {}",
                matches.len()
            );
            let device = Owned::new(unsafe { CFRetain(matches[0]) }, "CFRetain")?;
            let result = unsafe { IOHIDDeviceOpen(device.0, 0) };
            ensure!(
                result == 0,
                "Cannot open {name} HID: 0x{:08x}",
                result as u32
            );
            let hid = Self {
                name,
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
            bail!(
                "{} VIA response timed out; close other VIA controllers and check USB",
                self.name
            )
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
