use std::ffi;
use std::mem;
use std::sync;

use crate::ws;

mod output_hook;

pub static mut RAIDWIDE_SOURCE: obs_sys::obs_source_info = obs_sys::obs_source_info {
    id: c"raidwide".as_ptr(),
    type_: obs_sys::obs_source_type_OBS_SOURCE_TYPE_INPUT,
    output_flags: obs_sys::OBS_SOURCE_DO_NOT_DUPLICATE,
    get_name: Some(get_name),
    create: Some(create),
    update: Some(update),
    destroy: Some(destroy),
    get_properties: Some(get_properties),
    get_defaults: Some(get_defaults),
    icon_type: obs_sys::obs_icon_type_OBS_ICON_TYPE_CUSTOM,
    ..unsafe { mem::zeroed() }
};

pub const WS_URL: *const ffi::c_char = c"ws_url".as_ptr();
pub const TEST_WS_BUTTON: *const ffi::c_char = c"test_ws_button".as_ptr();

/// Shared by the UI thread (settings, frontend events) and the outputs'
/// threads (signals), hence the lock
struct RaidwideSource {
    state: sync::Mutex<State>,
}

#[derive(Default)]
struct State {
    ws_url: String,
    outputs: Vec<output_hook::TrackedOutput>,
}

impl RaidwideSource {
    /// # Safety
    /// `data` must be the pointer returned by `create`, not destroyed yet
    unsafe fn from_data<'a>(data: *mut ffi::c_void) -> &'a Self {
        unsafe { data.cast::<Self>().as_ref_unchecked() }
    }

    fn state(&self) -> sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(sync::PoisonError::into_inner)
    }
}

unsafe extern "C" fn get_name(_type_data: *mut ffi::c_void) -> *const ffi::c_char {
    c"Raidwide".as_ptr()
}

unsafe extern "C" fn create(
    settings: *mut obs_sys::obs_data_t,
    _source: *mut obs_sys::obs_source_t,
) -> *mut ffi::c_void {
    let raidwide = Box::new(RaidwideSource {
        state: sync::Mutex::new(State::default()),
    });

    let data = Box::into_raw(raidwide).cast();
    unsafe {
        update(data, settings);
        obs_sys::obs_frontend_add_event_callback(Some(output_hook::frontend_event_cb), data);
    }
    // Hooks the outputs already active, e.g. when the source is added during a
    // recording, and follows the existing ones from their next start
    output_hook::track_outputs(data);
    data
}

unsafe extern "C" fn destroy(data: *mut ffi::c_void) {
    unsafe {
        obs_sys::obs_frontend_remove_event_callback(Some(output_hook::frontend_event_cb), data);
    }
    output_hook::untrack_outputs(data);
    drop(unsafe { Box::from_raw(data.cast::<RaidwideSource>()) });
}

unsafe extern "C" fn update(data: *mut ffi::c_void, settings: *mut obs_sys::obs_data_t) {
    let source = unsafe { RaidwideSource::from_data(data) };
    let ws_url = unsafe { ffi::CStr::from_ptr(obs_sys::obs_data_get_string(settings, WS_URL)) };
    source.state().ws_url = ws_url.to_string_lossy().into_owned();
}

unsafe extern "C" fn get_properties(data: *mut ffi::c_void) -> *mut obs_sys::obs_properties_t {
    let properties = unsafe { obs_sys::obs_properties_create() };
    unsafe {
        obs_sys::obs_properties_add_text(
            properties,
            WS_URL,
            c"WebSocket URL".as_ptr(),
            obs_sys::obs_text_type_OBS_TEXT_DEFAULT,
        );
        obs_sys::obs_properties_add_button2(
            properties,
            TEST_WS_BUTTON,
            c"Test Connection".as_ptr(),
            Some(test_ws_button_callback),
            data,
        )
    };
    properties
}

unsafe extern "C" fn get_defaults(settings: *mut obs_sys::obs_data_t) {
    unsafe {
        obs_sys::obs_data_set_default_string(
            settings,
            WS_URL,
            c"ws://127.0.0.1:10500/MiniParse".as_ptr(),
        );
    }
}

unsafe extern "C" fn test_ws_button_callback(
    _props: *mut obs_sys::obs_properties_t,
    property: *mut obs_sys::obs_property,
    data: *mut ffi::c_void,
) -> bool {
    let source = unsafe { RaidwideSource::from_data(data) };
    let ws_url = source.state().ws_url.clone();
    let res = match ws::connect(&ws_url) {
        Ok(mut ws) => {
            let _ = ws.close(None);
            let _ = ws.flush();
            c"OK".as_ptr()
        }
        Err(_) => c"KO".as_ptr(),
    };
    unsafe { obs_sys::obs_property_set_description(property, res) };
    true
}
