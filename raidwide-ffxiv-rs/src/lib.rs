mod broadcast;
mod calldata;
mod log;
mod sei;

use std::collections::HashMap;
use std::ffi;
use std::ptr;
use std::slice;
use std::sync;
use std::sync::Arc;
use std::thread;
use std::time;

static mut OBS_MODULE_POINTER: *mut obs_sys::obs_module_t = std::ptr::null_mut();

pub const PLUGIN_NAME: &str = env!("CARGO_PKG_NAME");
pub const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const RAIDWIDE_UUID: [u8; sei::UUID_SIZE] = [
    0x72, 0x61, 0x69, 0x64, 0x77, 0x69, 0x64, 0x65, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14,
];

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
    obs_info!("Running rust plugin {PLUGIN_NAME} version {PLUGIN_VERSION}");

    let state: SharedState = None;
    // TODO: cleanup this memory in obs_module_unload
    let state = Box::into_raw(Box::new(state));

    unsafe {
        obs_sys::obs_frontend_add_event_callback(Some(frontend_event_cb), state.cast());
    }

    true
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    obs_info!("Plugin unloaded");
}

// MODULE IMPLEMENTATION

struct State<T: Clone> {
    sub: broadcast::ConsumerGenerator<T>,
    receivers: HashMap<ffi::CString, sync::mpsc::Receiver<T>>,
    thread_handle: thread::JoinHandle<()>,
    start_event_count: usize,
}

type Msg = Arc<Vec<u8>>;
type SharedState = Option<State<Msg>>;

fn ws_thread(prod: &mut broadcast::Producer<Msg>) {
    match tungstenite::connect("ws://127.0.0.1:10501/MiniParse") {
        Ok((mut ws, _res)) => {
            obs_info!("Connected to websocket");
            loop {
                let msg = match ws.read() {
                    Ok(msg) => msg,
                    Err(e) => {
                        obs_warning!("Error reading from websocket: {e}");
                        break;
                    }
                };
                if let Err(_) = prod.send(Arc::new(msg.into_data().into())) {
                    break;
                }
            }
        }
        Err(e) => {
            obs_warning!(
                "Failed to connect to websocket, falling back to dummy debug producer: {e}"
            );
            loop {
                thread::sleep(time::Duration::from_secs(1));
                if let Err(_) = prod.send(Arc::new(b"SEI RAIDWIDE".to_vec())) {
                    break;
                }
            }
        }
    }
    obs_debug!("Websocket thread finished");
}

fn init_state(state: &mut SharedState) {
    obs_debug!("State initialization");

    let (mut prod, sub) = broadcast::broadcast_channel();
    let thread_handle = thread::spawn(move || {
        ws_thread(&mut prod);
    });
    *state = Some(State {
        sub,
        thread_handle,
        receivers: HashMap::new(),
        start_event_count: 0,
    });
}

fn drop_state(state: &mut SharedState) {
    let mut state = state.take().unwrap();
    state.sub.close();
    state.thread_handle.join().unwrap();

    for (output_name, receiver) in state.receivers.iter_mut() {
        let output = unsafe { obs_sys::obs_get_output_by_name(output_name.as_ptr()) };
        if output.is_null() {
            obs_warning!(
                "Cannot remove callback for {} because no output with that name were found",
                output_name.to_string_lossy()
            );
            continue;
        }

        unsafe {
            obs_sys::obs_output_remove_packet_callback(
                output,
                Some(packet_cb),
                ptr::from_mut(receiver).cast(),
            );
            obs_sys::obs_output_release(output);
        }
    }

    obs_debug!("State destroyed");
}

