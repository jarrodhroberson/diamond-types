use bumpalo::Bump;
use rle::{HasLength, MergableSpan};
use crate::{CausalGraph, CRDTSpan, DTRange, KVPair, LocalVersion, ROOT_AGENT, Time};
use crate::causalgraph::ClientData;
use crate::encoding::parents::{read_parents_raw, write_parents_raw};
use crate::encoding::tools::{push_str, push_u32, push_u64, push_usize};
use crate::encoding::varint::{mix_bit_u32, mix_bit_usize, num_encode_zigzag_i64, strip_bit_usize_2};
use bumpalo::collections::vec::Vec as BumpVec;
use smallvec::smallvec;
use crate::causalgraph::entry::CGEntry;
use crate::encoding::bufparser::BufParser;
use crate::encoding::Merger;
use crate::encoding::parseerror::ParseError;
use crate::encoding::map::{WriteMap, ReadMap};

pub(crate) fn write_cg_aa(result: &mut BumpVec<u8>, write_parents: bool, span: CRDTSpan,
                          agent_map: &mut WriteMap, persist: bool, cg: &CausalGraph) {
    // We only write the parents info if parents is non-trivial.

    // Its rare, but possible for the agent assignment sequence to jump around a little.
    // This can happen when:
    // - The sequence numbers are shared with other documents, and hence the seqs are sparse
    // - Or the same agent made concurrent changes to multiple branches. The operations may
    //   be reordered to any order which obeys the time dag's partial order.

    let mapped_agent = agent_map.map_no_root_mut(&cg.client_data, span.agent, persist);
    let delta = agent_map.seq_delta(span.agent, span.seq_range, persist);

    // I tried adding an extra bit field to mark len != 1 - so we can skip encoding the
    // length. But in all the data sets I've looked at, len is so rarely 1 that it increased
    // filesize.
    let has_jump = delta != 0;

    let mut write_n = |mapped_agent: u32, is_known: bool| {
        let mut n = mix_bit_u32(mapped_agent, has_jump);
        n = mix_bit_u32(n, is_known);
        n = mix_bit_u32(n, write_parents);
        push_u32(result, n);
    };

    match mapped_agent {
        Ok(mapped_agent) => {
            // Agent is already known in the file. Just use its mapped ID.
            write_n(mapped_agent as u32, true);
        }
        Err(name) => {
            write_n(0, false);
            push_str(result, name);
        }
    }

    push_usize(result, span.len());

    if has_jump {
        push_u64(result, num_encode_zigzag_i64(delta as i64));
    }
}


pub(crate) fn write_cg_entry(result: &mut BumpVec<u8>, data: &CGEntry, write_map: &mut WriteMap,
                             persist: bool, cg: &CausalGraph) {
    assert_ne!(data.span.agent, ROOT_AGENT, "Cannot assign operations to ROOT");
    let write_parents = !data.parents_are_trivial()
        || data.start == 0 // Guard to prevent underflow
        || !write_map.txn_map_has(data.start - 1);

    // Keep the txn map up to date. This is only needed for parents, and it maps from local time
    // values -> output time values (the order in the file). This lets the file be ordered
    // differently from the local time.
    let next_output_time = write_map.txn_map.last().map_or(0, |last| last.1.end);
    let output_range = (next_output_time .. next_output_time + data.len()).into();

    if persist {
        // NOTE: We have to use .insert instead of .push here so the txn_map stays in the expected
        // order! This will only be relevant if write() is called in a different order from the
        // CG, which happens when we optimize the order.
        write_map.txn_map.insert(KVPair(data.start, output_range));
    }

    // We always write the agent assignment info.
    write_cg_aa(result, write_parents, data.span, write_map, persist, cg);

    // And optionally write parents info.
    // Write the parents, if it makes sense to do so.
    if write_parents {
        write_parents_raw(result, &data.parents, next_output_time, persist, write_map, cg);
    }
}

