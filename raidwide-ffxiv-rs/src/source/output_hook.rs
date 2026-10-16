use std::collections::VecDeque;
use std::ffi;
use std::mem;
use std::ptr;
use std::slice;
use std::sync::{self, mpsc};

use crate::calldata;
use crate::catch_panic;
use crate::sei;
use crate::source::RaidwideSource;
use crate::ws;
use crate::{obs_debug, obs_info, obs_warning};

/// An output whose activate and deactivate signals the source is connected to
pub struct TrackedOutput {
    /// Weak reference: OBS destroys and recreates outputs (e.g. after a
    /// settings change), and a new one can reuse a destroyed one's address
    weak: *mut obs_sys::obs_weak_output_t,
    /// Set while the output is active
    hook: Option<Box<OutputHook>>,
}

/// The state of an active output: its own OverlayPlugin connection, so that
/// it gets the messages OverlayPlugin sends when a connection opens (primary
/// player, zone), whenever it starts. The packet callback's parameter points
/// to it, hence the box
struct OutputHook {
    /// Only used by the packet callback, which OBS doesn't run concurrently
    /// for an output, so the lock is never contended
    messages: sync::Mutex<MessageQueue>,
    _connection: ws::Connection,
}

/// An output's messages, waiting for the first frame rendered after them.
/// A packet reaches the packet callback well after its frame was rendered
/// (encoder lookahead, B-frame reordering, interleaving with audio, stream
/// delay), so attaching every message received so far to it would place them
/// too early in the video.
///
/// Packets also come in decode order: a P-frame comes before the B-frames
/// displayed ahead of it, and takes the messages received before them too.
/// So each message goes with its offset to the render of the frame carrying
/// it, for the analysis to place it at that frame's PTS plus the offset.
struct MessageQueue {
    receiver: mpsc::Receiver<ws::Message>,
    pending: VecDeque<ws::Message>,
}

impl MessageQueue {
    fn new(receiver: mpsc::Receiver<ws::Message>) -> Self {
        Self {
            receiver,
            pending: VecDeque::new(),
        }
    }

