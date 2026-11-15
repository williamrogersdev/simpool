//! Touch, keyboard and button input, sent through the same private
//! SimulatorKit client the Simulator app uses. `simctl` has no input command.

use anyhow::{Context, Result, bail};
use std::ffi::{CString, c_char, c_void};
use std::thread::sleep;
use std::time::Duration;

type Id = *mut c_void;
type Sel = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    width: f64,
    height: f64,
}

type MouseMessage = unsafe extern "C" fn(*const Point, *const Point, u32, usize, Size, u32) -> *mut c_void;
type KeyMessage = unsafe extern "C" fn(u32, u32) -> *mut c_void;
type ButtonMessage = unsafe extern "C" fn(u32, u32, u32) -> *mut c_void;

const TOUCH_TARGET: u32 = 0x32;
const BUTTON_TARGET: u32 = 0x33;
const MOUSE_DOWN: usize = 1;
const MOUSE_UP: usize = 2;
const MOUSE_DRAGGED: usize = 6;
const PRESS: u32 = 1;
const RELEASE: u32 = 2;
const LEFT_SHIFT: u32 = 225;
const GAP: Duration = Duration::from_millis(25);

const CORE_SIMULATOR: &str = "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator";
const FOUNDATION: &str = "/System/Library/Frameworks/Foundation.framework/Foundation";

