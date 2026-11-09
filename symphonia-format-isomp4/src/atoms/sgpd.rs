// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::atoms::limits::*;
use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result, decode_error};

/// The grouping type of the roll recovery sample group (ISO/IEC 14496-12, §10.1).
pub const ROLL_GROUPING_TYPE: [u8; 4] = *b"roll";

/// Sample group description atom.
///
/// Only the roll recovery group (`roll`) is read. For any other grouping type, only the grouping
/// type is read.
#[derive(Debug)]
pub struct SgpdAtom {
    /// The grouping type.
    pub grouping_type: [u8; 4],
    /// The 1-based index of the group description entry of the samples that no sample to group atom
    /// maps (version 2 and later), or 0 if there is none. A sample mapped to index 0 is in no group.
    pub default_index: u32,
    /// For the roll recovery group, the roll distance of each entry: the number of samples before
    /// a sample (if negative) that must be decoded for the sample to be decoded correctly.
    pub roll_distances: Vec<i16>,
}

impl Atom for SgpdAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let (version, _) = it.read_extended_header()?;

        let grouping_type = it.read_quad_bytes()?;

        let mut atom = SgpdAtom { grouping_type, default_index: 0, roll_distances: Vec::new() };

        if grouping_type != ROLL_GROUPING_TYPE {
            return Ok(atom);
        }

        let default_length = if version >= 1 { it.read_u32()? } else { 0 };

        if version >= 2 {
            atom.default_index = it.read_u32()?;
        }

        let entry_count = it.read_u32()?;

        // Limit the maximum initial capacity to prevent malicious files from using all the
        // available memory.
        atom.roll_distances.reserve(MAX_TABLE_INITIAL_CAPACITY.min(entry_count as usize));

        for _ in 0..entry_count {
            // From version 1, an atom without a default entry length states the length of each
            // entry. A version 0 atom does neither, and a roll recovery entry is 2 bytes.
            let length = match version {
                0 => 2,
                _ if default_length == 0 => it.read_u32()?,
                _ => default_length,
            };

            if length < 2 {
                return decode_error("isomp4 (sgpd): roll recovery entry too short");
            }

            atom.roll_distances.push(it.read_i16()?);

            it.ignore_bytes(u64::from(length - 2))?;
        }

        Ok(atom)
    }
}
