mod broadcast;
mod sei;

use std::collections::HashMap;
use std::ffi;
use std::fmt;
use std::ptr;
use std::slice;
use std::sync;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::broadcast::Producer;

static mut OBS_MODULE_POINTER: *mut obs_sys::obs_module_t = std::ptr::null_mut();

const PLUGIN_NAME: &str = env!("CARGO_PKG_NAME");
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

const RAIDWIDE_UUID: [u8; sei::UUID_SIZE] = [
    0x72, 0x61, 0x69, 0x64, 0x77, 0x69, 0x64, 0x65, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14,
];

// LOG HELPERS

fn obs_log_args(log_level: u32, args: fmt::Arguments) {
    let message = format!("[{PLUGIN_NAME}] {args}");
    let message = ffi::CString::new(message.replace('\0', "\\0")).unwrap();

    unsafe {
        obs_sys::blog(log_level.cast_signed(), message.as_ptr());
    }
}

#[macro_export]
macro_rules! obs_log {
    ($level:expr, $($arg:tt)*) => {{
        $crate::obs_log_args($level, format_args!($($arg)*))
    }};
}

// MODULE DECLARATION

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_set_pointer(module: *mut obs_sys::obs_module_t) {
    unsafe {
        OBS_MODULE_POINTER = module;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_ver() -> u32 {
    obs_sys::LIBOBS_API_MAJOR_VER << 24
        | obs_sys::LIBOBS_API_MINOR_VER << 16
        | obs_sys::LIBOBS_API_PATCH_VER
}

// MODULE API

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_load() -> bool {
    obs_log!(
        obs_sys::LOG_INFO,
        "Running rust plugin {} version {}",
        PLUGIN_NAME,
        PLUGIN_VERSION,
    );

    let state: Option<State<Arc<tungstenite::Message>>> = None;
    // TODO: cleanup this memory in obs_module_unload
    let state = Box::into_raw(Box::new(state));

    unsafe {
        obs_sys::obs_frontend_add_event_callback(Some(frontend_event_cb), state.cast());
    }

    true
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    obs_log!(obs_sys::LOG_INFO, "Plugin unloaded");
}

// MODULE IMPLEMENTATION

struct State<T: Clone> {
    sub: broadcast::ConsumerGenerator<T>,
    receivers: HashMap<ffi::CString, sync::mpsc::Receiver<T>>,
    thread_handle: thread::JoinHandle<()>,
}

type Msg = Arc<String>;
type SharedState = Option<State<Msg>>;

fn ws_thread(prod: &mut Producer<Msg>) {
    match tungstenite::connect("ws://127.0.0.1:10501/MiniParse")
    {
        Ok((mut ws, _res)) => {
            obs_log!(obs_sys::LOG_INFO, "Connected to websocket");
            loop {
                prod.send(Arc::new(ws.read().unwrap().to_text().unwrap().to_string()))
            }
        }
        Err(_) => {
            obs_log!(obs_sys::LOG_INFO, "Failed to connect to websocket, falling back to dummy debug producer");
            loop {
                thread::sleep(Duration::from_secs(1));
                prod.send(Arc::new("SEI RAIDWIDE".to_string()))
            }
        }
    }
}

fn init_state(state: &mut SharedState) {
    obs_log!(obs_sys::LOG_INFO, "State initialization");

    let (mut prod, sub) = broadcast::broadcast_channel();
    let thread_handle = thread::spawn(move || {
        ws_thread(&mut prod);
    });
    *state = Some(State {
        sub,
        thread_handle,
        receivers: HashMap::new(),
    });
}

extern "C" fn frontend_event_cb(
    event: obs_sys::obs_frontend_event,
    private_data: *mut ffi::c_void,
) {
    let state = unsafe { private_data.cast::<SharedState>().as_mut_unchecked() };

    if let None = state {
        init_state(state);
    }

    if event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_STREAMING_STARTED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STARTED
    {
        obs_log!(
            obs_sys::LOG_INFO,
            "Recording or streaming started, hooking output"
        );

        unsafe { obs_sys::obs_enum_outputs(Some(enum_output_cb), private_data) };
    }
}

extern "C" fn enum_output_cb(data: *mut ffi::c_void, output: *mut obs_sys::obs_output_t) -> bool {
    let state = unsafe {
        data.cast::<SharedState>()
            .as_mut_unchecked()
            .as_mut()
            .unwrap()
    };

    let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };
    if !state.receivers.contains_key(output_name) {
        obs_log!(
            obs_sys::LOG_INFO,
            "Create new receiver for output {}",
            output_name.to_string_lossy()
        );
        state
            .receivers
            .insert(output_name.to_owned(), state.sub.subscribe());
    }
    let receiver = state.receivers.get_mut(output_name).unwrap();

    unsafe {
        obs_sys::obs_output_remove_packet_callback(
            output,
            Some(packet_cb),
            ptr::from_mut(receiver).cast(),
        );
        obs_sys::obs_output_add_packet_callback(
            output,
            Some(packet_cb),
            ptr::from_mut(receiver).cast(),
        );
    }

    true
}

const LINUX_REF_COUNT_OFFSFET: usize = size_of::<ffi::c_long>();
extern "C" fn packet_cb(
    output: *mut obs_sys::obs_output_t,
    pkt: *mut obs_sys::encoder_packet,
    _pkt_time: *mut obs_sys::encoder_packet_time,
    param: *mut ffi::c_void,
) {
    let pkt = unsafe { pkt.as_mut_unchecked() };
    let receiver = unsafe { param.cast::<sync::mpsc::Receiver<Msg>>().as_mut_unchecked() };

    let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };

    if pkt.type_ != obs_sys::obs_encoder_type_OBS_ENCODER_VIDEO {
        return;
    }

    obs_log!(
        obs_sys::LOG_DEBUG,
        "Received packet of size {} on output {}",
        pkt.size,
        output_name.to_string_lossy()
    );

    let mut buffer = vec![];
    while let Ok(msg) = receiver.try_recv() {
        buffer.extend_from_slice(msg.as_bytes())
    }

    if buffer.is_empty() {
        return;
    }

    let sei_data = sei::SeiUserDataUnregistered::new(&RAIDWIDE_UUID, &buffer);

    pkt.size += sei_data.encoded_size();

    // On linux the encoder_packet has a `ref_count: long` value allocated in
    // front of pkt.
    // TODO: check why this is not the case on windows.
    pkt.data = unsafe {
        obs_sys::brealloc(
            pkt.data.byte_sub(LINUX_REF_COUNT_OFFSFET).cast(),
            pkt.size + LINUX_REF_COUNT_OFFSFET,
        )
        .byte_add(LINUX_REF_COUNT_OFFSFET)
        .cast()
    };
    if pkt.data.is_null() {
        obs_log!(obs_sys::LOG_ERROR, "Cannot allocate space for SEI data");
        pkt.size -= sei_data.encoded_size();
        return;
    }

    let data = unsafe { slice::from_raw_parts_mut(pkt.data, pkt.size) };
    sei_data
        .encode(&mut data[pkt.size - sei_data.encoded_size()..])
        .unwrap();

    obs_log!(
        obs_sys::LOG_INFO,
        "Appended {} bytes of SEI data",
        sei_data.encoded_size()
    );
}