extern "C" fn frontend_event_cb(
    event: obs_sys::obs_frontend_event,
    private_data: *mut ffi::c_void,
) {
    let state = unsafe { private_data.cast::<SharedState>().as_mut_unchecked() };

    if event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_STREAMING_STARTED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STARTED
    {
        if let None = state {
            init_state(state);
        }
        state.as_mut().unwrap().start_event_count += 1;

        obs_info!("Hooking outputs");
        unsafe { obs_sys::obs_enum_outputs(Some(enum_output_cb), private_data) };
    }

    if event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_STREAMING_STOPPED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STOPPED
    {
        state.as_mut().unwrap().start_event_count -= 1;
        if state.as_mut().unwrap().start_event_count == 0 {
            drop_state(state);
        }
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

    let signal_handler = unsafe { obs_sys::obs_output_get_signal_handler(output) };

    obs_info!(
        "Adding signal handlers for output {}",
        output_name.to_string_lossy()
    );
    unsafe {
        obs_sys::signal_handler_connect(
            signal_handler,
            c"start".as_ptr(),
            Some(output_start_cb),
            ptr::from_mut(state).cast(),
        );
        obs_sys::signal_handler_connect(
            signal_handler,
            c"stop".as_ptr(),
            Some(output_stop_cb),
            ptr::from_mut(state).cast(),
        );
    };

    true
}

extern "C" fn output_start_cb(data: *mut ffi::c_void, callback_data: *mut obs_sys::calldata_t) {
    // TODO: make this thread safe?
    let state = unsafe {
        data.cast::<SharedState>()
            .as_mut_unchecked()
            .as_mut()
            .unwrap()
    };

    let output =
        unsafe { calldata::calldata_ptr(callback_data, c"output").cast::<obs_sys::obs_output_t>() };
    let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };

    if state
        .receivers
        .insert(output_name.to_owned(), state.sub.subscribe())
        .is_some()
    {
        obs_warning!(
            "Registering packet callback for output {}, but state already existed with the same name",
            output_name.to_string_lossy()
        );
    }

    let receiver = state.receivers.get_mut(output_name).unwrap();
    obs_debug!(
        "Registering packet callback for output {}",
        output_name.to_string_lossy()
    );
    unsafe {
        obs_sys::obs_output_remove_packet_callback(
            output,
            Some(packet_cb),
            ptr::from_mut(receiver).cast(),
        )
    };
}

extern "C" fn output_stop_cb(data: *mut ffi::c_void, callback_data: *mut obs_sys::calldata_t) {
    // TODO: make this thread safe?
    let state = unsafe {
        data.cast::<SharedState>()
            .as_mut_unchecked()
            .as_mut()
            .unwrap()
    };

    let output =
        unsafe { calldata::calldata_ptr(callback_data, c"output").cast::<obs_sys::obs_output_t>() };
    let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };

    let mut receiver = match state.receivers.remove(output_name) {
        None => {
            obs_warning!(
                "Unregistering packet callback for output {}, but state does not contain any output with this name",
                output_name.to_string_lossy()
            );
            return;
        }
        Some(receiver) => receiver,
    };

    obs_debug!(
        "Unregistering packet callback for output {}",
        output_name.to_string_lossy()
    );
    unsafe {
        obs_sys::obs_output_remove_packet_callback(
            output,
            Some(packet_cb),
            ptr::from_mut(&mut receiver).cast(),
        )
    };
}

// On linux the encoder_packet has a `ref_count: long` value allocated in
// front of pkt.
// TODO: check why this is not the case on windows.
#[cfg(not(target_os = "windows"))]
const ENCODER_PACKET_ALLOC_OFFSET: usize = size_of::<ffi::c_long>();
#[cfg(target_os = "windows")]
const ENCODER_PACKET_ALLOC_OFFSET: usize = 0;

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

    obs_debug!(
        "Received packet of size {} on output {}",
        pkt.size,
        output_name.to_string_lossy()
    );

    let mut buffer = vec![];
    while let Ok(msg) = receiver.try_recv() {
        buffer.extend_from_slice(&msg)
    }

    if buffer.is_empty() {
        return;
    }

    let sei_data = sei::SeiUserDataUnregistered::new(&RAIDWIDE_UUID, &buffer);

    pkt.size += sei_data.encoded_size();

    pkt.data = unsafe {
        obs_sys::brealloc(
            pkt.data.byte_sub(ENCODER_PACKET_ALLOC_OFFSET).cast(),
            pkt.size + ENCODER_PACKET_ALLOC_OFFSET,
        )
        .byte_add(ENCODER_PACKET_ALLOC_OFFSET)
        .cast()
    };
    if pkt.data.is_null() {
        obs_error!("Cannot allocate space for SEI data");
        // TODO: signal error
        pkt.size -= sei_data.encoded_size();
        return;
    }

    let data = unsafe { slice::from_raw_parts_mut(pkt.data, pkt.size) };
    sei_data
        .encode(&mut data[pkt.size - sei_data.encoded_size()..])
        .unwrap();

    obs_info!("Appended {} bytes of SEI data", sei_data.encoded_size());
}
