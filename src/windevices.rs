//! The Windows output devices, for the Equalizer APO device buttons.
//!
//! Just enough of the MMDevice API to turn Equalizer APO's endpoint GUIDs into
//! names the panel can show.

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eRender};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
};

/// An audio endpoint: its id ends with the endpoint GUID Equalizer APO uses.
pub struct Endpoint {
    pub id: String,
    pub name: String,
}

/// Keeps COM initialised for as long as it is held.
struct Com(bool);

impl Com {
    fn init() -> Com {
        // SAFETY: plain COM initialisation; balanced in Drop when it succeeded.
        Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Every active output device (friendly name and endpoint id).
pub fn outputs() -> Vec<Endpoint> {
    let _com = Com::init();
    // SAFETY: COM calls on the enumerator and its collection; the strings it
    // hands out are copied and freed here.
    unsafe {
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return Vec::new();
        };
        let Ok(list) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        (0..list.GetCount().unwrap_or(0))
            .filter_map(|i| {
                let dev = list.Item(i).ok()?;
                let id = take_pwstr(dev.GetId().ok()?);
                let store = dev.OpenPropertyStore(STGM_READ).ok()?;
                let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
                let name = take_pwstr(PropVariantToStringAlloc(&value).ok()?);
                Some(Endpoint { id, name })
            })
            .collect()
    }
}

/// Copy a COM-allocated wide string and free it.
unsafe fn take_pwstr(p: windows::core::PWSTR) -> String {
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}
