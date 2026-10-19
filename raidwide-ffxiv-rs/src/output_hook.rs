use std::collections::VecDeque;
use std::ffi;
use std::mem;
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{self, mpsc};

use crate::calldata;
use crate::catch_panic;
use crate::metadata;
use crate::ws;
use crate::{obs_debug, obs_info, obs_warning};

/// An output whose activate and deactivate signals are connected to
struct TrackedOutput {
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
    /// Set once the output was found to encode with an unsupported codec
    unsupported_codec: AtomicBool,
    /// Replaced when the WebSocket URL changes, sending to the same queue
    connection: sync::Mutex<ws::Connection>,
    sender: mpsc::Sender<ws::Message>,
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

    /// The metadata payload of a frame rendered at `render_ns`: the messages
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

// SAFETY: OBS' weak references and the packet callback can be used from any
// thread, the rest of the hook is Send
unsafe impl Send for TrackedOutput {}

/// The plugin's hook on OBS' outputs. It is enabled while at least one
/// Raidwide source exists: adding the source to a scene collection is what
/// opts in, so that streams don't carry combat data otherwise. There is a
/// single hook however many sources there are, owned by the module rather
/// than a source, since OBS destroys sources on another thread.
struct Raidwide {
    /// Serializes enabling, disabling and tracking outputs, which happen on
    /// the UI thread and OBS' destruction thread. Signal callbacks never take
    /// it, so it may be held while connecting and disconnecting signals.
    lifecycle: sync::Mutex<Lifecycle>,
    state: sync::Mutex<State>,
}

struct Lifecycle {
    /// Number of existing Raidwide sources
    sources: usize,
}

/// Shared with the outputs' signal callbacks. OBS holds an output's signal
/// mutex while running its signal callbacks, which lock the state, so the
/// state must not be locked while connecting or disconnecting signals. The
/// packet callback doesn't lock the state, so (un)registering packet
/// callbacks with the state locked is fine.
struct State {
    ws_url: String,
    outputs: Vec<TrackedOutput>,
}

static RAIDWIDE: Raidwide = Raidwide {
    lifecycle: sync::Mutex::new(Lifecycle { sources: 0 }),
    state: sync::Mutex::new(State {
        ws_url: String::new(),
        outputs: Vec::new(),
    }),
};

fn lock<T>(mutex: &sync::Mutex<T>) -> sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(sync::PoisonError::into_inner)
}

pub fn load() {
    unsafe { obs_sys::obs_frontend_add_event_callback(Some(frontend_event_cb), ptr::null_mut()) };
}

pub fn unload() {
    unsafe {
        obs_sys::obs_frontend_remove_event_callback(Some(frontend_event_cb), ptr::null_mut())
    };
    // The sources are normally all destroyed by now
    let mut lifecycle = lock(&RAIDWIDE.lifecycle);
    if lifecycle.sources > 0 {
        lifecycle.sources = 0;
        untrack_outputs(&lifecycle);
    }
}

/// A Raidwide source was created: enables the hook with the first one
pub fn add_source() {
    let mut lifecycle = lock(&RAIDWIDE.lifecycle);
    lifecycle.sources += 1;
    if lifecycle.sources == 1 {
        obs_info!("Enabled, adding data to the outputs");
        // Hooks the outputs already active, e.g. when the source is added
        // during a recording, and follows the existing ones from their next
        // start
        track_outputs(&lifecycle);
    } else {
        obs_warning!(
            "{} Raidwide sources exist, only one is needed: they share the last WebSocket URL set",
            lifecycle.sources
        );
    }
}

/// A Raidwide source was destroyed: disables the hook with the last one
pub fn remove_source() {
    let mut lifecycle = lock(&RAIDWIDE.lifecycle);
    lifecycle.sources = lifecycle.sources.saturating_sub(1);
    if lifecycle.sources == 0 {
        obs_info!("Disabled, no Raidwide source left");
        untrack_outputs(&lifecycle);
    }
}