    /// The SEI payload of a frame rendered at `render_ns`: the messages
    /// received before it, or all of them when its render time is unknown,
    /// each one as `{"offset_ns":<received - rendered, <= 0>,"message":<raw>}`
    /// (0 without render time), concatenated
    fn take_received_before(&mut self, render_ns: Option<u64>) -> Vec<u8> {
        // Received in order, so the pending messages are sorted
        self.pending.extend(self.receiver.try_iter());

        let mut buffer = vec![];
        while let Some(msg) = self.pending.front() {
            let offset_ns = match render_ns {
                Some(render_ns) if msg.received_ns > render_ns => break,
                Some(render_ns) => -i64::try_from(render_ns - msg.received_ns).unwrap_or(i64::MAX),
                None => 0,
            };
            buffer.extend_from_slice(format!(r#"{{"offset_ns":{offset_ns},"message":"#).as_bytes());
            buffer.extend_from_slice(&msg.data);
            buffer.push(b'}');
            self.pending.pop_front();
        }
        buffer
    }
}

impl TrackedOutput {
    fn new(output: *mut obs_sys::obs_output_t) -> Self {
        Self {
            weak: unsafe { obs_sys::obs_output_get_weak_output(output) },
            hook: None,
        }
    }

    fn is(&self, output: *mut obs_sys::obs_output_t) -> bool {
        unsafe { obs_sys::obs_weak_output_references_output(self.weak, output) }
    }

    /// A strong reference to the output, null if it was destroyed
    fn upgrade(&self) -> *mut obs_sys::obs_output_t {
        unsafe { obs_sys::obs_weak_output_get_output(self.weak) }
    }

    fn is_alive(&self) -> bool {
        let output = self.upgrade();
        unsafe { obs_sys::obs_output_release(output) };
        !output.is_null()
    }
}

impl Drop for TrackedOutput {
    fn drop(&mut self) {
        unsafe { obs_sys::obs_weak_output_release(self.weak) };
    }
}

// Locking: OBS holds an output's signal mutex while running its signal
// callbacks, which lock the source's state, so the state must not be locked
// while connecting or disconnecting signals. The packet callback doesn't lock
// the state, so (un)registering packet callbacks with the state locked is fine.

/// Connect to the signals of the outputs not tracked yet, and hook the active
/// ones. Outputs are only enumerated when something starts, so the ones
/// created since the last enumeration are picked up then.
pub fn track_outputs(data: *mut ffi::c_void) {
    let source = unsafe { RaidwideSource::from_data(data) };

    // Forget the outputs OBS destroyed, their signals and packet callbacks
    // went with them. Their connections are stopped outside of the lock.
    let destroyed: Vec<_> = source
        .state()
        .outputs
        .extract_if(.., |tracked| !tracked.is_alive())
        .collect();
    drop(destroyed);

    let mut outputs: Vec<*mut obs_sys::obs_output_t> = vec![];
    unsafe {
        obs_sys::obs_enum_outputs(Some(collect_output_cb), ptr::from_mut(&mut outputs).cast())
    };

    for output in outputs {
        track_output(source, output, data);
        unsafe { obs_sys::obs_output_release(output) };
    }
}

fn track_output(
    source: &RaidwideSource,
    output: *mut obs_sys::obs_output_t,
    data: *mut ffi::c_void,
) {
    {
        let mut state = source.state();
        if !state.outputs.iter().any(|tracked| tracked.is(output)) {
            obs_info!("Adding signal handlers for output {}", output_name(output));
            state.outputs.push(TrackedOutput::new(output));
            drop(state);
            unsafe { connect_signals(output, data) };
        }
    }

    // Covers outputs that were active before their signals were connected
    if unsafe { obs_sys::obs_output_active(output) } {
        hook_output(source, output, false);
    }
}

/// Stop tracking every output, for the source's destruction
pub fn untrack_outputs(data: *mut ffi::c_void) {
    let source = unsafe { RaidwideSource::from_data(data) };

    // Signal callbacks running meanwhile find nothing to hook or unhook
    let tracked_outputs = mem::take(&mut source.state().outputs);
    for mut tracked in tracked_outputs {
        let output = tracked.upgrade();
        if output.is_null() {
            continue;
        }

        // Returns once the signal callbacks in progress have finished
        unsafe { disconnect_signals(output, data) };
        if let Some(hook) = tracked.hook.take() {
            unsafe { remove_packet_callback(output, &hook) };
        }
        unsafe { obs_sys::obs_output_release(output) };
    }
}

/// Start the output's connection and packet callback. `restart` replaces an
/// existing hook: an activation starts a new recording or stream, which needs
/// a new connection to get OverlayPlugin's initial messages
fn hook_output(source: &RaidwideSource, output: *mut obs_sys::obs_output_t, restart: bool) {
    let mut state = source.state();
    let ws_url = state.ws_url.clone();
    let Some(tracked) = state.outputs.iter_mut().find(|tracked| tracked.is(output)) else {
        obs_debug!(
            "Output {} is not tracked, not hooking it",
            output_name(output)
        );
        return;
    };

    if tracked.hook.is_some() && !restart {
        return;
    }
    // Dropped (which stops its connection) once the state is unlocked
    let previous = tracked.hook.take();
    if let Some(hook) = &previous {
        obs_debug!("Replacing the hook of output {}", output_name(output));
        unsafe { remove_packet_callback(output, hook) };
    }

    obs_debug!(
        "Registering packet callback for output {}",
        output_name(output)
    );
    let (sender, receiver) = mpsc::channel();
    let hook = Box::new(OutputHook {
        messages: sync::Mutex::new(MessageQueue::new(receiver)),
        _connection: ws::Connection::spawn(ws_url, sender),
    });
    unsafe {
        obs_sys::obs_output_add_packet_callback(
            output,
            Some(packet_cb),
            ptr::from_ref::<OutputHook>(&hook).cast_mut().cast(),
        )
    };
    tracked.hook = Some(hook);
    drop(state);
    drop(previous);
}

fn unhook_output(source: &RaidwideSource, output: *mut obs_sys::obs_output_t) {
    let hook = source
        .state()
        .outputs
        .iter_mut()
        .find(|tracked| tracked.is(output))
        .and_then(|tracked| tracked.hook.take());

    if let Some(hook) = hook {
        obs_debug!(
            "Unregistering packet callback for output {}",
            output_name(output)
        );
        // Returns once a packet callback in progress has finished, so the
        // hook can be dropped (which stops its connection) afterwards
        unsafe { remove_packet_callback(output, &hook) };
    }
}

unsafe fn remove_packet_callback(output: *mut obs_sys::obs_output_t, hook: &OutputHook) {
    unsafe {
        obs_sys::obs_output_remove_packet_callback(
            output,
            Some(packet_cb),
            ptr::from_ref(hook).cast_mut().cast(),
        )
    };
}

unsafe fn connect_signals(output: *mut obs_sys::obs_output_t, data: *mut ffi::c_void) {
    let signal_handler = unsafe { obs_sys::obs_output_get_signal_handler(output) };
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
    }
}

unsafe fn disconnect_signals(output: *mut obs_sys::obs_output_t, data: *mut ffi::c_void) {
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
    }
}

fn output_name<'a>(output: *mut obs_sys::obs_output_t) -> std::borrow::Cow<'a, str> {
    unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) }.to_string_lossy()
}

pub extern "C" fn frontend_event_cb(
    event: obs_sys::obs_frontend_event,
    private_data: *mut ffi::c_void,
) {
    if event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_STREAMING_STARTED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STARTED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_REPLAY_BUFFER_STARTED
    {
        catch_panic("frontend event callback", (), || {
            track_outputs(private_data)
        });
    }
}

/// Collects a strong reference to every output, so they are tracked outside
/// of the outputs' list lock
extern "C" fn collect_output_cb(
    param: *mut ffi::c_void,
    output: *mut obs_sys::obs_output_t,
) -> bool {
    let outputs = unsafe {
        param
            .cast::<Vec<*mut obs_sys::obs_output_t>>()
            .as_mut_unchecked()
    };
    let output = unsafe { obs_sys::obs_output_get_ref(output) };
    if !output.is_null() {
        outputs.push(output);
    }
    true
}

