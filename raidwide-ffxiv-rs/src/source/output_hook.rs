use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi;
use std::mem;
use std::ptr;
use std::slice;
use std::sync;
use std::sync::Arc;
use std::thread;
use std::time;

use crate::broadcast;
use crate::calldata;
use crate::sei;
use crate::source::RaidwideSource;
use crate::{obs_debug, obs_info, obs_warning};

pub type Msg = Arc<Vec<u8>>;

#[derive(Default)]
pub enum RaidwideSourceState {
    #[default]
    Idle,
    Running {
        thread_handle: thread::JoinHandle<()>,
        start_event_count: usize,
        consumer: broadcast::ConsumerGenerator<Msg>,
        // TODO: thread-safe?
        receivers: HashMap<ffi::CString, sync::mpsc::Receiver<Msg>>,
        signal_subscribed: HashSet<ffi::CString>,
    },
}

impl RaidwideSourceState {
    pub fn start(&mut self, data: *mut ffi::c_void) {
        obs_debug!("Starting raidwide");
        assert!(matches!(self, Self::Idle));

        let source = unsafe { data.cast::<RaidwideSource>().as_mut_unchecked() };

        let (mut producer, consumer) = broadcast::broadcast_channel();
        let thread_handle = thread::spawn({
            let url = source.ws_url.clone();
            move || {
                ws_thread(&mut producer, &url);
            }
        });

        *self = Self::Running {
            consumer,
            receivers: HashMap::new(),
            signal_subscribed: HashSet::new(),
            thread_handle,
            start_event_count: 1,
        };

        obs_debug!("Raidwide started");
    }

    pub fn stop(&mut self, _data: *mut ffi::c_void) {
        obs_debug!("Stopping raidwide");
        assert!(matches!(self, Self::Running { .. }));

        let state = mem::replace(self, Self::Idle);

        match state {
            Self::Idle => panic!("State management error"),
            Self::Running {
                mut consumer,
                mut receivers,
                thread_handle,
                ..
            } => {
                consumer.close();
                thread_handle.join().unwrap();

                for (output_name, receiver) in receivers.iter_mut() {
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
            }
        }

        obs_debug!("Raidwide stopped");
    }

    pub fn try_start(&mut self, data: *mut ffi::c_void) {
        // TODO: run enum output callbacks in case new output have been created
        match self {
            Self::Idle => {
                self.start(data);
                obs_info!("Hooking outputs");
                unsafe { obs_sys::obs_enum_outputs(Some(enum_output_cb), data) };
            }
            Self::Running {
                start_event_count, ..
            } => *start_event_count += 1,
        }
    }

    pub fn try_stop(&mut self, data: *mut ffi::c_void) {
        match self {
            Self::Running {
                start_event_count, ..
            } => {
                *start_event_count -= 1;
                if *start_event_count == 0 {
                    self.stop(data);
                }
            }
            Self::Idle => obs_warning!("stop was called more than start"),
        }
    }

    pub fn register_output_hook(&mut self, output: *mut obs_sys::obs_output_t) {
        let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };
        obs_debug!(
            "Registering packet callback for output {}",
            output_name.to_string_lossy()
        );

        let (consumer, receivers) = match self {
            RaidwideSourceState::Running {
                consumer,
                receivers,
                ..
            } => (consumer, receivers),
            _ => {
                obs_warning!("Raidwide is not running");
                return;
            }
        };

        if receivers
            .insert(output_name.to_owned(), consumer.subscribe())
            .is_some()
        {
            obs_warning!(
                "Registering packet callback for output {}, but state already existed with the same name",
                output_name.to_string_lossy()
            );
        }

        let receiver = receivers.get_mut(output_name).unwrap();
        unsafe {
            obs_sys::obs_output_add_packet_callback(
                output,
                Some(packet_cb),
                ptr::from_mut(receiver).cast(),
            )
        };
    }

    pub fn unregister_output_hook(&mut self, output: *mut obs_sys::obs_output_t) {
        let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };

        obs_debug!(
            "Unregistering packet callback for output {}",
            output_name.to_string_lossy()
        );

        let receivers = match self {
            RaidwideSourceState::Running { receivers, .. } => receivers,
            _ => {
                obs_warning!("Raidwide is not running");
                return;
            }
        };

        if !receivers.contains_key(output_name) {
            obs_warning!(
                "Packet callbacks were already unregistered for output {}, ignoring",
                output_name.to_string_lossy()
            );
            return;
        }