/// The OverlayPlugin WebSocket URL. The active outputs reconnect to it, keeping
/// their packet callback and the messages not inserted yet
pub fn set_ws_url(ws_url: String) {
    let mut state = lock(&RAIDWIDE.state);
    // Sources are updated with unchanged settings too, e.g. when created
    if state.ws_url == ws_url {
        return;
    }
    state.ws_url = ws_url;

    for tracked in &state.outputs {
        let Some(hook) = &tracked.hook else {
            continue;
        };
        obs_debug!("Reconnecting an active output to {}", state.ws_url);
        let connection = ws::Connection::spawn(state.ws_url.clone(), hook.sender.clone());
        // Not joined: OBS updates the source on every keystroke in the URL
        // field, and the previous connection may be stuck connecting
        mem::replace(&mut *lock(&hook.connection), connection).stop_in_background();
    }
}

pub fn ws_url() -> String {
    lock(&RAIDWIDE.state).ws_url.clone()
}

/// Connect to the signals of the outputs not tracked yet, and hook the active
/// ones. Outputs are only enumerated when something starts, so the ones
/// created since the last enumeration are picked up then.
fn track_outputs(_lifecycle: &Lifecycle) {
    // Forget the outputs OBS destroyed, their signals and packet callbacks
    // went with them. Their connections are stopped outside of the lock.
    let destroyed: Vec<_> = lock(&RAIDWIDE.state)
        .outputs
        .extract_if(.., |tracked| !tracked.is_alive())
        .collect();
    drop(destroyed);

    let mut outputs: Vec<*mut obs_sys::obs_output_t> = vec![];
    unsafe {
        obs_sys::obs_enum_outputs(Some(collect_output_cb), ptr::from_mut(&mut outputs).cast())
    };

    for output in outputs {
        track_output(output);
        unsafe { obs_sys::obs_output_release(output) };
    }
}

fn track_output(output: *mut obs_sys::obs_output_t) {
    {
        let mut state = lock(&RAIDWIDE.state);
        if !state.outputs.iter().any(|tracked| tracked.is(output)) {
            obs_info!("Adding signal handlers for output {}", output_name(output));
            state.outputs.push(TrackedOutput::new(output));
            drop(state);
            unsafe { connect_signals(output) };
        }
    }

    // Covers outputs that were active before their signals were connected
    if unsafe { obs_sys::obs_output_active(output) } {
        hook_output(output, false);
    }
}

/// Stop tracking every output
fn untrack_outputs(_lifecycle: &Lifecycle) {
    // Signal callbacks running meanwhile find nothing to hook or unhook
    let tracked_outputs = mem::take(&mut lock(&RAIDWIDE.state).outputs);
    for mut tracked in tracked_outputs {
        let output = tracked.upgrade();
        if output.is_null() {
            continue;
        }

        // Returns once the signal callbacks in progress have finished
        unsafe { disconnect_signals(output) };
        if let Some(hook) = tracked.hook.take() {
            unsafe { remove_packet_callback(output, &hook) };
        }
        unsafe { obs_sys::obs_output_release(output) };
    }
}

/// Start the output's connection and packet callback. `restart` replaces an
/// existing hook: an activation starts a new recording or stream, which needs
/// a new connection to get OverlayPlugin's initial messages
fn hook_output(output: *mut obs_sys::obs_output_t, restart: bool) {
    let mut state = lock(&RAIDWIDE.state);
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
        unsupported_codec: AtomicBool::new(false),
        connection: sync::Mutex::new(ws::Connection::spawn(ws_url, sender.clone())),
        sender,
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

fn unhook_output(output: *mut obs_sys::obs_output_t) {
    let hook = lock(&RAIDWIDE.state)
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

unsafe fn connect_signals(output: *mut obs_sys::obs_output_t) {
    let signal_handler = unsafe { obs_sys::obs_output_get_signal_handler(output) };
    unsafe {
        obs_sys::signal_handler_connect(
            signal_handler,
            c"activate".as_ptr(),
            Some(output_activate_cb),
            ptr::null_mut(),
        );
        obs_sys::signal_handler_connect(
            signal_handler,
            c"deactivate".as_ptr(),
            Some(output_deactivate_cb),
            ptr::null_mut(),
        );
    }
}

unsafe fn disconnect_signals(output: *mut obs_sys::obs_output_t) {
    let signal_handler = unsafe { obs_sys::obs_output_get_signal_handler(output) };
    unsafe {
        obs_sys::signal_handler_disconnect(
            signal_handler,
            c"activate".as_ptr(),
            Some(output_activate_cb),
            ptr::null_mut(),
        );
        obs_sys::signal_handler_disconnect(
            signal_handler,
            c"deactivate".as_ptr(),
            Some(output_deactivate_cb),
            ptr::null_mut(),
        );
    }
}

fn output_name<'a>(output: *mut obs_sys::obs_output_t) -> std::borrow::Cow<'a, str> {
    unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) }.to_string_lossy()
}

