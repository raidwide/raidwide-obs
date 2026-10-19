use std::io;

use super::{UserData, sei};
// Prefix SEI (39), layer 0, temporal_id_plus1 = 1.
const NAL_HEADER: [u8; 2] = [0x4E, 0x01];

/// Insert a prefix SEI before the first VCL NAL, after any preceding AUD,
/// VPS, SPS, PPS and prefix SEIs in this Annex B access unit.
/// At the end without a VCL NAL, at the start without a start code.
pub fn h265_insertion_offset(access_unit: &[u8]) -> usize {
    sei::insertion_offset(access_unit, NAL_HEADER.len(), |nal| {
        ((nal[0] >> 1) & 0x3F) <= 31
    })
}

impl UserData<'_, '_> {
    pub fn encoded_h265_size(&self) -> usize {
        self.encoded_sei_size(&NAL_HEADER)
    }

    pub fn encode_h265(&self, out: &mut [u8]) -> io::Result<usize> {
        self.encode_sei(&NAL_HEADER, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{RAIDWIDE_UUID, UUID_SIZE};

    fn expected_sei(payload: &[u8]) -> Vec<u8> {
        let size = UUID_SIZE + payload.len();
        let mut nal = vec![0, 0, 0, 1, 0x4E, 0x01, 5];
        nal.extend(std::iter::repeat_n(0xFF, size / 255));
        nal.push((size % 255) as u8);
        nal.extend_from_slice(&RAIDWIDE_UUID);
        nal.extend_from_slice(payload);
        nal.push(0x80);
        nal
    }

    fn encode(payload: &[u8]) -> Vec<u8> {
        let sei = UserData::new(&RAIDWIDE_UUID, payload);
        let mut out = vec![0; sei.encoded_h265_size()];
        assert_eq!(sei.encode_h265(&mut out).unwrap(), out.len());
        out
    }

    #[test]
    fn encodes_small_payloads() {
        for payload in [b"".as_slice(), b"{}"] {
            assert_eq!(encode(payload), expected_sei(payload));
        }
    }

    #[test]
    fn encodes_payload_sizes_around_255() {
        for len in [
            254 - UUID_SIZE,
            255 - UUID_SIZE,
            256 - UUID_SIZE,
            510 - UUID_SIZE,
        ] {
            let payload = vec![b'a'; len];
            assert_eq!(encode(&payload), expected_sei(&payload));
        }
    }

    #[test]
    fn rejects_small_output_buffers() {
        let sei = UserData::new(&RAIDWIDE_UUID, b"{}");
        let mut out = vec![0; sei.encoded_h265_size() - 1];
        assert!(sei.encode_h265(&mut out).is_err());
    }

    #[test]
    fn inserts_before_all_vcl_types() {
        for nal_type in 0..=31 {
            // AUD followed by a VCL NAL, with both start code lengths.
            for start_code in [&[0, 0, 1][..], &[0, 0, 0, 1][..]] {
                let mut access_unit = vec![0, 0, 0, 1, 0x46, 0x01, 0x50];
                let offset = access_unit.len();
                access_unit.extend_from_slice(start_code);
                access_unit.extend_from_slice(&[nal_type << 1, 0x01, 0x80]);
                assert_eq!(h265_insertion_offset(&access_unit), offset);
                assert_eq!(h265_insertion_offset(&access_unit[offset..]), 0);
            }
        }
    }

    #[test]
    fn inserts_after_parameter_sets_and_existing_prefix_sei() {
        let mut access_unit = vec![
            0, 0, 0, 1, 0x46, 0x01, 0x50, // AUD
            0, 0, 1, 0x40, 0x01, 0x80, // VPS
            0, 0, 0, 1, 0x42, 0x01, 0x80, // SPS
            0, 0, 1, 0x44, 0x01, 0x80, // PPS
            0, 0, 1, 0x4E, 0x01, 0x80, // Prefix SEI
        ];
        let offset = access_unit.len();
        access_unit.extend_from_slice(&[
            0, 0, 0, 1, 0x26, 0x01, 0x80, // IDR
            0, 0, 1, 0x02, 0x01, 0x80, // Another slice
            0, 0, 1, 0x50, 0x01, 0x80, // Suffix SEI
        ]);
        assert_eq!(h265_insertion_offset(&access_unit), offset);
    }

    #[test]
    fn inserts_at_the_end_without_vcl() {
        for access_unit in [
            &[0, 0, 0, 1, 0x46, 0x01, 0x50][..],
            &[0, 0, 0, 1][..],
            &[0, 0, 1, 0x02][..], // Truncated two-byte header
        ] {
            assert_eq!(h265_insertion_offset(access_unit), access_unit.len());
        }
    }

    #[test]
    fn inserts_at_the_start_without_start_code() {
        assert_eq!(h265_insertion_offset(&[0x26, 0x01, 0x80]), 0);
        assert_eq!(h265_insertion_offset(&[]), 0);
    }
}
