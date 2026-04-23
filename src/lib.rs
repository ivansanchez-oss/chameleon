//! Public API for the chamaleon keyboard-layout switcher. Windows-only for now.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::{
    Win32::Devices::DeviceAndDriverInstallation::*,
    Win32::Foundation::{LPARAM, WPARAM},
    Win32::UI::Input::KeyboardAndMouse::{
        ACTIVATE_KEYBOARD_LAYOUT_FLAGS, ActivateKeyboardLayout, KLF_ACTIVATE, LoadKeyboardLayoutW,
    },
    Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, PostMessageW, WM_INPUTLANGCHANGEREQUEST},
    core::{GUID, PCWSTR},
};

// https://learn.microsoft.com/en-us/windows-hardware/drivers/install/guid-devinterface-keyboard
const GUID_DEVINTERFACE_KEYBOARD: GUID = GUID::from_u128(0x884b96c3_56ef_11d1_bc8c_00a0c91405dd);

/// A Windows keyboard layout identified by its KLID string.
///
/// See <https://learn.microsoft.com/en-us/windows/win32/intl/language-identifier-constants-and-strings>.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(from = "String")]
#[non_exhaustive]
pub enum KeyboardLayout {
    EnglishUS,
    EnglishUK,
    SpanishLatinAmerica,
    SpanishSpain,
    French,
    German,
    PortugueseBrazil,
    Italian,
    /// Raw KLID string, e.g. `"00000409"`.
    Custom(String),
}

impl KeyboardLayout {
    /// KLID string consumed by `LoadKeyboardLayoutW`.
    pub fn klid(&self) -> &str {
        match self {
            Self::EnglishUS => "00000409",
            Self::EnglishUK => "00000809",
            Self::SpanishLatinAmerica => "0000080A",
            Self::SpanishSpain => "0000040A",
            Self::French => "0000040C",
            Self::German => "00000407",
            Self::PortugueseBrazil => "00000416",
            Self::Italian => "00000410",
            Self::Custom(s) => s.as_str(),
        }
    }
}

impl From<String> for KeyboardLayout {
    fn from(s: String) -> Self {
        match s.as_str() {
            "EnglishUS" => Self::EnglishUS,
            "EnglishUK" => Self::EnglishUK,
            "SpanishLatinAmerica" => Self::SpanishLatinAmerica,
            "SpanishSpain" => Self::SpanishSpain,
            "French" => Self::French,
            "German" => Self::German,
            "PortugueseBrazil" => Self::PortugueseBrazil,
            "Italian" => Self::Italian,
            _ => Self::Custom(s),
        }
    }
}

/// Errors returned by the chamaleon API.
#[derive(Debug)]
pub enum Error {
    /// `KeyboardFilterBuilder::build` was called without setting `default_layout`.
    MissingDefaultLayout,
    /// `CM_Register_Notification` returned a non-success `CONFIGRET`.
    RegisterFailed(CONFIGRET),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingDefaultLayout => {
                write!(f, "default_layout must be set before calling build")
            }
            Self::RegisterFailed(cr) => write!(f, "CM_Register_Notification failed: {cr:?}"),
        }
    }
}

impl std::error::Error for Error {}

/// Per-keyboard configuration entry.
#[derive(Debug, Clone)]
struct KeyboardEntry {
    alias: Option<String>,
    layout: KeyboardLayout,
}

/// Watches PnP keyboard connect/disconnect events and switches the active layout.
pub struct KeyboardFilter {
    default_layout: KeyboardLayout,
    on_connect: HashMap<String, KeyboardEntry>,
}

impl KeyboardFilter {
    /// Start a builder. `default_layout` must be set before `build`.
    pub fn builder() -> KeyboardFilterBuilder {
        KeyboardFilterBuilder {
            default_layout: None,
            on_connect: HashMap::new(),
        }
    }

    pub fn default_layout(&self) -> &KeyboardLayout {
        &self.default_layout
    }

    /// Subscribe to keyboard PnP events. The returned [`Watcher`] keeps the
    /// subscription alive; drop it to stop.
    pub fn watch(&self) -> Result<Watcher, Error> {
        let present = present_keyboard_ids();
        match present.iter().find_map(|id| self.on_connect.get(id)) {
            Some(entry) => {
                tracing::info!(
                    alias = entry.alias.as_deref().unwrap_or("-"),
                    klid = entry.layout.klid(),
                    "configured keyboard present at startup"
                );
                switch_layout(entry.layout.klid());
            }
            None => {
                tracing::info!("no configured keyboard present at startup, applying default");
                switch_layout(self.default_layout.klid());
            }
        }

        let state = Box::new(WatchState {
            default_layout: self.default_layout.clone(),
            on_connect: self.on_connect.clone(),
        });
        let context = &*state as *const WatchState as *const c_void;

        unsafe {
            let mut filter = CM_NOTIFY_FILTER::default();
            filter.cbSize = std::mem::size_of::<CM_NOTIFY_FILTER>() as u32;
            filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
            filter.u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_KEYBOARD;

            let mut handle = HCMNOTIFICATION::default();
            let cr = CM_Register_Notification(
                &filter,
                Some(context),
                Some(notify_callback),
                &mut handle,
            );

            if cr != CR_SUCCESS {
                return Err(Error::RegisterFailed(cr));
            }

            Ok(Watcher {
                handle,
                _state: state,
            })
        }
    }
}