        let receiver = receivers.get_mut(output_name).unwrap();
        unsafe {
            obs_sys::obs_output_remove_packet_callback(
                output,
                Some(packet_cb),
                ptr::from_mut(receiver).cast(),
            )
        };
        receivers.remove(output_name).unwrap();
    }

    pub fn unregister_output_signal_hooks(
        &mut self,
        output: *mut obs_sys::obs_output_t,
        data: *mut ffi::c_void,
    ) {
        let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };
        obs_debug!(
            "Unregistering signal callbacks for output {}",
            output_name.to_string_lossy()
        );

        self.unregister_output_hook(output);

        let signal_subscribed = match self {
            RaidwideSourceState::Running {
                signal_subscribed, ..
            } => signal_subscribed,
            _ => {
                obs_warning!("Raidwide is not running");
                return;
            }
        };

        let signal_handler = unsafe { obs_sys::obs_output_get_signal_handler(output) };
        unsafe {
            obs_sys::signal_handler_disconnect(
                signal_handler,
                c"activate".as_ptr(),
                Some(output_activate_cb),
                data,
            );
            obs_sys::signal_handler_disconnect(
                signal_handler,
                c"deactivate".as_ptr(),
                Some(output_deactivate_cb),
                data,
            );
            obs_sys::signal_handler_disconnect(
                signal_handler,
                c"stop".as_ptr(),
                Some(output_stop_cb),
                data,
            );
        };
        signal_subscribed.remove(output_name);
    }
}

fn ws_thread(prod: &mut broadcast::Producer<Msg>, url: &str) {
    match tungstenite::connect(url) {
        Ok((mut ws, _res)) => {
            obs_info!("Connected to {url}");
            loop {
                let msg = match ws.read() {
                    Ok(msg) => msg,
                    Err(e) => {
                        obs_warning!("Error reading from WebSocket: {e}");
                        break;
                    }
                };
                if let Err(_) = prod.send(Arc::new(msg.into_data().into())) {
                    break;
                }
            }
        }
        Err(e) => {
            obs_warning!("Failed to connect to {url}, falling back to dummy debug producer: {e}");
            loop {
                thread::sleep(time::Duration::from_secs(1));
                obs_debug!("Simulating WebSocket data");
                if let Err(_) = prod.send(Arc::new(b"SEI RAIDWIDE".to_vec())) {
                    break;
                }
            }
        }
    }
    obs_debug!("WebSocket thread finished");
}

pub extern "C" fn frontend_event_cb(
    event: obs_sys::obs_frontend_event,
    private_data: *mut ffi::c_void,
) {
    let source = unsafe { private_data.cast::<RaidwideSource>().as_mut_unchecked() };

    if event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_STREAMING_STARTED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STARTED
    {
        source.state.try_start(private_data);
    }

    if event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_STREAMING_STOPPED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STOPPED
    {
        source.state.try_stop(private_data);
    }
}

extern "C" fn enum_output_cb(data: *mut ffi::c_void, output: *mut obs_sys::obs_output_t) -> bool {
    let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };
    let signal_handler = unsafe { obs_sys::obs_output_get_signal_handler(output) };

    let source = unsafe { data.cast::<RaidwideSource>().as_mut_unchecked() };

    match &mut source.state {
        RaidwideSourceState::Idle => {
            obs_warning!("Enumerating outputs while raidwide is not running");
            return false;
        }
        RaidwideSourceState::Running {
            signal_subscribed, ..
        } => {
            if signal_subscribed.contains(output_name) {
                obs_debug!("Duplicate output signal subscription detected, ignoring");
                return true;
            }

            obs_info!(
                "Adding signal handlers for output {}",
                output_name.to_string_lossy()
            );

            signal_subscribed.insert(output_name.to_owned());
            unsafe {
                obs_sys::signal_handler_connect(
                    signal_handler,
                    c"activate".as_ptr(),
                    Some(output_activate_cb),
                    data,
                );
                obs_sys::signal_handler_connect(
                    signal_handler,
                    c"deactivate".as_ptr(),
                    Some(output_deactivate_cb),
                    data,
                );
                obs_sys::signal_handler_connect(
                    signal_handler,
                    c"stop".as_ptr(),
                    Some(output_stop_cb),
                    data,
                );

                if obs_sys::obs_output_active(output) {
                    source.state.register_output_hook(output);
                }
            };
        }
    }

    true
}

