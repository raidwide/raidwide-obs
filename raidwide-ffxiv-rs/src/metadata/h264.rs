use std::io;

use super::{UserData, sei};

const NAL_HEADER: [u8; 1] = [6]; // SEI

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
    sei::insertion_offset(access_unit, NAL_HEADER.len(), |nal| {
        NAL_TYPES_SLICE.contains(&(nal[0] & 0x1F))
    })
}

impl UserData<'_, '_> {
    pub fn encoded_h264_size(&self) -> usize {
        self.encoded_sei_size(&NAL_HEADER)
    }

    pub fn encode_h264(&self, out: &mut [u8]) -> io::Result<usize> {
        self.encode_sei(&NAL_HEADER, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{RAIDWIDE_UUID, UUID_SIZE};

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
        let sei = UserData::new(&RAIDWIDE_UUID, payload);
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
        let sei = UserData::new(&RAIDWIDE_UUID, b"{}");
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