extern "C" fn output_activate_cb(data: *mut ffi::c_void, callback_data: *mut obs_sys::calldata_t) {
    catch_panic("output activate callback", (), || {
        let source = unsafe { RaidwideSource::from_data(data) };
        let output = unsafe {
            calldata::calldata_ptr(callback_data, c"output").cast::<obs_sys::obs_output_t>()
        };
        hook_output(source, output, true);
    })
}

extern "C" fn output_deactivate_cb(
    data: *mut ffi::c_void,
    callback_data: *mut obs_sys::calldata_t,
) {
    catch_panic("output deactivate callback", (), || {
        let source = unsafe { RaidwideSource::from_data(data) };
        let output = unsafe {
            calldata::calldata_ptr(callback_data, c"output").cast::<obs_sys::obs_output_t>()
        };
        unhook_output(source, output);
    })
}

// The encoder_packet has a `ref_count: long` value allocated in
// front of pkt.
const ENCODER_PACKET_ALLOC_OFFSET: usize = size_of::<ffi::c_long>();

extern "C" fn packet_cb(
    output: *mut obs_sys::obs_output_t,
    pkt_ref: *mut obs_sys::encoder_packet,
    pkt_time: *mut obs_sys::encoder_packet_time,
    param: *mut ffi::c_void,
) {
    catch_panic("packet callback", (), || unsafe {
        add_sei(output, pkt_ref, pkt_time, param)
    })
}

/// # Safety
/// The packet callback's arguments, `param` pointing to the output's hook
unsafe fn add_sei(
    output: *mut obs_sys::obs_output_t,
    pkt_ref: *mut obs_sys::encoder_packet,
    pkt_time: *mut obs_sys::encoder_packet_time,
    param: *mut ffi::c_void,
) {
    let mut pkt = unsafe { pkt_ref.as_mut_unchecked() }.to_owned();
    if pkt.type_ != obs_sys::obs_encoder_type_OBS_ENCODER_VIDEO {
        return;
    }

    if unsafe { ffi::CStr::from_ptr(obs_sys::obs_encoder_get_codec(pkt.encoder)) } != c"h264" {
        obs_debug!("Unsupported codec");
        return;
    }

    let hook = unsafe { param.cast::<OutputHook>().as_ref_unchecked() };
    let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };
    obs_debug!(
        "Received packet of size {} on output {}",
        pkt.size,
        output_name.to_string_lossy(),
    );

    // OBS matches the timing to the packet by PTS, it is missing if that fails
    let render_ns = unsafe { pkt_time.as_ref() }.map(|time| time.cts);
    let buffer = hook
        .messages
        .lock()
        .unwrap_or_else(sync::PoisonError::into_inner)
        .take_received_before(render_ns);

    if buffer.is_empty() {
        return;
    }

    let sei_data = sei::SeiUserDataUnregistered::new(&sei::RAIDWIDE_UUID, &buffer);
    let sei_size = sei_data.encoded_h264_size();
    let mut sei_nal = vec![0; sei_size];
    if let Err(e) = sei_data.encode_h264(&mut sei_nal) {
        obs_warning!("Failed to encode the SEI, dropping its data: {e}");
        return;
    }

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
    new_pkt_data[ENCODER_PACKET_ALLOC_OFFSET..ENCODER_PACKET_ALLOC_OFFSET + sei_size]
        .copy_from_slice(&sei_nal);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn message(received_ns: u64, data: &[u8]) -> ws::Message {
        ws::Message {
            received_ns,
            data: data.to_vec(),
        }
    }

    #[test]
    fn takes_the_messages_received_before_the_frame() {
        let (sender, receiver) = mpsc::channel();
        let mut queue = MessageQueue::new(receiver);
        sender.send(message(10, b"1")).unwrap();
        sender.send(message(20, b"2")).unwrap();
        sender.send(message(30, b"3")).unwrap();

        assert_eq!(queue.take_received_before(Some(5)), b"");
        assert_eq!(
            queue.take_received_before(Some(20)),
            br#"{"offset_ns":-10,"message":1}{"offset_ns":0,"message":2}"#
        );
        sender.send(message(40, b"4")).unwrap();
        assert_eq!(
            queue.take_received_before(Some(35)),
            br#"{"offset_ns":-5,"message":3}"#
        );
        assert_eq!(queue.take_received_before(Some(35)), b"");
        assert_eq!(
            queue.take_received_before(Some(40)),
            br#"{"offset_ns":0,"message":4}"#
        );
    }

    #[test]
    fn takes_everything_without_render_time() {
        let (sender, receiver) = mpsc::channel();
        let mut queue = MessageQueue::new(receiver);
        sender.send(message(10, b"1")).unwrap();
        assert_eq!(queue.take_received_before(Some(5)), b"");
        sender.send(message(20, b"2")).unwrap();
        assert_eq!(
            queue.take_received_before(None),
            br#"{"offset_ns":0,"message":1}{"offset_ns":0,"message":2}"#
        );
    }
}
