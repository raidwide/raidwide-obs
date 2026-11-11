//! Regression readers deliberately do not use the production framing helpers.
//! Fixtures are real compressed access units; no encoder is needed at test time.

use raidwide_ffxiv_rs::metadata::{Codec, RAIDWIDE_UUID, UserData, packet_size, splice_packet};

const H264: &[u8] = include_bytes!("fixtures/h264-keyframe.h264");
const HEVC: &[u8] = include_bytes!("fixtures/hevc-keyframe.h265");
const AV1: &[u8] = include_bytes!("fixtures/av1-keyframe.obu");
const JSON: &[u8] = br#"{"version":1,"event":"CombatData","data":"fixture"}"#;

#[derive(Debug)]
struct Unit<'a> {
    start: usize,
    kind: u8,
    bytes: &'a [u8],
    body: &'a [u8],
    size_bytes: &'a [u8],
}

fn annex_b(codec: Codec, packet: &[u8]) -> Vec<Unit<'_>> {
    let mut starts = Vec::new();
    let mut pos = 0;
    while pos + 3 <= packet.len() {
        let width = if packet[pos..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if packet[pos..].starts_with(&[0, 0, 1]) {
            3
        } else {
            pos += 1;
            continue;
        };
        starts.push((pos, width));
        pos += width;
    }
    assert_eq!(starts.first().map(|entry| entry.0), Some(0));
    starts
        .iter()
        .enumerate()
        .map(|(index, &(start, width))| {
            let end = starts.get(index + 1).map_or(packet.len(), |entry| entry.0);
            let header = &packet[start + width..end];
            assert_eq!(header[0] & 0x80, 0, "forbidden_zero_bit");
            let (kind, header_size) = match codec {
                Codec::H264 => (header[0] & 31, 1),
                Codec::Hevc => {
                    assert!(header.len() >= 2);
                    assert_ne!(header[1] & 7, 0, "nuh_temporal_id_plus1");
                    ((header[0] >> 1) & 63, 2)
                }
                Codec::Av1 => panic!("AV1 does not use NAL units"),
            };
            Unit {
                start,
                kind,
                bytes: &packet[start..end],
                body: &header[header_size..],
                size_bytes: &[],
            }
        })
        .collect()
}

fn rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut zeros = 0;
    let mut pos = 0;
    while pos < ebsp.len() {
        let byte = ebsp[pos];
        if zeros == 2 {
            assert!(byte >= 3, "unescaped start-code-like sequence in EBSP");
            if byte == 3 {
                assert!(ebsp.get(pos + 1).is_some_and(|&next| next <= 3));
                zeros = 0;
                pos += 1;
                continue;
            }
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        pos += 1;
    }
    out
}

fn sei_number(bytes: &[u8], pos: &mut usize) -> usize {
    let mut result = 0;
    loop {
        let byte = bytes[*pos];
        *pos += 1;
        result += usize::from(byte);
        if byte != 255 {
            return result;
        }
    }
}

fn unregistered_data(unit: &Unit<'_>) -> Vec<u8> {
    assert!(matches!(unit.kind, 6 | 39), "H.264 or HEVC prefix SEI");
    let bytes = rbsp(unit.body);
    let mut pos = 0;
    assert_eq!(sei_number(&bytes, &mut pos), 5);
    let size = sei_number(&bytes, &mut pos);
    let data = bytes[pos..pos + size].to_vec();
    assert_eq!(&bytes[pos + size..], &[0x80], "rbsp_trailing_bits");
    data
}

fn leb128(bytes: &[u8], pos: &mut usize) -> usize {
    let mut value = 0u64;
    for shift in (0..56).step_by(7) {
        let byte = bytes[*pos];
        *pos += 1;
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return usize::try_from(value).unwrap();
        }
    }
    panic!("OBU length exceeds eight LEB128 bytes");
}

