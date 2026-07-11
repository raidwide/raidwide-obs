use std::io;
use std::io::Write;

pub const UUID_SIZE: usize = 16;

pub const RAIDWIDE_UUID: [u8; UUID_SIZE] = [
    0x72, 0x61, 0x69, 0x64, 0x77, 0x69, 0x64, 0x65, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14,
];

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
