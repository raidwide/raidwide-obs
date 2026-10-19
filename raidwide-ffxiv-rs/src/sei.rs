use std::io;
use std::io::Write;

pub const UUID_SIZE: usize = 16;

pub const RAIDWIDE_UUID: [u8; UUID_SIZE] = [
    0x72, 0x61, 0x69, 0x64, 0x77, 0x69, 0x64, 0x65, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14,
];

/// NAL unit types of the slices, the access unit's VCL NALs: non-IDR (1) to
/// IDR (5)
const NAL_TYPES_SLICE: std::ops::RangeInclusive<u8> = 1..=5;

/// Where to insert an SEI NAL in an H.264 Annex B access unit: before its
/// first slice, after the access unit delimiter and the parameter sets that
/// precede it. A parser can only read an SEI once it knows the SPS: in an
/// MPEG-TS stream (SRT, RIST, ...) the first access unit is the only one with
/// them, which OBS' encoders repeat in-band, and an SEI ahead of them is
/// dropped (GStreamer's h264parse does). At the end of an access unit without
/// slice, at its start if it isn't Annex B
pub fn h264_insertion_offset(access_unit: &[u8]) -> usize {
    let mut start = match find_start_code(access_unit, 0) {
        Some(start) => start,
        None => return 0,
    };
    loop {
        let header = start + 3;
        if access_unit
            .get(header)
            .is_some_and(|nal| NAL_TYPES_SLICE.contains(&(nal & 0x1F)))
        {
            // Including the leading zero of a 4 bytes start code. The previous
            // NAL's last byte can't be zero, it ends with its stop bit
            return if start > 0 && access_unit[start - 1] == 0 {
                start - 1
            } else {
                start
            };
        }
        match find_start_code(access_unit, header) {
            Some(next) => start = next,
            None => return access_unit.len(),
        }
    }
}

/// Index of the first `00 00 01` from `from`
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .position(|window| window == [0, 0, 1])
        .map(|position| from + position)
}

pub struct SeiUserDataUnregistered<'u, 'p> {
    uuid: &'u [u8; UUID_SIZE],
    payload: &'p [u8],
}

impl<'u, 'p> SeiUserDataUnregistered<'u, 'p> {
    const START_CODE: [u8; 4] = [0, 0, 0, 1];
    const NAL_HEADER: [u8; 1] = [6]; // SEI = 6
    const PAYLOAD_TYPE: [u8; 1] = [5]; // Unregistered User Data = 5
    const NAL_TRAILER: [u8; 1] = [0x80];

    pub fn new(uuid: &'u [u8; UUID_SIZE], payload: &'p [u8]) -> Self {
        Self { uuid, payload }
    }

    fn sei_size(&self) -> usize {
        self.uuid.len() + self.payload.len()
    }

    pub fn encoded_h264_size(&self) -> usize {
        Self::START_CODE.len()
            + Self::NAL_HEADER.len()
            + Self::PAYLOAD_TYPE.len()
            + (1 + self.sei_size().div_euclid(0xFF)) // size of varint(sei_size)
            + self.sei_size()
            + Self::NAL_TRAILER.len()
    }

    pub fn encode_h264(&self, out: &mut [u8]) -> io::Result<usize> {
        if out.len() < self.encoded_h264_size() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The SEI NAL as `raidwide/pipeline/h264.py` parses it (and
    /// `tests/session.py` builds it)
    fn expected_sei(payload: &[u8]) -> Vec<u8> {
        let size = UUID_SIZE + payload.len();
        let mut nal = vec![0, 0, 0, 1, 6, 5];
        nal.extend(std::iter::repeat_n(0xFF, size / 255));
        nal.push((size % 255) as u8);
        nal.extend_from_slice(&RAIDWIDE_UUID);
        nal.extend_from_slice(payload);
        nal.push(0x80);
        nal
    }

    fn encode(payload: &[u8]) -> Vec<u8> {
        let sei = SeiUserDataUnregistered::new(&RAIDWIDE_UUID, payload);
        let mut out = vec![0; sei.encoded_h264_size()];
        assert_eq!(sei.encode_h264(&mut out).unwrap(), out.len());
        out
    }

    #[test]
    fn encodes_small_payloads() {
        assert_eq!(encode(b"{}"), expected_sei(b"{}"));
    }

    #[test]
    fn encodes_payload_sizes_around_255() {
        // SEI sizes of 254, 255 (0xFF 0x00) and 300 (0xFF 45)
        for len in [254 - UUID_SIZE, 255 - UUID_SIZE, 300 - UUID_SIZE] {
            let payload = vec![b'a'; len];
            assert_eq!(encode(&payload), expected_sei(&payload));
        }
    }

    #[test]
    fn rejects_small_output_buffers() {
        let sei = SeiUserDataUnregistered::new(&RAIDWIDE_UUID, b"{}");
        let mut out = vec![0; sei.encoded_h264_size() - 1];
        assert!(sei.encode_h264(&mut out).is_err());
    }

    #[test]
    fn inserts_before_the_first_slice() {
        // AUD then IDR slice, 4 bytes start codes
        assert_eq!(
            h264_insertion_offset(&[0, 0, 0, 1, 9, 0xF0, 0, 0, 0, 1, 0x65, 0x88]),
            6
        );
        // AUD then non-IDR slice, 3 bytes start codes
        assert_eq!(
            h264_insertion_offset(&[0, 0, 1, 9, 0x10, 0, 0, 1, 0x41, 0x9A]),
            5
        );
        // Slice first
        assert_eq!(h264_insertion_offset(&[0, 0, 1, 0x41, 0x9A]), 0);
        assert_eq!(h264_insertion_offset(&[0, 0, 0, 1, 0x65, 0x88]), 0);
    }

    #[test]
    fn inserts_after_the_parameter_sets() {
        // AUD, SPS, PPS, the encoder's SEI then IDR slice, as OBS' first
        // access unit of an MPEG-TS stream
        let access_unit = [
            0, 0, 0, 1, 9, 0x10, // AUD
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x1F, // SPS
            0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80, // PPS
            0, 0, 1, 0x06, 0x05, 0x01, 0x80, // SEI
            0, 0, 0, 1, 0x65, 0x88, 0x84, // IDR slice
        ];
        assert_eq!(h264_insertion_offset(&access_unit), 29);
        // Without AUD, 3 bytes start code before the slice
        assert_eq!(
            h264_insertion_offset(&[0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x65]),
            6
        );
    }

    #[test]
    fn inserts_at_the_end_without_slice() {
        assert_eq!(h264_insertion_offset(&[0, 0, 0, 1, 9, 0xF0]), 6);
        assert_eq!(h264_insertion_offset(&[0, 0, 0, 1]), 4);
    }

    #[test]
    fn inserts_at_the_start_without_start_code() {
        assert_eq!(h264_insertion_offset(&[0x65, 0x88]), 0);
        assert_eq!(h264_insertion_offset(&[]), 0);
    }
}
