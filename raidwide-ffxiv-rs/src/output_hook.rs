use std::collections::VecDeque;
use std::ffi;
use std::mem;
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{self, mpsc};

use crate::calldata;
use crate::catch_panic;
use crate::sei;
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
    /// Set once the output was found to encode with another codec than H.264
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
///
/// Each message is also numbered, from 0 for each hook (a hook lives while its
/// output is active), so the analysis can tell when messages were lost (frames
/// or packets dropped on a live link) and keep a single copy of the repeated
/// ones.
///
/// Messages can be repeated in the next frames (`set_repeat_frames`), so that
/// one frame lost on a live link doesn't lose them. Each copy gets its offset
/// to the frame carrying it, so whichever copy the analysis keeps is placed
/// right
struct MessageQueue {
    receiver: mpsc::Receiver<ws::Message>,
    pending: VecDeque<ws::Message>,
    /// Messages already sent, to send again in the next frames
    repeating: VecDeque<Repeat>,
    /// Sequence number of the next message taken
    next_seq: u64,
}

struct Repeat {
    seq: u64,
    message: ws::Message,
    /// Copies left to send
    remaining: u32,
}

impl MessageQueue {
    fn new(receiver: mpsc::Receiver<ws::Message>) -> Self {
        Self {
            receiver,
            pending: VecDeque::new(),
            repeating: VecDeque::new(),
            next_seq: 0,
        }
    }

    /// The SEI payload of a frame rendered at `render_ns`: the copies of the
    /// messages already sent, then the messages received before it, or all
    /// of them when its render time is unknown, each one as
    /// `{"seq":<n>,"offset_ns":<received - rendered, <= 0>,"message":<raw>}`
    /// (offset 0 without render time), concatenated. Each new message is sent
    /// again in the next `repeat` frames rendered after it
    fn take_received_before(&mut self, render_ns: Option<u64>, repeat: u32) -> Vec<u8> {
        // Received in order, so the pending messages are sorted
        self.pending.extend(self.receiver.try_iter());

        let mut buffer = vec![];
        for copy in &mut self.repeating {
            // Skips the frames rendered before the message, B-frames decoded
            // after the frame that carried it
            let Some(offset_ns) = offset_ns(render_ns, copy.message.received_ns) else {
                continue;
            };
            append_message(&mut buffer, copy.seq, offset_ns, &copy.message.data);
            copy.remaining -= 1;
        }
        self.repeating.retain(|copy| copy.remaining > 0);

        while let Some(msg) = self.pending.front() {
            let Some(offset_ns) = offset_ns(render_ns, msg.received_ns) else {
                break;
            };
            let seq = self.next_seq;
            append_message(&mut buffer, seq, offset_ns, &msg.data);
            self.next_seq += 1;
            let Some(message) = self.pending.pop_front() else {
                break;
            };
            if repeat > 0 {
                self.repeating.push_back(Repeat {
                    seq,
                    message,
                    remaining: repeat,
                });
            }
        }
        buffer
    }
}

/// Offset of a message received at `received_ns` to the render of the frame
/// carrying it, 0 when the render time is unknown, None when the frame was
/// rendered before the message
fn offset_ns(render_ns: Option<u64>, received_ns: u64) -> Option<i64> {
    match render_ns {
        Some(render_ns) if received_ns > render_ns => None,
        Some(render_ns) => Some(-i64::try_from(render_ns - received_ns).unwrap_or(i64::MAX)),
        None => Some(0),
    }
}