fn obus(packet: &[u8]) -> Vec<Unit<'_>> {
    let mut units = Vec::new();
    let mut pos = 0;
    while pos < packet.len() {
        let start = pos;
        let header = packet[pos];
        pos += 1;
        assert_eq!(header & 0x81, 0, "forbidden/reserved OBU bits");
        assert_ne!(header & 2, 0, "low-overhead OBU must carry its size");
        if header & 4 != 0 {
            assert_eq!(packet[pos] & 7, 0, "extension reserved bits");
            pos += 1;
        }
        let size_start = pos;
        let size = leb128(packet, &mut pos);
        let body_start = pos;
        pos += size;
        units.push(Unit {
            start,
            kind: (header >> 3) & 15,
            bytes: &packet[start..pos],
            body: &packet[body_start..pos],
            size_bytes: &packet[size_start..body_start],
        });
    }
    assert_eq!(pos, packet.len());
    units
}

fn av1_data<'a>(unit: &Unit<'a>) -> &'a [u8] {
    assert_eq!(unit.kind, 5, "metadata OBU");
    let mut pos = 0;
    assert_eq!(leb128(unit.body, &mut pos), 6, "private metadata type");
    let data = &unit.body[pos..];
    assert_eq!(&data[..16], &RAIDWIDE_UUID);
    assert_eq!(data[16], 1, "private metadata version");
    assert_eq!(data.last(), Some(&0x80), "OBU trailing bits");
    &data[17..data.len() - 1]
}

fn splice(codec: Codec, packet: &[u8], payload: &[u8], offset: usize) -> Vec<u8> {
    let size = packet_size(codec, packet, payload);
    let mut out = vec![0xa5; size + 8];
    assert_eq!(
        splice_packet(codec, packet, payload, &mut out).unwrap(),
        size
    );
    assert_eq!(
        &out[size..],
        &[0xa5; 8],
        "must not overwrite spare capacity"
    );
    out.truncate(size);
    let inserted = size - packet.len();
    assert_eq!(
        inserted,
        UserData::new(&RAIDWIDE_UUID, payload).encoded_size(codec)
    );
    assert_eq!(&out[..offset], &packet[..offset]);
    assert_eq!(&out[offset + inserted..], &packet[offset..]);
    out
}

fn json_of_length(length: usize) -> Vec<u8> {
    let prefix = br#"{"version":1,"data":""#;
    let mut json = prefix.to_vec();
    json.resize(length - 2, b'x');
    json.extend_from_slice(b"\"}");
    assert_eq!(json.len(), length);
    json
}

#[test]
fn real_h264_keyframe_splice_preserves_parameter_sets_sei_and_frame() {
    let original = annex_b(Codec::H264, H264);
    assert_eq!(
        original.iter().map(|unit| unit.kind).collect::<Vec<_>>(),
        [9, 7, 8, 6, 5]
    );
    assert!(!unregistered_data(&original[3]).starts_with(&RAIDWIDE_UUID));
    let out = splice(Codec::H264, H264, JSON, original[4].start);
    let units = annex_b(Codec::H264, &out);
    assert_eq!(
        units.iter().map(|unit| unit.kind).collect::<Vec<_>>(),
        [9, 7, 8, 6, 6, 5]
    );
    for index in 0..4 {
        assert_eq!(units[index].bytes, original[index].bytes);
    }
    assert_eq!(units[5].bytes, original[4].bytes);
    let recovered = unregistered_data(&units[4]);
    assert_eq!(&recovered[..16], &RAIDWIDE_UUID);
    assert_eq!(&recovered[16..], JSON);
}

