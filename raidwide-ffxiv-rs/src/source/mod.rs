use std::ffi;
use std::mem;

use crate::obs_debug;
use crate::source::output_hook::RaidwideSourceState;

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

#[derive(Default)]
struct RaidwideSource {
    source: *mut obs_sys::obs_source_t,
    ws_url: String,

    callback_registered: bool,
    state: output_hook::RaidwideSourceState,
}

unsafe extern "C" fn get_name(_type_data: *mut ffi::c_void) -> *const ffi::c_char {
    c"Raidwide".as_ptr()
}

unsafe extern "C" fn create(
    settings: *mut obs_sys::obs_data_t,
    source: *mut obs_sys::obs_source_t,
) -> *mut ffi::c_void {
    let mut raidwide = Box::new(RaidwideSource::default());
    raidwide.source = source;

    let data = Box::into_raw(raidwide).cast();
    unsafe { update(data, settings) };
    data
}

unsafe extern "C" fn destroy(data: *mut ffi::c_void) {
    let mut source: Box<RaidwideSource> = unsafe { Box::from_raw(data.cast()) };

    if source.callback_registered {
        unsafe {
            obs_sys::obs_frontend_remove_event_callback(Some(output_hook::frontend_event_cb), data);
        }
    }

    if matches!(source.state, RaidwideSourceState::Running { .. }) {
        source.state.stop(data);
    }

    drop(source)
}

unsafe extern "C" fn update(data: *mut ffi::c_void, settings: *mut obs_sys::obs_data_t) {
    let source = unsafe { data.cast::<RaidwideSource>().as_mut_unchecked() };
    source.ws_url = unsafe { ffi::CStr::from_ptr(obs_sys::obs_data_get_string(settings, WS_URL)) }
        .to_str()
        .unwrap()
        .to_string();

    if !source.callback_registered {
        obs_debug!("Registering frontend callback event");
        unsafe {
            obs_sys::obs_frontend_add_event_callback(Some(output_hook::frontend_event_cb), data);
        }
        source.callback_registered = true;
    }
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
    let source = unsafe { data.cast::<RaidwideSource>().as_mut_unchecked() };
    let res = match tungstenite::client::connect(&source.ws_url) {
        // TOOD: close connection
        Ok(_) => c"OK".as_ptr(),
        Err(_) => c"KO".as_ptr(),
    };
    unsafe { obs_sys::obs_property_set_description(property, res) };
    true
}