fn append_message(buffer: &mut Vec<u8>, seq: u64, offset_ns: i64, data: &[u8]) {
    buffer.extend_from_slice(
        format!(r#"{{"seq":{seq},"offset_ns":{offset_ns},"message":"#).as_bytes(),
    );
    buffer.extend_from_slice(data);
    buffer.push(b'}');
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

/// How many frames after the first one each message is repeated in, set by
/// the sources (the last one set wins, as the URL)
static REPEAT_FRAMES: AtomicU32 = AtomicU32::new(1);

/// Upper bound of `set_repeat_frames`
pub const MAX_REPEAT_FRAMES: u32 = 10;

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

/// Repeats each message in the next `frames` frames (0: sent once), up to
/// `MAX_REPEAT_FRAMES`. Applies to the frames encoded from now on
pub fn set_repeat_frames(frames: u32) {
    REPEAT_FRAMES.store(frames.min(MAX_REPEAT_FRAMES), Ordering::Relaxed);
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
    if unsafe { (*pkt_ref).type_ } != obs_sys::obs_encoder_type_OBS_ENCODER_VIDEO {
        return;
    }
    let mut pkt = unsafe { pkt_ref.read() };
    let hook = unsafe { param.cast::<OutputHook>().as_ref_unchecked() };

    let codec = unsafe { ffi::CStr::from_ptr(obs_sys::obs_encoder_get_codec(pkt.encoder)) };
    if codec != c"h264" {
        if !hook.unsupported_codec.swap(true, Ordering::Relaxed) {
            obs_warning!(
                "Output {} encodes its video with {}, only H.264 is supported: no data is added to it",
                output_name(output),
                codec.to_string_lossy()
            );
        }
        return;
    }

    // OBS matches the timing to the packet by PTS, it is missing if that fails
    let render_ns = unsafe { pkt_time.as_ref() }.map(|time| time.cts);
    let buffer = hook
        .messages
        .lock()
        .unwrap_or_else(sync::PoisonError::into_inner)
        .take_received_before(render_ns, REPEAT_FRAMES.load(Ordering::Relaxed));

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
    // encoder_packet with a ref_count of 1 with our sei data inserted in the
    // original data, and decrement the ref_count of the original packet.
    let old_pkt_data = unsafe { slice::from_raw_parts(pkt.data, pkt.size) };
    let (before_sei, after_sei) = old_pkt_data.split_at(sei::h264_insertion_offset(old_pkt_data));
    let new_pkt_data = unsafe {
        let new_pkt_size = ENCODER_PACKET_ALLOC_OFFSET + sei_size + old_pkt_data.len();
        let new_pkt_ptr = obs_sys::bmalloc(new_pkt_size);
        slice::from_raw_parts_mut(new_pkt_ptr.cast::<u8>(), new_pkt_size)
    };

    let (ref_count, data) = new_pkt_data.split_at_mut(ENCODER_PACKET_ALLOC_OFFSET);
    ref_count.copy_from_slice(&ffi::c_long::from(1).to_ne_bytes());
    let (data_before_sei, rest) = data.split_at_mut(before_sei.len());
    let (data_sei, data_after_sei) = rest.split_at_mut(sei_size);
    data_before_sei.copy_from_slice(before_sei);
    data_sei.copy_from_slice(&sei_nal);
    data_after_sei.copy_from_slice(after_sei);

    unsafe { obs_sys::obs_encoder_packet_release(pkt_ref) };

    pkt.size = data.len();
    pkt.data = data.as_mut_ptr();
    unsafe { *pkt_ref = pkt };

    obs_debug!("Added {} bytes of SEI data", sei_size);
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

        assert_eq!(queue.take_received_before(Some(5), 0), b"");
        assert_eq!(
            queue.take_received_before(Some(20), 0),
            br#"{"seq":0,"offset_ns":-10,"message":1}{"seq":1,"offset_ns":0,"message":2}"#
        );
        sender.send(message(40, b"4")).unwrap();
        assert_eq!(
            queue.take_received_before(Some(35), 0),
            br#"{"seq":2,"offset_ns":-5,"message":3}"#
        );
        assert_eq!(queue.take_received_before(Some(35), 0), b"");
        assert_eq!(
            queue.take_received_before(Some(40), 0),
            br#"{"seq":3,"offset_ns":0,"message":4}"#
        );
    }

    #[test]
    fn takes_everything_without_render_time() {
        let (sender, receiver) = mpsc::channel();
        let mut queue = MessageQueue::new(receiver);
        sender.send(message(10, b"1")).unwrap();
        assert_eq!(queue.take_received_before(Some(5), 0), b"");
        sender.send(message(20, b"2")).unwrap();
        assert_eq!(
            queue.take_received_before(None, 0),
            br#"{"seq":0,"offset_ns":0,"message":1}{"seq":1,"offset_ns":0,"message":2}"#
        );
    }

    #[test]
    fn repeats_the_messages_in_the_next_frames() {
        let (sender, receiver) = mpsc::channel();
        let mut queue = MessageQueue::new(receiver);
        sender.send(message(10, b"1")).unwrap();
        assert_eq!(
            queue.take_received_before(Some(20), 2),
            br#"{"seq":0,"offset_ns":-10,"message":1}"#
        );
        sender.send(message(25, b"2")).unwrap();
        // The copies first, with their offset to this frame
        assert_eq!(
            queue.take_received_before(Some(30), 2),
            br#"{"seq":0,"offset_ns":-20,"message":1}{"seq":1,"offset_ns":-5,"message":2}"#
        );
        // A B-frame rendered before message 2 only carries message 1
        assert_eq!(
            queue.take_received_before(Some(22), 2),
            br#"{"seq":0,"offset_ns":-12,"message":1}"#
        );
        assert_eq!(
            queue.take_received_before(Some(40), 2),
            br#"{"seq":1,"offset_ns":-15,"message":2}"#
        );
        assert_eq!(
            queue.take_received_before(Some(50), 2),
            br#"{"seq":1,"offset_ns":-25,"message":2}"#
        );
        assert_eq!(queue.take_received_before(Some(60), 2), b"");
    }
}
