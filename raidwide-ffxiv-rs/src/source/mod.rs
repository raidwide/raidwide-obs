use std::ffi;
use std::mem;

use crate::catch_panic;
use crate::output_hook;
use crate::ws;

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
pub const TEST_WS_RESULT: *const ffi::c_char = c"test_ws_result".as_ptr();

/// The source only opts in (data is added to the outputs while one exists)
/// and holds the settings, the hook is the module's (see `output_hook`).
/// OBS needs a non-null pointer for it.
struct RaidwideSource;

unsafe extern "C" fn get_name(_type_data: *mut ffi::c_void) -> *const ffi::c_char {
    c"Raidwide".as_ptr()
}

unsafe extern "C" fn create(
    settings: *mut obs_sys::obs_data_t,
    _source: *mut obs_sys::obs_source_t,
) -> *mut ffi::c_void {
    catch_panic("source creation", std::ptr::null_mut(), || {
        let data = Box::into_raw(Box::new(RaidwideSource)).cast();
        unsafe { update(data, settings) };
        output_hook::add_source();
        data
    })
}

unsafe extern "C" fn destroy(data: *mut ffi::c_void) {
    catch_panic("source destruction", (), || {
        output_hook::remove_source();
        drop(unsafe { Box::from_raw(data.cast::<RaidwideSource>()) });
    })
}

unsafe extern "C" fn update(_data: *mut ffi::c_void, settings: *mut obs_sys::obs_data_t) {
    catch_panic("source update", (), || {
        let ws_url = unsafe { ffi::CStr::from_ptr(obs_sys::obs_data_get_string(settings, WS_URL)) };
        output_hook::set_ws_url(ws_url.to_string_lossy().into_owned());
    })
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
        );
        // Filled by the test button
        let result = obs_sys::obs_properties_add_text(
            properties,
            TEST_WS_RESULT,
            c"".as_ptr(),
            obs_sys::obs_text_type_OBS_TEXT_INFO,
        );
        obs_sys::obs_property_set_visible(result, false);
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
    props: *mut obs_sys::obs_properties_t,
    _property: *mut obs_sys::obs_property,
    _data: *mut ffi::c_void,
) -> bool {
    catch_panic("test connection button", false, || {
        let ws_url = output_hook::ws_url();
        let (text, info_type) = match ws::connect(&ws_url) {
            Ok(mut ws) => {
                let _ = ws.close(None);
                let _ = ws.flush();
                (
                    format!("Connected to {ws_url}"),
                    obs_sys::obs_text_info_type_OBS_TEXT_INFO_NORMAL,
                )
            }
            Err(e) => (
                format!("Could not connect to {ws_url}: {e}"),
                obs_sys::obs_text_info_type_OBS_TEXT_INFO_ERROR,
            ),
        };
        let text = ffi::CString::new(text.replace('\0', "")).unwrap_or_default();

        let result = unsafe { obs_sys::obs_properties_get(props, TEST_WS_RESULT) };
        unsafe {
            obs_sys::obs_property_set_description(result, text.as_ptr());
            obs_sys::obs_property_text_set_info_type(result, info_type);
            obs_sys::obs_property_set_visible(result, true);
        }
        // Refreshes the properties view
        true
    })
}