extern "C" fn frontend_event_cb(event: obs_sys::obs_frontend_event, _data: *mut ffi::c_void) {
    if event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_STREAMING_STARTED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STARTED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_REPLAY_BUFFER_STARTED
    {
        catch_panic("frontend event callback", (), || {
            let lifecycle = lock(&RAIDWIDE.lifecycle);
            if lifecycle.sources > 0 {
                track_outputs(&lifecycle);
            }
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

extern "C" fn output_activate_cb(_data: *mut ffi::c_void, callback_data: *mut obs_sys::calldata_t) {
    catch_panic("output activate callback", (), || {
        let output = unsafe {
            calldata::calldata_ptr(callback_data, c"output").cast::<obs_sys::obs_output_t>()
        };
        hook_output(output, true);
    })
}

extern "C" fn output_deactivate_cb(
    _data: *mut ffi::c_void,
    callback_data: *mut obs_sys::calldata_t,
) {
    catch_panic("output deactivate callback", (), || {
        let output = unsafe {
            calldata::calldata_ptr(callback_data, c"output").cast::<obs_sys::obs_output_t>()
        };
        unhook_output(output);
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
        add_metadata(output, pkt_ref, pkt_time, param)
    })
}

/// # Safety
/// The packet callback's arguments, `param` pointing to the output's hook
unsafe fn add_metadata(
    output: *mut obs_sys::obs_output_t,
    pkt_ref: *mut obs_sys::encoder_packet,
    pkt_time: *mut obs_sys::encoder_packet_time,
    param: *mut ffi::c_void,
) {
    if unsafe { (*pkt_ref).type_ } != obs_sys::obs_encoder_type_OBS_ENCODER_VIDEO {
        return;
    }
    let mut pkt = unsafe { pkt_ref.read() };
    let hook = unsafe { param.cast::<OutputHook>().as_ref_unchecked() };

    let codec = unsafe { ffi::CStr::from_ptr(obs_sys::obs_encoder_get_codec(pkt.encoder)) };
    let codec = match metadata::Codec::from_obs_name(codec.to_bytes()) {
        Some(codec) => codec,
        None => {
            if !hook.unsupported_codec.swap(true, Ordering::Relaxed) {
                obs_warning!(
                    "Output {} encodes its video with {}, only H.264, H.265 and AV1 are supported: no data is added to it",
                    output_name(output),
                    codec.to_string_lossy()
                );
            }
            return;
        }
    };

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

    // to properly handle ref_count on encoder_packet, we create a new
    // encoder_packet with a ref_count of 1 with our metadata inserted in the
    // original data, and decrement the ref_count of the original packet.
    let old_pkt_data = unsafe { slice::from_raw_parts(pkt.data, pkt.size) };
    let packet_size = metadata::packet_size(codec, old_pkt_data, &buffer);
    let metadata_size = packet_size - old_pkt_data.len();
    let new_pkt_data = unsafe {
        let new_pkt_size = ENCODER_PACKET_ALLOC_OFFSET + packet_size;
        let new_pkt_ptr = obs_sys::bmalloc(new_pkt_size);
        slice::from_raw_parts_mut(new_pkt_ptr.cast::<u8>(), new_pkt_size)
    };

    let (ref_count, data) = new_pkt_data.split_at_mut(ENCODER_PACKET_ALLOC_OFFSET);
    ref_count.copy_from_slice(&ffi::c_long::from(1).to_ne_bytes());
    metadata::splice_packet(codec, old_pkt_data, &buffer, data).unwrap();

    unsafe { obs_sys::obs_encoder_packet_release(pkt_ref) };

    pkt.size = data.len();
    pkt.data = data.as_mut_ptr();
    unsafe { *pkt_ref = pkt };

    obs_debug!("Added {metadata_size} bytes of {codec:?} metadata");
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
