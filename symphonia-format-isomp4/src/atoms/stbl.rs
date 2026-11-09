// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::Error;

use crate::atoms::sgpd::ROLL_GROUPING_TYPE;
use crate::atoms::{
    Atom, AtomError, AtomHeader, AtomIterator, AtomType, Co64Atom, ReadAtom, Result, SbgpAtom,
    SgpdAtom, StcoAtom, StscAtom, StsdAtom, StszAtom, SttsAtom, decode_error,
};

use log::{debug, warn};

/// Handle the error `err` reading a sample group atom. The roll recovery group is a seek hint, so a
/// malformed sample group atom drops it rather than refusing the file. An I/O error is returned.
fn ignore_malformed_sample_group(err: AtomError) -> Result<()> {
    match err {
        AtomError::Other(Error::IoError(_)) => Err(err),
        err => {
            let err = Error::from(err);
            warn!("isomp4: ignoring the roll recovery group (malformed sample group atom: {err})");
            Ok(())
        }
    }
}

/// The roll recovery sample group of a track (ISO/IEC 14496-12, §10.1).
#[derive(Debug)]
pub struct RollRecovery {
    /// The group descriptions.
    sgpd: SgpdAtom,
    /// The mapping of samples to group descriptions, if any.
    sbgp: Option<SbgpAtom>,
}

impl RollRecovery {
    /// Get the number of samples before the sample `sample_num` that must be decoded for it to be
    /// decoded correctly.
    pub fn pre_roll(&self, sample_num: u32) -> u32 {
        // A sample not mapped by the sample to group atom belongs to the default group
        // description, if there is one.
        let mut index = self.sgpd.default_index;

        if let Some(sbgp) = &self.sbgp {
            let mut next_run_first_sample = 0u64;

            for entry in &sbgp.entries {
                next_run_first_sample += u64::from(entry.sample_count);

                if u64::from(sample_num) < next_run_first_sample {
                    index = entry.group_description_index;
                    break;
                }
            }
        }

        // The index is 1-based, and 0 is no group. A negative roll distance is a pre-roll.
        match index.checked_sub(1).and_then(|i| self.sgpd.roll_distances.get(i as usize)) {
            Some(&distance) if distance < 0 => u32::from(distance.unsigned_abs()),
            _ => 0,
        }
    }
}

/// Sample table atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct StblAtom {
    pub stsd: StsdAtom,
    pub stts: SttsAtom,
    pub stsc: StscAtom,
    pub stsz: StszAtom,
    pub stco: Option<StcoAtom>,
    pub co64: Option<Co64Atom>,
    pub roll: Option<RollRecovery>,
}

impl Atom for StblAtom {
    fn read<R: ReadAtom>(it: &mut AtomIterator<R>, _header: &AtomHeader) -> Result<Self> {
        let mut stsd = None;
        let mut stts = None;
        let mut stsc = None;
        let mut stsz = None;
        let mut stco = None;
        let mut co64 = None;
        let mut roll_sgpd = None;
        let mut roll_sbgp = None;
        let mut roll_malformed = false;

        while let Some(header) = it.next_header()? {
            match header.atom_type {
                AtomType::SampleDescription => {
                    stsd = Some(it.read_atom::<StsdAtom>()?);
                }
                AtomType::TimeToSample => {
                    stts = Some(it.read_atom::<SttsAtom>()?);
                }
                AtomType::CompositionTimeToSample => {
                    // Composition time to sample atom is only required for video.
                    debug!("ignoring ctts atom.");
                }
                AtomType::SyncSample => {
                    // Sync sample atom is only required for video.
                    debug!("ignoring stss atom.");
                }
                AtomType::SampleToChunk => {
                    stsc = Some(it.read_atom::<StscAtom>()?);
                }
                AtomType::SampleSize => {
                    stsz = Some(it.read_atom::<StszAtom>()?);
                }
                AtomType::ChunkOffset => {
                    stco = Some(it.read_atom::<StcoAtom>()?);
                }
                AtomType::ChunkOffset64 => {
                    co64 = Some(it.read_atom::<Co64Atom>()?);
                }
                AtomType::SampleGroupDescription => match it.read_atom::<SgpdAtom>() {
                    Ok(sgpd) if sgpd.grouping_type == ROLL_GROUPING_TYPE => roll_sgpd = Some(sgpd),
                    Ok(_) => (),
                    Err(err) => {
                        ignore_malformed_sample_group(err)?;
                        roll_malformed = true;
                    }
                },
                AtomType::SampleToGroup => match it.read_atom::<SbgpAtom>() {
                    Ok(sbgp) if sbgp.grouping_type == ROLL_GROUPING_TYPE => roll_sbgp = Some(sbgp),
                    Ok(_) => (),
                    Err(err) => {
                        ignore_malformed_sample_group(err)?;
                        roll_malformed = true;
                    }
                },
                _ => (),
            }
        }

        // A malformed roll recovery group is dropped whole: without its sample to group atom, every
        // sample would belong to the default group description, if there is one.
        let roll = roll_sgpd
            .filter(|_| !roll_malformed)
            .map(|sgpd| RollRecovery { sgpd, sbgp: roll_sbgp });

        if stco.is_none() && co64.is_none() {
            // This is a spec. violation, but some m4a files appear to lack these atoms.
            warn!("missing stco or co64 atom");
        }

        let Some(stsd) = stsd
        else {
            return decode_error("isomp4 (stbl): missing stsd atom");
        };
        let Some(stts) = stts
        else {
            return decode_error("isomp4 (stbl): missing stts atom");
        };
        let Some(stsc) = stsc
        else {
            return decode_error("isomp4 (stbl): missing stsc atom");
        };
        let Some(stsz) = stsz
        else {
            return decode_error("isomp4 (stbl): missing stsz atom");
        };

        Ok(StblAtom { stsd, stts, stsc, stsz, stco, co64, roll })
    }
}
