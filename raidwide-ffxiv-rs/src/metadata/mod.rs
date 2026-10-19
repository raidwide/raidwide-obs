//! Codec-independent application data carried in compressed video packets.
//!
//! H.264/HEVC use unregistered user-data SEI NALs; AV1 uses a private metadata
//! OBU. The application payload and UUID are shared, not the codec framing.

use std::io;

mod av1;
mod h264;
mod h265;
mod sei;

pub const UUID_SIZE: usize = 16;

pub const RAIDWIDE_UUID: [u8; UUID_SIZE] = [
    0x72, 0x61, 0x69, 0x64, 0x77, 0x69, 0x64, 0x65, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14, 0xFF, 0x14,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn from_obs_name(name: &[u8]) -> Option<Self> {
        match name {
            b"h264" => Some(Self::H264),
            b"hevc" => Some(Self::Hevc),
            b"av1" => Some(Self::Av1),
            _ => None,
        }
    }

    pub fn insertion_offset(self, packet: &[u8]) -> usize {
        match self {
            Self::H264 => h264::h264_insertion_offset(packet),
            Self::Hevc => h265::h265_insertion_offset(packet),
            Self::Av1 => av1::av1_insertion_offset(packet),
        }
    }
}

pub struct UserData<'u, 'p> {
    uuid: &'u [u8; UUID_SIZE],
    payload: &'p [u8],
}

impl<'u, 'p> UserData<'u, 'p> {
    pub fn new(uuid: &'u [u8; UUID_SIZE], payload: &'p [u8]) -> Self {
        Self { uuid, payload }
    }

    pub fn encoded_size(&self, codec: Codec) -> usize {
        match codec {
            Codec::H264 => self.encoded_h264_size(),
            Codec::Hevc => self.encoded_h265_size(),
            Codec::Av1 => self.encoded_av1_size(),
        }
    }

    pub fn encode(&self, codec: Codec, out: &mut [u8]) -> io::Result<usize> {
        match codec {
            Codec::H264 => self.encode_h264(out),
            Codec::Hevc => self.encode_h265(out),
            Codec::Av1 => self.encode_av1(out),
        }
    }
}

/// Size of a video packet after adding the Raidwide metadata envelope.
pub fn packet_size(codec: Codec, packet: &[u8], payload: &[u8]) -> usize {
    packet.len() + UserData::new(&RAIDWIDE_UUID, payload).encoded_size(codec)
}

/// Copy a packet into `out`, inserting metadata before its first coded frame.
/// Only the returned number of bytes is written; extra output space is untouched.
pub fn splice_packet(
    codec: Codec,
    packet: &[u8],
    payload: &[u8],
    out: &mut [u8],
) -> io::Result<usize> {
    let user_data = UserData::new(&RAIDWIDE_UUID, payload);
    let metadata_size = user_data.encoded_size(codec);
    let size = packet.len() + metadata_size;
    if out.len() < size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output buffer is too small",
        ));
    }
    let offset = codec.insertion_offset(packet);
    let (before, rest) = out[..size].split_at_mut(offset);
    let (metadata, after) = rest.split_at_mut(metadata_size);
    user_data.encode(codec, metadata)?;
    before.copy_from_slice(&packet[..offset]);
    after.copy_from_slice(&packet[offset..]);
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_obs_codec_names() {
        assert_eq!(Codec::from_obs_name(b"h264"), Some(Codec::H264));
        assert_eq!(Codec::from_obs_name(b"hevc"), Some(Codec::Hevc));
        assert_eq!(Codec::from_obs_name(b"av1"), Some(Codec::Av1));
        assert_eq!(Codec::from_obs_name(b"vp9"), None);
    }

    #[test]
    fn dispatches_encoding_without_changing_existing_seis() {
        let data = UserData::new(&RAIDWIDE_UUID, b"{}");
        for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
            let mut expected = vec![
                0;
                match codec {
                    Codec::H264 => data.encoded_h264_size(),
                    Codec::Hevc => data.encoded_h265_size(),
                    Codec::Av1 => data.encoded_av1_size(),
                }
            ];
            match codec {
                Codec::H264 => data.encode_h264(&mut expected).unwrap(),
                Codec::Hevc => data.encode_h265(&mut expected).unwrap(),
                Codec::Av1 => data.encode_av1(&mut expected).unwrap(),
            };
            let mut out = vec![0; data.encoded_size(codec)];
            assert_eq!(data.encode(codec, &mut out).unwrap(), out.len());
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn inserts_metadata_without_modifying_original_packet_bytes() {
        for (codec, prefix, frame) in [
            (
                Codec::H264,
                &[0, 0, 0, 1, 9, 0xf0][..],
                &[0, 0, 1, 0x65, 0x80][..],
            ),
            (
                Codec::Hevc,
                &[0, 0, 0, 1, 0x46, 1, 0x50][..],
                &[0, 0, 1, 0x26, 1, 0x80][..],
            ),
            (
                Codec::Av1,
                &[0x12, 0, 0x0a, 1, 0x80][..],
                &[0x32, 1, 0x80][..],
            ),
        ] {
            for prefix in [prefix, &[][..]] {
                let packet = [prefix, frame].concat();
                let offset = prefix.len();
                let size = packet_size(codec, &packet, b"{}");
                let mut modified = vec![0xa5; size + 4];
                assert_eq!(
                    splice_packet(codec, &packet, b"{}", &mut modified).unwrap(),
                    size
                );
                assert_eq!(&modified[size..], &[0xa5; 4]);
                modified.truncate(size);
                let metadata_size = size - packet.len();
                assert_eq!(&modified[..offset], prefix);
                assert_eq!(&modified[offset + metadata_size..], frame);
                assert_eq!(codec.insertion_offset(&modified), offset + metadata_size);
                let mut metadata = vec![0; metadata_size];
                UserData::new(&RAIDWIDE_UUID, b"{}")
                    .encode(codec, &mut metadata)
                    .unwrap();
                assert_eq!(&modified[offset..offset + metadata_size], metadata);

                let mut too_small = vec![0xa5; size - 1];
                assert_eq!(
                    splice_packet(codec, &packet, b"{}", &mut too_small)
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::InvalidInput
                );
                assert!(too_small.iter().all(|&byte| byte == 0xa5));
            }
        }
    }
}