fn text(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

fn class(name: &str) -> Result<Id> {
    let found = unsafe { objc_getClass(text(name).as_ptr()) };
    if found.is_null() {
        bail!("this Xcode has no `{name}` class, so simpool cannot send input. Use `simpool udid` with Maestro or idb instead.");
    }
    Ok(found)
}

fn selector(name: &str) -> Sel {
    unsafe { sel_registerName(text(name).as_ptr()) }
}

fn load(path: &str) -> Result<*mut c_void> {
    let handle = unsafe { libc::dlopen(text(path).as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        bail!("could not load {path}");
    }
    Ok(handle)
}

fn symbol(library: *mut c_void, name: &str) -> Result<*mut c_void> {
    let found = unsafe { libc::dlsym(library, text(name).as_ptr()) };
    if found.is_null() {
        bail!("this Xcode's SimulatorKit has no `{name}`, so simpool cannot send input. Use `simpool udid` with Maestro or idb instead.");
    }
    Ok(found)
}

unsafe fn send0(receiver: Id, name: &str) -> Id {
    let call: unsafe extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(objc_msgSend as *const c_void) };
    unsafe { call(receiver, selector(name)) }
}

unsafe fn send1(receiver: Id, name: &str, argument: *const c_void) -> Id {
    let call: unsafe extern "C" fn(Id, Sel, *const c_void) -> Id = unsafe { std::mem::transmute(objc_msgSend as *const c_void) };
    unsafe { call(receiver, selector(name), argument) }
}

unsafe fn send2(receiver: Id, name: &str, first: *const c_void, second: *mut Id) -> Id {
    let call: unsafe extern "C" fn(Id, Sel, *const c_void, *mut Id) -> Id = unsafe { std::mem::transmute(objc_msgSend as *const c_void) };
    unsafe { call(receiver, selector(name), first, second) }
}

unsafe fn string(value: &str) -> Result<Id> {
    Ok(unsafe { send1(class("NSString")?, "stringWithUTF8String:", text(value).as_ptr().cast()) })
}

pub struct Hid {
    client: Id,
    mouse: MouseMessage,
    key: KeyMessage,
    button: ButtonMessage,
}

impl Hid {
    pub fn for_device(udid: &str) -> Result<Self> {
        let developer_dir = crate::simctl::run("xcode-select", &["-p"], crate::simctl::QUICK)?;
        Self::open(udid, developer_dir.trim())
    }

    pub fn open(udid: &str, developer_dir: &str) -> Result<Self> {
        load(FOUNDATION)?;
        load(CORE_SIMULATOR)?;
        let kit_path = format!("{}/../SharedFrameworks/SimulatorKit.framework/SimulatorKit", developer_dir.trim_end_matches('/'));
        let kit = load(&kit_path)?;
        unsafe {
            let mut error: Id = std::ptr::null_mut();
            let context = send2(class("SimServiceContext")?, "sharedServiceContextForDeveloperDir:error:", string(developer_dir)?, &mut error);
            if context.is_null() {
                bail!("the simulator service refused a connection for {developer_dir}");
            }
            let call_set: unsafe extern "C" fn(Id, Sel, *mut Id) -> Id = std::mem::transmute(objc_msgSend as *const c_void);
            let set = call_set(context, selector("defaultDeviceSetWithError:"), &mut error);
            if set.is_null() {
                bail!("the simulator service returned no device set");
            }
            let devices = send0(set, "devicesByUDID");
            let uuid = send1(send0(class("NSUUID")?, "alloc"), "initWithUUIDString:", string(udid)?);
            let device = send1(devices, "objectForKey:", uuid);
            if device.is_null() {
                bail!("simulator {udid} is not known to the simulator service");
            }
            let client = send2(send0(class("_TtC12SimulatorKit24SimDeviceLegacyHIDClient")?, "alloc"), "initWithDevice:error:", device, &mut error);
            if client.is_null() {
                bail!("could not open an input session on {udid}. Is it booted?");
            }
            Ok(Self {
                client,
                mouse: std::mem::transmute::<*mut c_void, MouseMessage>(symbol(kit, "IndigoHIDMessageForMouseNSEvent")?),
                key: std::mem::transmute::<*mut c_void, KeyMessage>(symbol(kit, "IndigoHIDMessageForKeyboardArbitrary")?),
                button: std::mem::transmute::<*mut c_void, ButtonMessage>(symbol(kit, "IndigoHIDMessageForButton")?),
            })
        }
    }

    fn deliver(&self, message: *mut c_void) -> Result<()> {
        if message.is_null() {
            bail!("SimulatorKit built no input message");
        }
        unsafe {
            let call: unsafe extern "C" fn(Id, Sel, *mut c_void, bool, Id, Id) = std::mem::transmute(objc_msgSend as *const c_void);
            call(self.client, selector("sendWithMessage:freeWhenDone:completionQueue:completion:"), message, true, std::ptr::null_mut(), std::ptr::null_mut());
        }
        sleep(GAP);
        Ok(())
    }

    fn touch(&self, phase: usize, x: f64, y: f64) -> Result<()> {
        let point = Point { x, y };
        let size = Size { width: 1.0, height: 1.0 };
        self.deliver(unsafe { (self.mouse)(&point, std::ptr::null(), TOUCH_TARGET, phase, size, 0) })
    }

    /// Coordinates are fractions of the screen, 0 to 1 from the top left.
    pub fn tap(&self, x: f64, y: f64, hold: Duration) -> Result<()> {
        self.touch(MOUSE_DOWN, x, y)?;
        sleep(hold);
        self.touch(MOUSE_UP, x, y)
    }

    pub fn swipe(&self, from: (f64, f64), to: (f64, f64), duration: Duration) -> Result<()> {
        let steps = (duration.as_millis() / 16).clamp(6, 120) as u32;
        self.touch(MOUSE_DOWN, from.0, from.1)?;
        for step in 1..=steps {
            let progress = f64::from(step) / f64::from(steps);
            self.touch(MOUSE_DRAGGED, from.0 + (to.0 - from.0) * progress, from.1 + (to.1 - from.1) * progress)?;
            sleep((duration / steps).saturating_sub(GAP));
        }
        self.touch(MOUSE_UP, to.0, to.1)
    }

    pub fn key(&self, usage: u32, shifted: bool) -> Result<()> {
        if shifted {
            self.deliver(unsafe { (self.key)(LEFT_SHIFT, PRESS) })?;
        }
        self.deliver(unsafe { (self.key)(usage, PRESS) })?;
        self.deliver(unsafe { (self.key)(usage, RELEASE) })?;
        if shifted {
            self.deliver(unsafe { (self.key)(LEFT_SHIFT, RELEASE) })?;
        }
        Ok(())
    }

    pub fn type_text(&self, value: &str) -> Result<()> {
        for letter in value.chars() {
            let (usage, shifted) = key_for(letter).with_context(|| format!("`{letter}` is not on the US keyboard simpool types with"))?;
            self.key(usage, shifted)?;
        }
        Ok(())
    }

    pub fn button(&self, code: u32) -> Result<()> {
        self.deliver(unsafe { (self.button)(code, PRESS, BUTTON_TARGET) })?;
        sleep(Duration::from_millis(80));
        self.deliver(unsafe { (self.button)(code, RELEASE, BUTTON_TARGET) })
    }
}

/// USB HID keyboard usage and whether shift is held, for a US layout.
pub fn key_for(letter: char) -> Option<(u32, bool)> {
    const SYMBOLS: &[(char, char, u32)] = &[
        ('-', '_', 45),
        ('=', '+', 46),
        ('[', '{', 47),
        (']', '}', 48),
        ('\\', '|', 49),
        (';', ':', 51),
        ('\'', '"', 52),
        ('`', '~', 53),
        (',', '<', 54),
        ('.', '>', 55),
        ('/', '?', 56),
    ];
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    Some(match letter {
        'a'..='z' => (4 + (letter as u32 - 'a' as u32), false),
        'A'..='Z' => (4 + (letter as u32 - 'A' as u32), true),
        '1'..='9' => (30 + (letter as u32 - '1' as u32), false),
        '0' => (39, false),
        '\n' => (40, false),
        '\t' => (43, false),
        ' ' => (44, false),
        _ => {
            if let Some(index) = SHIFTED_DIGITS.find(letter) {
                return Some((if index == 0 { 39 } else { 29 + index as u32 }, true));
            }
            let (_, _, usage) = SYMBOLS.iter().find(|(plain, shifted, _)| *plain == letter || *shifted == letter)?;
            let shifted = SYMBOLS.iter().any(|(_, upper, _)| *upper == letter);
            (*usage, shifted)
        }
    })
}

pub fn named_key(name: &str) -> Option<u32> {
    Some(match name {
        "return" | "enter" => 40,
        "escape" => 41,
        "delete" | "backspace" => 42,
        "tab" => 43,
        "space" => 44,
        "right" => 79,
        "left" => 80,
        "down" => 81,
        "up" => 82,
        _ => return None,
    })
}

pub fn named_button(name: &str) -> Option<u32> {
    Some(match name {
        "home" => 0x0,
        "lock" => 0x1,
        "side" => 0xbb8,
        "siri" => 0x400002,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_us_keys() {
        assert_eq!(key_for('a'), Some((4, false)));
        assert_eq!(key_for('Z'), Some((29, true)));
        assert_eq!(key_for('1'), Some((30, false)));
        assert_eq!(key_for('0'), Some((39, false)));
        assert_eq!(key_for('!'), Some((30, true)));
        assert_eq!(key_for(')'), Some((39, true)));
        assert_eq!(key_for('@'), Some((31, true)));
        assert_eq!(key_for('.'), Some((55, false)));
        assert_eq!(key_for('?'), Some((56, true)));
        assert_eq!(key_for('_'), Some((45, true)));
        assert_eq!(key_for('é'), None);
    }
}
