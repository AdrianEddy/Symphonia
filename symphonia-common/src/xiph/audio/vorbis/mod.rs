// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::audio::{Channels, layouts};
use symphonia_core::errors::{Result, decode_error};

/// Get the mapping 0 channel listing for the given number of channels.
///
/// Vorbis I, §4.3.9 "Output channel order" assigns every count from 1 to 8,
/// and each one is a layout Symphonia already names — as `CHANNEL_LAYOUT_OGG_*`,
/// which is this table. Quoting the assignments, in order: "the stream is
/// monophonic", "the stream is stereo. channel order: left, right", "the stream
/// is a 1d-surround encoding. channel order: left, center, right", "the stream
/// is quadraphonic surround. channel order: front left, front right, rear left,
/// rear right", "the stream is five-channel surround. channel order: front
/// left, center, front right, rear left, rear right", "the stream is 5.1
/// surround. channel order: front left, center, front right, rear left, rear
/// right, LFE", "the stream is 6.1 surround. channel order: front left, center,
/// front right, side left, side right, rear center, LFE", and "the stream is
/// 7.1 surround. channel order: front left, center, front right, side left,
/// side right, rear left, rear right, LFE".
pub fn vorbis_channels_to_channels(num_channels: u8) -> Option<Channels> {
    let channels = match num_channels {
        // "the stream is monophonic" — one Vorbis channel is the mono channel,
        // which is front CENTRE. It is not a front-left channel that happens to
        // be alone.
        1 => layouts::CHANNEL_LAYOUT_OGG_1P0,
        2 => layouts::CHANNEL_LAYOUT_OGG_2P0,
        3 => layouts::CHANNEL_LAYOUT_OGG_3P0,
        4 => layouts::CHANNEL_LAYOUT_OGG_4P0,
        5 => layouts::CHANNEL_LAYOUT_OGG_5P0,
        6 => layouts::CHANNEL_LAYOUT_OGG_5P1,
        7 => layouts::CHANNEL_LAYOUT_OGG_6P1,
        8 => layouts::CHANNEL_LAYOUT_OGG_7P1,
        _ => return None,
    };

    Some(channels)
}

// Xiph lacing for three packets starts with `2`, if it's not 2 assume the extradata is
// parseable as a raw Vorbis identification header (which must start with a `1`).
pub const XIPH_LACED_LEADING_HEADER: u8 = 2;

/// Unpack Vorbis extradata packed in the Xiph lacing format (used by WebM/Matroska).
///
/// If the data is not Xiph laced (does not start with 2), it is returned as is.
/// If it is Xiph laced, it extracts the Identification and Setup packets and returns
/// them concatenated.
pub fn unpack_xiph_laced_extradata(extradata: &[u8]) -> Result<(&[u8], &[u8])> {
    let Some((&header_count, lacing_data)) = extradata.split_first()
    else {
        return decode_error("vorbis: extradata is empty");
    };

    if header_count != XIPH_LACED_LEADING_HEADER {
        return decode_error("vorbis: invalid Xiph lacing count");
    }

    let mut iter = lacing_data.iter();
    let mut lengths = [0usize; 2]; // Xiph lacing for 3 packets encodes exactly 2 lengths
    for length in &mut lengths {
        loop {
            let &val = match iter.next() {
                Some(v) => v,
                None => return decode_error("vorbis: truncated length lacing"),
            };
            *length = match length.checked_add(val as usize) {
                Some(l) => l,
                None => return decode_error("vorbis: lacing length overflow"),
            };
            if val < 255 {
                break;
            }
        }
    }
    let [ident_len, comment_len] = lengths;

    let remaining_data = iter.as_slice();
    if remaining_data.is_empty() {
        return decode_error("vorbis: no data remains after reading lacing");
    }

    let id_and_comment_len = match ident_len.checked_add(comment_len) {
        Some(len) if len <= remaining_data.len() => len,
        _ => return decode_error("vorbis: header lengths exceed buffer size"),
    };

    let id_packet = &remaining_data[..ident_len];
    let setup_packet = &remaining_data[id_and_comment_len..];
    Ok((id_packet, setup_packet))
}

#[cfg(test)]
mod tests {
    use symphonia_core::audio::Position;

    use super::*;

