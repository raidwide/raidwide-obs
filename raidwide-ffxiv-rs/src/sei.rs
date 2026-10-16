use std::io;
use std::io::Write;

pub const UUID_SIZE: usize = 16;

pub const RAIDWIDE_UUID: [u8; UUID_SIZE] = [
    0x72, 0x61, 0x69, 0x64, 0x77, 0x69, 0x64, 0x65, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14,
];

const NAL_TYPE_AUD: u8 = 9;

/// Where to insert an SEI NAL in an H.264 Annex B access unit: after its
/// access unit delimiter when it starts with one, since the AUD must be the
/// access unit's first NAL (parsers split access units on it), at its start
/// otherwise. OBS' encoders emit AUDs along with repeated headers, which OBS
/// enables for streams other than RTMP (SRT, RIST, WHIP, HLS)
pub fn h264_insertion_offset(access_unit: &[u8]) -> usize {
    // The first start code, 3 or 4 bytes long
    let header = match find_start_code(access_unit, 0) {
        Some(start @ (0 | 1)) => start + 3,
        _ => return 0,
    };
    if access_unit
        .get(header)
        .is_none_or(|nal| nal & 0x1F != NAL_TYPE_AUD)
    {
        return 0;
    }

    match find_start_code(access_unit, header + 1) {
        // Including the leading zero of a 4 bytes start code. The AUD's last
        // byte can't be zero, it ends with its stop bit
        Some(start) if access_unit[start - 1] == 0 => start - 1,
        Some(start) => start,
        None => access_unit.len(),
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
    fn inserts_after_the_access_unit_delimiter() {
        // 4 bytes start codes
        assert_eq!(
            h264_insertion_offset(&[0, 0, 0, 1, 9, 0xF0, 0, 0, 0, 1, 0x65, 0x88]),
            6
        );
        // 3 bytes start codes
        assert_eq!(
            h264_insertion_offset(&[0, 0, 1, 9, 0x10, 0, 0, 1, 0x41, 0x9A]),
            5
        );
        // Nothing after it
        assert_eq!(h264_insertion_offset(&[0, 0, 0, 1, 9, 0xF0]), 6);
    }

    #[test]
    fn inserts_at_the_start_without_access_unit_delimiter() {
        assert_eq!(
            h264_insertion_offset(&[0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x65]),
            0
        );
        assert_eq!(h264_insertion_offset(&[0, 0, 1, 0x41, 0x9A]), 0);
        assert_eq!(h264_insertion_offset(&[0, 0, 0, 1]), 0);
        assert_eq!(h264_insertion_offset(&[]), 0);
    }
}