pub struct KeyboardFilterBuilder {
    default_layout: Option<KeyboardLayout>,
    on_connect: HashMap<String, KeyboardEntry>,
}

impl KeyboardFilterBuilder {
    /// Layout applied when no configured keyboard is present (startup with no
    /// keyboard, or any disconnect). Required.
    pub fn default_layout(mut self, layout: KeyboardLayout) -> Self {
        self.default_layout = Some(layout);
        self
    }

    /// Register a layout to apply when the keyboard with the given identifier
    /// connects. The identifier is the `VID_xxxx&PID_xxxx` substring of the
    /// device's symbolic link, e.g. `"VID_258A&PID_002A"` (case-insensitive).
    /// `alias` is optional and only used in logs. Call multiple times to
    /// configure several keyboards.
    pub fn on_connect(
        mut self,
        id: impl Into<String>,
        alias: Option<String>,
        layout: KeyboardLayout,
    ) -> Self {
        self.on_connect.insert(
            id.into().to_ascii_uppercase(),
            KeyboardEntry { alias, layout },
        );
        self
    }

    pub fn build(self) -> Result<KeyboardFilter, Error> {
        Ok(KeyboardFilter {
            default_layout: self.default_layout.ok_or(Error::MissingDefaultLayout)?,
            on_connect: self.on_connect,
        })
    }
}

/// Active subscription guard. Unregisters on drop.
pub struct Watcher {
    handle: HCMNOTIFICATION,
    _state: Box<WatchState>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            let _ = CM_Unregister_Notification(self.handle);
        }
    }
}

struct WatchState {
    default_layout: KeyboardLayout,
    on_connect: HashMap<String, KeyboardEntry>,
}

unsafe extern "system" fn notify_callback(
    _notify: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    event_data: *const CM_NOTIFY_EVENT_DATA,
    _event_data_size: u32,
) -> u32 {
    let state = unsafe { &*(context as *const WatchState) };
    let device = unsafe { device_symbolic_link(event_data) };

    match action {
        CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL => {
            let key = device_key(&device);
            match state.on_connect.get(&key) {
                Some(entry) => {
                    tracing::info!(
                        id = %key,
                        alias = entry.alias.as_deref().unwrap_or("-"),
                        "keyboard connected"
                    );
                    switch_layout(entry.layout.klid());
                }
                None => {
                    tracing::info!(id = %key, "keyboard connected (no configuration)");
                }
            }
        }
        CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL => {
            tracing::info!(device = %device, "keyboard disconnected");
            switch_layout(state.default_layout.klid());
        }
        _ => {}
    }
    0 // ERROR_SUCCESS
}

// VID/PID extracted from a HID symbolic link, uppercased for case-insensitive
// matching against the user-supplied identifiers in `on_connect`.
fn device_key(symlink: &str) -> String {
    let upper = symlink.to_ascii_uppercase();
    if let Some(idx) = upper.find("VID_") {
        let tail = &upper[idx..];
        if tail.len() >= 17 {
            return tail[..17].to_string();
        }
    }
    upper
}

// SymbolicLink is a variable-length wide string laid out past the struct's
// fixed-size [u16; 1] placeholder; walk until the null terminator.
unsafe fn device_symbolic_link(event_data: *const CM_NOTIFY_EVENT_DATA) -> String {
    if event_data.is_null() {
        return String::new();
    }
    unsafe {
        let start = std::ptr::addr_of!((*event_data).u.DeviceInterface.SymbolicLink) as *const u16;
        let mut len = 0usize;
        while len < 4096 && *start.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(start, len))
    }
}

// Enumerate all currently-present keyboard device interfaces and return their
// `VID_xxxx&PID_xxxx` identifiers. The list API returns a multi-sz wide buffer:
// null-terminated strings back-to-back, ending with an extra empty string.
fn present_keyboard_ids() -> Vec<String> {
    unsafe {
        let mut len: u32 = 0;
        let cr = CM_Get_Device_Interface_List_SizeW(
            &mut len,
            &GUID_DEVINTERFACE_KEYBOARD,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        );
        if cr != CR_SUCCESS || len <= 1 {
            return Vec::new();
        }

        let mut buffer = vec![0u16; len as usize];
        let cr = CM_Get_Device_Interface_ListW(
            &GUID_DEVINTERFACE_KEYBOARD,
            PCWSTR::null(),
            &mut buffer,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        );
        if cr != CR_SUCCESS {
            return Vec::new();
        }

        let mut ids = Vec::new();
        let mut start = 0usize;
        for i in 0..buffer.len() {
            if buffer[i] == 0 {
                if i == start {
                    break;
                }
                let symlink = String::from_utf16_lossy(&buffer[start..i]);
                ids.push(device_key(&symlink));
                start = i + 1;
            }
        }
        ids
    }
}

fn switch_layout(klid: &str) {
    let wide: Vec<u16> = klid.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        match LoadKeyboardLayoutW(PCWSTR(wide.as_ptr()), KLF_ACTIVATE) {
            Ok(hkl) => {
                let _ = ActivateKeyboardLayout(hkl, ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0));
                let _ = PostMessageW(
                    Some(HWND_BROADCAST),
                    WM_INPUTLANGCHANGEREQUEST,
                    WPARAM(0),
                    LPARAM(hkl.0 as isize),
                );
                tracing::info!(klid, "keyboard layout switched");
            }
            Err(e) => {
                tracing::error!(klid, error = %e, "failed to load keyboard layout");
            }
        }
    }
}