fn read_cg_aa(reader: &mut BufParser, persist: bool, cg: &mut CausalGraph, read_map: &mut ReadMap)
    -> Result<(bool, CRDTSpan), ParseError>
{
    // Bits are:
    // has_parents
    // is_known
    // delta != 0 (has_jump)
    // (mapped agent)

    // dbg!(reader.0);
    let mut n = reader.next_usize()?;

    let has_parents = strip_bit_usize_2(&mut n);
    let is_known = strip_bit_usize_2(&mut n);
    let has_jump = strip_bit_usize_2(&mut n);
    let mapped_agent = n;

    let (agent, last_seq, idx) = if !is_known {
        if mapped_agent != 0 { return Err(ParseError::GenericInvalidData); }
        let agent_name = reader.next_str()?;
        let agent = cg.get_or_create_agent_id(agent_name);
        let idx = read_map.agent_map.len();
        if persist {
            read_map.agent_map.push((agent, 0));
        }
        (agent, 0, idx)
    } else {
        let entry = read_map.agent_map[mapped_agent];
        (entry.0, entry.1, mapped_agent)
    };

    let len = reader.next_usize()?;

    let jump = if has_jump {
        reader.next_zigzag_isize()?
    } else { 0 };

    let start = isize_try_add(last_seq, jump)
        .ok_or(ParseError::GenericInvalidData)?;
    let end = start + len;

    if persist {
        read_map.agent_map[idx].1 = end;
    }

    Ok((has_parents, CRDTSpan {
        agent,
        seq_range: (start..end).into(),
    }))
}

fn isize_try_add(x: usize, y: isize) -> Option<usize> {
    let result = (x as i128) + (y as i128);

    if result < 0 || result > usize::MAX as i128 { None }
    else { Some(result as usize) }
}

/// NOTE: This does not put the returned data into the causal graph, or update read_map's txn_map.
fn read_raw(reader: &mut BufParser, persist: bool, cg: &mut CausalGraph, next_file_time: Time, read_map: &mut ReadMap) -> Result<(LocalVersion, CRDTSpan), ParseError> {
    // First we have agent assignment, then optional parents.
    let (has_parents, span) = read_cg_aa(reader, persist, cg, read_map)?;

    let parents = if has_parents {
        read_parents_raw(reader, persist, cg, next_file_time, read_map)?
    } else {
        let last_time = read_map.last_time().ok_or(ParseError::GenericInvalidData)?;
        smallvec![last_time]
    };

    Ok((parents, span))
}

/// Read a CG entry and save it in the causal graph.
///
/// On success, returns the new CG entry read. Note: the new entry's contents might not be
/// contiguous in the causal graph.
pub(crate) fn read_cg_entry_into_cg_nonoverlapping(reader: &mut BufParser, persist: bool, cg: &mut CausalGraph, read_map: &mut ReadMap) -> Result<CGEntry, ParseError> {
    let next_file_time = read_map.len();
    let (parents, span) = read_raw(reader, persist, cg, next_file_time, read_map)?;
    let merged_span = cg.merge_and_assign_nonoverlapping(&parents, span);

    if persist {
        read_map.txn_map.push(KVPair(next_file_time, merged_span));
    }

    Ok((CGEntry {
        start: merged_span.start,
        parents,
        span
    }))
}

pub(crate) fn read_cg_entry_into_cg(reader: &mut BufParser, persist: bool, cg: &mut CausalGraph, read_map: &mut ReadMap) -> Result<(), ParseError> {
    let mut next_file_time = read_map.len();
    let (parents, span) = read_raw(reader, persist, cg, next_file_time, read_map)?;

    // Save it into the causal graph, and update
    let merged_span = cg.merge_and_assign(&parents, span);

    if persist {
        if merged_span.len() == span.len() {
            // This is the normal case. We read the entire entry.
            read_map.txn_map.push(KVPair(next_file_time, merged_span));
        } else {
            // The file contained some data which is already in the causal graph. We need to read
            // the versions back out of CG to populate read_map, so those versions can be referenced
            // by future edits in the file / data set.
            //
            // We already know the timespan for merged_span - so I could use that and just query the
            // rest. But eh. This is smaller and should be just as performant.
            let client_data = &cg.client_data[span.agent as usize];
            for KVPair(_, time) in client_data.item_times.iter_range_packed(span.seq_range) {
                read_map.txn_map.push(KVPair(next_file_time, time));
                next_file_time += time.len();
            }
        }
    }

    Ok(())
}

pub(crate) fn write_cg_entry_iter<'a, I: Iterator<Item=CGEntry>>(bump: &'a Bump, iter: I, write_map: &mut WriteMap, cg: &CausalGraph) -> BumpVec<'a, u8> {
    // let mut last_seq_for_agent: LastSeqForAgent = bumpvec![in bump; 0; client_data.len()];
    let mut result = BumpVec::new_in(bump);
    let mut next_output_time = 0;

    Merger::new(|entry: CGEntry, _| {
        write_cg_entry(&mut result, &entry, write_map, true, cg);
        // write_agent_assignment_span(&mut result, None, span, map, true, client_data);
    }).flush_iter(iter);

    result
}