    #[test]
    fn test_vorbis_channels_to_channels() {
        // "the stream is monophonic", and mono is front centre — NOT a lone
        // front-left channel, which is what a stereo pair's left half would be.
        assert_eq!(
            vorbis_channels_to_channels(1),
            Some(Channels::Positioned(Position::FRONT_CENTER))
        );
        assert_ne!(
            vorbis_channels_to_channels(1),
            Some(Channels::Positioned(Position::FRONT_LEFT))
        );

        // The remaining counts are unchanged; pin them so naming them by layout
        // cannot silently move one.
        #[rustfmt::skip]
        let expected: [(u8, Channels); 7] = [
            (2, layouts::CHANNEL_LAYOUT_OGG_2P0),
            (3, layouts::CHANNEL_LAYOUT_OGG_3P0),
            (4, layouts::CHANNEL_LAYOUT_OGG_4P0),
            (5, layouts::CHANNEL_LAYOUT_OGG_5P0),
            (6, layouts::CHANNEL_LAYOUT_OGG_5P1),
            (7, layouts::CHANNEL_LAYOUT_OGG_6P1),
            (8, layouts::CHANNEL_LAYOUT_OGG_7P1),
        ];
        for (count, channels) in expected {
            assert_eq!(channels.count(), usize::from(count), "{count} channels");
            assert_eq!(vorbis_channels_to_channels(count), Some(channels), "{count} channels");
        }

        // Mapping 0 defines 1 to 8 channels and nothing else.
        assert_eq!(vorbis_channels_to_channels(0), None);
        for count in 9..=u8::MAX {
            assert_eq!(vorbis_channels_to_channels(count), None, "{count} channels");
        }
    }

    #[test]
    fn test_unpack_xiph_laced_extradata_valid() {
        let id_packet = b"id_packet";
        let comment_packet = b"comment_packet";
        let setup_packet = b"setup_packet";

        let mut extradata = Vec::new();
        extradata.push(XIPH_LACED_LEADING_HEADER);
        extradata.push(id_packet.len() as u8);
        extradata.push(comment_packet.len() as u8);
        extradata.extend_from_slice(id_packet);
        extradata.extend_from_slice(comment_packet);
        extradata.extend_from_slice(setup_packet);

        let result = unpack_xiph_laced_extradata(&extradata).unwrap();

        assert_eq!(result, (id_packet.as_ref(), setup_packet.as_ref()));
    }

    #[test]
    fn test_unpack_xiph_laced_extradata_truncated() {
        let id_packet = b"id";
        let comment_packet = b"comment";

        let mut extradata = Vec::new();
        extradata.push(XIPH_LACED_LEADING_HEADER);
        extradata.push(id_packet.len() as u8);
        extradata.push(comment_packet.len() as u8);
        extradata.extend_from_slice(id_packet);

        let result = unpack_xiph_laced_extradata(&extradata);
        assert!(result.is_err());
    }

    #[test]
    fn test_unpack_xiph_laced_extradata_large_sizes() {
        let setup_packet = b"setup";

        let mut extradata = Vec::new();
        extradata.push(XIPH_LACED_LEADING_HEADER);
        extradata.push(255);
        extradata.push(0);
        extradata.push(0);
        extradata.extend_from_slice(&vec![0; 255]);
        extradata.extend_from_slice(setup_packet);

        let result = unpack_xiph_laced_extradata(&extradata).unwrap();

        let expected_id = vec![0; 255];
        assert_eq!(result, (expected_id.as_slice(), setup_packet.as_ref()));
    }

    #[test]
    fn test_unpack_xiph_laced_extradata_empty() {
        let result = unpack_xiph_laced_extradata(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_unpack_xiph_laced_extradata_invalid_count() {
        let extradata = vec![1, 30, 0]; // Raw vorbis identification header starts with 1
        let result = unpack_xiph_laced_extradata(&extradata);
        assert!(result.is_err());
    }

    #[test]
    fn test_unpack_xiph_laced_extradata_truncated_lacing() {
        // Lacing is incomplete (we have a 255 but no next byte to finish the size)
        let extradata = vec![XIPH_LACED_LEADING_HEADER, 255];
        let result = unpack_xiph_laced_extradata(&extradata);
        assert!(result.is_err());
    }

    #[test]
    fn test_unpack_xiph_laced_extradata_no_data_after_lacing() {
        // Lacing completes successfully (lengths 0 and 0) but no actual packet data follows
        let extradata = vec![XIPH_LACED_LEADING_HEADER, 0, 0];
        let result = unpack_xiph_laced_extradata(&extradata);
        assert!(result.is_err());
    }
}