extern "C" fn output_activate_cb(data: *mut ffi::c_void, callback_data: *mut obs_sys::calldata_t) {
    let source = unsafe { data.cast::<RaidwideSource>().as_mut_unchecked() };
    let output =
        unsafe { calldata::calldata_ptr(callback_data, c"output").cast::<obs_sys::obs_output_t>() };
    source.state.register_output_hook(output);
}

extern "C" fn output_deactivate_cb(
    data: *mut ffi::c_void,
    callback_data: *mut obs_sys::calldata_t,
) {
    let source = unsafe { data.cast::<RaidwideSource>().as_mut_unchecked() };
    let output =
        unsafe { calldata::calldata_ptr(callback_data, c"output").cast::<obs_sys::obs_output_t>() };
    source.state.unregister_output_hook(output);
}

extern "C" fn output_stop_cb(data: *mut ffi::c_void, callback_data: *mut obs_sys::calldata_t) {
    let source = unsafe { data.cast::<RaidwideSource>().as_mut_unchecked() };
    let output =
        unsafe { calldata::calldata_ptr(callback_data, c"output").cast::<obs_sys::obs_output_t>() };
    source.state.unregister_output_signal_hooks(output, data);
}

// On linux the encoder_packet has a `ref_count: long` value allocated in
// front of pkt.
// TODO: check why this is not the case on windows.
const ENCODER_PACKET_ALLOC_OFFSET: usize = size_of::<ffi::c_long>();

extern "C" fn packet_cb(
    output: *mut obs_sys::obs_output_t,
    pkt_ref: *mut obs_sys::encoder_packet,
    _pkt_time: *mut obs_sys::encoder_packet_time,
    param: *mut ffi::c_void,
) {
    let mut pkt = unsafe { pkt_ref.as_mut_unchecked() }.to_owned();
    if pkt.type_ != obs_sys::obs_encoder_type_OBS_ENCODER_VIDEO {
        return
    }

    if unsafe { ffi::CStr::from_ptr(obs_sys::obs_encoder_get_codec(pkt.encoder)) } != c"h264" {
        obs_debug!("Unsupported codec");
        return;
    }

    let receiver = unsafe { param.cast::<sync::mpsc::Receiver<Msg>>().as_mut_unchecked() };
    let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };
    obs_debug!(
        "Received packet of size {} on output {}",
        pkt.size,
        output_name.to_string_lossy(),
    );

    let mut buffer = vec![];
    while let Ok(msg) = receiver.try_recv() {
        buffer.extend_from_slice(&msg)
    }

    if buffer.is_empty() {
        return;
    }

    let sei_data = sei::SeiUserDataUnregistered::new(&sei::RAIDWIDE_UUID, &buffer);
    let sei_size = sei_data.encoded_h264_size();

    // to properly handle ref_count on encoder_packet, we create a new
    // encoder_packet with a ref_count of 1 with our sei data prepended to the
    // original data, and decrement the ref_count of the original packet.
    let old_pkt_data = unsafe { slice::from_raw_parts(pkt.data, pkt.size) };
    let new_pkt_data = unsafe {
        let new_pkt_size = ENCODER_PACKET_ALLOC_OFFSET + sei_size + old_pkt_data.len();
        let new_pkt_ptr = obs_sys::bmalloc(new_pkt_size);
        slice::from_raw_parts_mut(new_pkt_ptr.cast::<u8>(), new_pkt_size)
    };

    new_pkt_data[0..ENCODER_PACKET_ALLOC_OFFSET]
        .copy_from_slice(&ffi::c_long::from(1).to_ne_bytes());
    sei_data
        .encode_h264(
            &mut new_pkt_data[ENCODER_PACKET_ALLOC_OFFSET..ENCODER_PACKET_ALLOC_OFFSET + sei_size],
        )
        .unwrap();
    new_pkt_data[ENCODER_PACKET_ALLOC_OFFSET + sei_size..].copy_from_slice(old_pkt_data);

    unsafe { obs_sys::obs_encoder_packet_release(pkt_ref) };

    pkt.size = new_pkt_data.len() - ENCODER_PACKET_ALLOC_OFFSET;
    pkt.data = unsafe {
        new_pkt_data
            .as_mut_ptr()
            .byte_add(ENCODER_PACKET_ALLOC_OFFSET)
    };

    unsafe {
        // TODO: check if its aligned
        ptr::write_unaligned(pkt_ref, pkt);
    }

    obs_info!("Appended {} bytes of SEI data", sei_size);
}
