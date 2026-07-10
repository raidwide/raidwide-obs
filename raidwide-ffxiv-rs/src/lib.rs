use std::ffi;
use std::fmt;
use std::io;
use std::io::Write;
use std::ptr;
use std::slice;

static mut OBS_MODULE_POINTER: *mut obs_sys::obs_module_t = std::ptr::null_mut();

const PLUGIN_NAME: &str = env!("CARGO_PKG_NAME");
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

const UUID_SIZE: usize = 16;
const RAIDWIDE_UUID: [u8; UUID_SIZE] = [
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

    unsafe {
        obs_sys::obs_frontend_add_event_callback(Some(frontend_event_cb), ptr::null_mut());
    }

    true
}

#[unsafe(no_mangle)]
pub extern "C" fn obs_module_unload() {
    obs_log!(obs_sys::LOG_INFO, "Plugin unloaded");
}

// MODULE IMPLEMENTATION

extern "C" fn frontend_event_cb(
    event: obs_sys::obs_frontend_event,
    _private_data: *mut ffi::c_void,
) {
    if event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_STREAMING_STARTED
        || event == obs_sys::obs_frontend_event_OBS_FRONTEND_EVENT_RECORDING_STARTED
    {
        obs_log!(
            obs_sys::LOG_INFO,
            "Recording or streaming started, hooking output"
        );
        unsafe { obs_sys::obs_enum_outputs(Some(enum_output_cb), ptr::null_mut()) };
    }
}

extern "C" fn enum_output_cb(_data: *mut ffi::c_void, output: *mut obs_sys::obs_output_t) -> bool {
    let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };
    obs_log!(
        obs_sys::LOG_INFO,
        "Found output {}",
        output_name.to_string_lossy()
    );

    unsafe {
        obs_sys::obs_output_remove_packet_callback(output, Some(packet_cb), ptr::null_mut());
        obs_sys::obs_output_add_packet_callback(output, Some(packet_cb), ptr::null_mut());
    }

    true
}

struct SeiUserDataUnregistered<'u, 'p> {
    uuid: &'u [u8; UUID_SIZE],
    payload: &'p [u8],
}

impl<'u, 'p> SeiUserDataUnregistered<'u, 'p> {
    const START_CODE: [u8; 4] = [0, 0, 0, 1];
    const NAL_HEADER: [u8; 1] = [6]; // SEI = 6
    const PAYLOAD_TYPE: [u8; 1] = [5]; // Unregistered User Data = 5
    const NAL_TRAILER: [u8; 1] = [0x80];

    fn sei_size(&self) -> usize {
        self.uuid.len() + self.payload.len()
    }

    pub fn encoded_size(&self) -> usize {
        Self::START_CODE.len()
            + Self::NAL_HEADER.len()
            + Self::PAYLOAD_TYPE.len()
            + (1 + self.sei_size().div_euclid(0xFF)) // size of varint(sei_size)
            + self.sei_size()
            + Self::NAL_TRAILER.len()
    }

    pub fn encode(&self, out: &mut [u8]) -> io::Result<usize> {
        if out.len() < self.encoded_size() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output buffer is too small",
            ));
        }

        let mut offset = 0;
        offset += (&mut out[offset..]).write(&Self::START_CODE)?;
        offset += (&mut out[offset..]).write(&Self::NAL_HEADER)?;
        offset += (&mut out[offset..]).write(&Self::PAYLOAD_TYPE)?;

        // TODO: chunk this write if very large
        for _ in 0..self.sei_size().div_euclid(0xFF) {
            offset += (&mut out[offset..]).write(&[0xFF])?;
        }
        offset += (&mut out[offset..]).write(&[self.sei_size().rem_euclid(0xFF) as u8])?;

        offset += (&mut out[offset..]).write(self.uuid)?;
        offset += (&mut out[offset..]).write(self.payload)?;
        offset += (&mut out[offset..]).write(&Self::NAL_TRAILER)?;

        Ok(offset)
    }
}

extern "C" fn packet_cb(
    output: *mut obs_sys::obs_output_t,
    pkt: *mut obs_sys::encoder_packet,
    _pkt_time: *mut obs_sys::encoder_packet_time,
    _param: *mut ffi::c_void,
) {
    const LINUX_REF_COUNT_OFFSFET: usize = size_of::<ffi::c_long>();

    let pkt = unsafe { pkt.as_mut_unchecked() };
    let output_name = unsafe { ffi::CStr::from_ptr(obs_sys::obs_output_get_name(output)) };

    if pkt.type_ != obs_sys::obs_encoder_type_OBS_ENCODER_VIDEO {
        return;
    }

    obs_log!(
        obs_sys::LOG_INFO,
        "Received packet of size {} on output {}",
        pkt.size,
        output_name.to_string_lossy()
    );

    let sei = SeiUserDataUnregistered {
        uuid: &RAIDWIDE_UUID,
        payload: b"SEI RAIDWIDE",
    };

    pkt.size += sei.encoded_size();
    // On linux the encoder_packet has a `ref_count: long` value allocated in
    // front of pkt.
    // TODO: check why this is not the case on windows.
    pkt.data = unsafe {
        obs_sys::brealloc(
            pkt.data.byte_sub(LINUX_REF_COUNT_OFFSFET) as *mut _,
            pkt.size + LINUX_REF_COUNT_OFFSFET,
        )
        .byte_add(LINUX_REF_COUNT_OFFSFET) as *mut _
    };
    if pkt.data.is_null() {
        obs_log!(obs_sys::LOG_ERROR, "Cannot allocate space for SEI data");
        pkt.size -= sei.encoded_size();
        return;
    }

    let data = unsafe { slice::from_raw_parts_mut(pkt.data, pkt.size) };
    sei.encode(&mut data[pkt.size - sei.encoded_size()..])
        .unwrap();

    obs_log!(
        obs_sys::LOG_DEBUG,
        "Appended {} bytes of SEI data",
        sei.encoded_size()
    );
}