#[test]
fn real_hevc_keyframe_splice_preserves_parameter_sets_sei_and_frame() {
    let original = annex_b(Codec::Hevc, HEVC);
    assert_eq!(
        original.iter().map(|unit| unit.kind).collect::<Vec<_>>(),
        [35, 32, 33, 34, 39, 20]
    );
    assert!(!unregistered_data(&original[4]).starts_with(&RAIDWIDE_UUID));
    let out = splice(Codec::Hevc, HEVC, JSON, original[5].start);
    let units = annex_b(Codec::Hevc, &out);
    assert_eq!(
        units.iter().map(|unit| unit.kind).collect::<Vec<_>>(),
        [35, 32, 33, 34, 39, 39, 20]
    );
    for index in 0..5 {
        assert_eq!(units[index].bytes, original[index].bytes);
    }
    assert_eq!(units[6].bytes, original[5].bytes);
    let recovered = unregistered_data(&units[5]);
    assert_eq!(&recovered[..16], &RAIDWIDE_UUID);
    assert_eq!(&recovered[16..], JSON);
    assert_eq!(
        std::str::from_utf8(&recovered[16..]).unwrap(),
        r#"{"version":1,"event":"CombatData","data":"fixture"}"#
    );
}

#[test]
fn real_av1_keyframe_splice_preserves_delimiter_sequence_and_frame() {
    let original = obus(AV1);
    assert_eq!(
        original.iter().map(|unit| unit.kind).collect::<Vec<_>>(),
        [2, 1, 6]
    );
    assert!(original[0].body.is_empty());
    // The real compressed frame itself already requires a multibyte OBU size.
    assert_eq!(original[2].size_bytes, [0xa6, 0x01]);
    let out = splice(Codec::Av1, AV1, JSON, original[2].start);
    let units = obus(&out);
    assert_eq!(
        units.iter().map(|unit| unit.kind).collect::<Vec<_>>(),
        [2, 1, 5, 6]
    );
    assert_eq!(units[0].bytes, original[0].bytes);
    assert_eq!(units[1].bytes, original[1].bytes);
    assert_eq!(units[3].bytes, original[2].bytes);
    assert_eq!(av1_data(&units[2]), JSON);
}

#[test]
fn real_h264_splice_crosses_exact_sei_length_extensions() {
    check_sei_length_extensions(Codec::H264, H264, 4);
}

#[test]
fn real_hevc_splice_crosses_exact_sei_length_extensions() {
    check_sei_length_extensions(Codec::Hevc, HEVC, 5);
}

fn check_sei_length_extensions(codec: Codec, packet: &[u8], sei_index: usize) {
    let offset = annex_b(codec, packet).last().unwrap().start;
    for (size, length_bytes) in [
        (254, &[0xfe][..]),
        (255, &[0xff, 0][..]),
        (256, &[0xff, 1][..]),
        (509, &[0xff, 0xfe][..]),
        (510, &[0xff, 0xff, 0][..]),
        (511, &[0xff, 0xff, 1][..]),
    ] {
        let json = json_of_length(size - 16);
        let out = splice(codec, packet, &json, offset);
        let units = annex_b(codec, &out);
        let sei = &units[sei_index];
        assert_eq!(&sei.body[1..1 + length_bytes.len()], length_bytes);
        let data = unregistered_data(sei);
        assert_eq!(data.len(), size);
        assert_eq!(&data[..16], &RAIDWIDE_UUID);
        assert_eq!(&data[16..], json);
    }
}

#[test]
fn real_av1_splice_crosses_exact_obu_leb128_boundaries() {
    let offset = obus(AV1).last().unwrap().start;
    for (size, length_bytes) in [
        (127, &[0x7f][..]),
        (128, &[0x80, 1][..]),
        (129, &[0x81, 1][..]),
        (16383, &[0xff, 0x7f][..]),
        (16384, &[0x80, 0x80, 1][..]),
    ] {
        // metadata_type + UUID + version + JSON + trailing_bits.
        let json = json_of_length(size - 19);
        let out = splice(Codec::Av1, AV1, &json, offset);
        let units = obus(&out);
        assert_eq!(units[2].size_bytes, length_bytes);
        assert_eq!(units[2].body.len(), size);
        assert_eq!(av1_data(&units[2]), json);
    }
}

#[test]
fn real_packets_reject_an_output_buffer_one_byte_short() {
    for (codec, packet) in [(Codec::H264, H264), (Codec::Hevc, HEVC), (Codec::Av1, AV1)] {
        let mut out = vec![0; packet_size(codec, packet, JSON) - 1];
        let error = splice_packet(codec, packet, JSON, &mut out).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
