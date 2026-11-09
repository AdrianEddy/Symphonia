// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::atoms::limits::*;
use crate::atoms::sgpd::ROLL_GROUPING_TYPE;
use crate::atoms::{Atom, AtomHeader, AtomIterator, ReadAtom, Result};

/// A run of samples belonging to one sample group.
#[derive(Debug)]
pub struct SampleToGroupEntry {
    /// The number of consecutive samples in the run.
    pub sample_count: u32,
    /// The 1-based index of the group description entry the samples belong to, or 0 if they
    /// belong to no group of this grouping type.
    pub group_description_index: u32,
}

/// Sample to group atom.
///
/// Only the roll recovery group (`roll`) is read. For any other grouping type, only the grouping
/// type is read.
#[derive(Debug)]
pub struct SbgpAtom {
    /// The grouping type.
    pub grouping_type: [u8; 4],
    /// The runs of samples, in sample order.
    pub entries: Vec<SampleToGroupEntry>,
}

impl Atom for SbgpAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let (version, _) = it.read_extended_header()?;

        let grouping_type = it.read_quad_bytes()?;

        if grouping_type != ROLL_GROUPING_TYPE {
            return Ok(SbgpAtom { grouping_type, entries: Vec::new() });
        }

        if version == 1 {
            // The grouping type parameter.
            let _ = it.read_u32()?;
        }

        let entry_count = it.read_u32()?;

        // Limit the maximum initial capacity to prevent malicious files from using all the
        // available memory.
        let mut entries = Vec::with_capacity(MAX_TABLE_INITIAL_CAPACITY.min(entry_count as usize));

        for _ in 0..entry_count {
            let sample_count = it.read_u32()?;
            let group_description_index = it.read_u32()?;

            entries.push(SampleToGroupEntry { sample_count, group_description_index });
        }

        Ok(SbgpAtom { grouping_type, entries })
    }
}
