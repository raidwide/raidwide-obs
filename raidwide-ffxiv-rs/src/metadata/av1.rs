use std::io::{self, Write};

use super::{UUID_SIZE, UserData};

const METADATA_TYPE: u8 = 6;
const VERSION: u8 = 1;
const METADATA_OVERHEAD: usize = 1 + UUID_SIZE + 1 + 1;

fn leb128_size(value: u32) -> usize {
    (u32::BITS - value.leading_zeros()).max(1).div_ceil(7) as usize
}

fn write_leb128(mut value: u32, out: &mut [u8]) -> usize {
    let mut offset = 0;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out[offset] = byte | if value != 0 { 0x80 } else { 0 };
        offset += 1;
        if value == 0 {
            return offset;
        }
    }
}

fn read_leb128(packet: &[u8], offset: &mut usize) -> usize {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = packet[*offset];
        *offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value as usize;
        }
        shift += 7;
    }
}

impl UserData<'_, '_> {
    pub fn encoded_av1_size(&self) -> usize {
        let size = METADATA_OVERHEAD + self.payload.len();
        1 + leb128_size(size as u32) + size
    }

    /// Private metadata: UUID, version 1, opaque payload, trailing bits.
    pub fn encode_av1(&self, out: &mut [u8]) -> io::Result<usize> {
        let encoded_size = self.encoded_av1_size();
        if out.len() < encoded_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output buffer is too small",
            ));
        }

        let size = METADATA_OVERHEAD + self.payload.len();
        let mut out = &mut out[..encoded_size];
        out.write_all(&[0x2a])?; // Metadata OBU, explicit size.
        let size_bytes = write_leb128(size as u32, out);
        out = &mut out[size_bytes..];
        out.write_all(&[METADATA_TYPE])?;
        out.write_all(self.uuid)?;
        out.write_all(&[VERSION])?;
        out.write_all(self.payload)?;
        out.write_all(&[0x80])?;
        Ok(encoded_size)
    }
}

/// Insert before the first frame header (3) or frame (6), after prefix OBUs.
/// Assumes valid, explicitly sized, single-layer low-overhead OBUs from OBS.
pub fn av1_insertion_offset(packet: &[u8]) -> usize {
    let mut offset = 0;
    while offset < packet.len() {
        let header = packet[offset];
        let obu_type = (header >> 3) & 0x0f;
        if matches!(obu_type, 3 | 6) {
            return offset;
        }
        offset += 1 + usize::from(header & 0x04 != 0); // Header and optional extension.
        let size = read_leb128(packet, &mut offset);
        offset += size;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: [u8; UUID_SIZE] = [0x42; UUID_SIZE];

    fn encode(payload: &[u8]) -> Vec<u8> {
        let data = UserData::new(&UUID, payload);
        let mut out = vec![0; data.encoded_av1_size()];
        assert_eq!(data.encode_av1(&mut out).unwrap(), out.len());
        out
    }

    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0; 1 + leb128_size(payload.len() as u32) + payload.len()];
        out[0] = (kind << 3) | 2;
        let offset = 1 + write_leb128(payload.len() as u32, &mut out[1..]);
        out[offset..].copy_from_slice(payload);
        out
    }

    #[test]
    fn encodes_empty_and_zero_containing_payloads() {
        for payload in [b"".as_slice(), &[0, 0, 1, 0, 0x80, 0xff]] {
            let encoded = encode(payload);
            let mut expected = vec![0x2a, (METADATA_OVERHEAD + payload.len()) as u8, 6];
            expected.extend_from_slice(&UUID);
            expected.push(1);
            expected.extend_from_slice(payload);
            expected.push(0x80);
            assert_eq!(encoded, expected);
            assert_eq!(av1_insertion_offset(&encoded), encoded.len());
        }
    }

    #[test]
    fn rejects_small_output_buffers() {
        let data = UserData::new(&UUID, b"{}");
        for len in [0, data.encoded_av1_size() - 1] {
            let mut out = vec![0xff; len];
            let error = data.encode_av1(&mut out).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(out, vec![0xff; len]);
        }
    }

    #[test]
    fn encodes_metadata_at_leb128_boundaries() {
        for size in [127usize, 128, 129, 16383, 16384, 16385] {
            let encoded = encode(&vec![0; size - METADATA_OVERHEAD]);
            let mut offset = 1;
            assert_eq!(read_leb128(&encoded, &mut offset), size);
            assert_eq!(encoded.len() - offset, size);
            assert_eq!(encoded[offset], METADATA_TYPE);
            assert_eq!(encoded.last(), Some(&0x80));
        }
    }

    #[test]
    fn leb128_round_trips() {
        for value in [
            0,
            1,
            127,
            128,
            16383,
            16384,
            (1 << 28) - 1,
            1 << 28,
            u32::MAX,
        ] {
            let mut encoded = vec![0; leb128_size(value)];
            assert_eq!(write_leb128(value, &mut encoded), encoded.len());
            let mut offset = 0;
            assert_eq!(read_leb128(&encoded, &mut offset), value as usize);
            assert_eq!(offset, encoded.len());
        }
        assert_eq!(
            read_leb128(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0], &mut 0),
            0
        );
    }

    #[test]
    fn inserts_after_prefix_and_before_first_frame() {
        let prefix = [obu(2, &[]), obu(1, &[1]), obu(5, &[6, 0x80]), obu(15, &[0])].concat();
        for kind in [3, 6] {
            let packet = [prefix.clone(), obu(kind, &[1]), obu(4, &[1])].concat();
            assert_eq!(av1_insertion_offset(&packet), prefix.len());
            assert_eq!(av1_insertion_offset(&obu(kind, &[1])), 0);
        }
        assert_eq!(av1_insertion_offset(&prefix), prefix.len());
        assert_eq!(av1_insertion_offset(&[]), 0);
    }

    #[test]
    fn skips_extensions_and_nonminimal_sizes() {
        let packet = [0x0e, 0, 0x81, 0, 0x80, 0x36, 0, 1, 0x80];
        assert_eq!(av1_insertion_offset(&packet), 5);
    }
}
