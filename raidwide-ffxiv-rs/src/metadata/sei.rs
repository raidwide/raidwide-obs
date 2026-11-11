//! Shared Annex B scanning and unregistered user-data SEI framing.

use std::io::{self, Write};

use super::UserData;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const PAYLOAD_TYPE: u8 = 5;
const TRAILING_BITS: u8 = 0x80;

pub(super) fn insertion_offset(
    access_unit: &[u8],
    header_size: usize,
    is_vcl: impl Fn(&[u8]) -> bool,
) -> usize {
    let mut start = match find_start_code(access_unit, 0) {
        Some(start) => start,
        None => return 0,
    };
    loop {
        let header = start + 3;
        if access_unit
            .get(header..header + header_size)
            .is_some_and(&is_vcl)
        {
            // Include the leading zero when this is a four-byte start code.
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

fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .position(|window| window == [0, 0, 1])
        .map(|position| from + position)
}

impl UserData<'_, '_> {
    fn sei_size(&self) -> usize {
        self.uuid.len() + self.payload.len()
    }

    pub(super) fn encoded_sei_size(&self, nal_header: &[u8]) -> usize {
        START_CODE.len() + nal_header.len() + 1 + (1 + self.sei_size() / 255) + self.sei_size() + 1
    }

    pub(super) fn encode_sei(&self, nal_header: &[u8], out: &mut [u8]) -> io::Result<usize> {
        let size = self.encoded_sei_size(nal_header);
        if out.len() < size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output buffer is too small",
            ));
        }
        let mut out = &mut out[..size];
        out.write_all(&START_CODE)?;
        out.write_all(nal_header)?;
        out.write_all(&[PAYLOAD_TYPE])?;
        for _ in 0..self.sei_size() / 255 {
            out.write_all(&[255])?;
        }
        out.write_all(&[(self.sei_size() % 255) as u8])?;
        out.write_all(self.uuid)?;
        out.write_all(self.payload)?;
        out.write_all(&[TRAILING_BITS])?;
        Ok(size)
    }
}
