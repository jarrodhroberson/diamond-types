//! This is an experiment in storing the causal graph (time DAG) in a file.
//!
//! The file starts with magic bytes ("DMNDT_CG") and a version.
//!
//! Then we have the 2 blitting buffers. The buffers store outstanding entries for both agent
//! assignment and parent information.
//!
//! Then all the chunks. Each chunk has a type.
//!
//!
//! Blitting buffers contain:
//! - Checksum
//! - Length
//! - Entry index (goes up every time we flush to the end of the file)
//! - Counter (goes up every time we blit back and forth)
//! - Actual data


use std::cmp::Ordering;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io;
use std::io::{BufReader, BufWriter, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::Path;
use bumpalo::Bump;
use rle::{HasLength, MergableSpan, RleRun};
use crate::encoding::agent_assignment::{AgentMappingDec, AgentMappingEnc, read_agent_assignment, write_agent_assignment_span};
use crate::encoding::bufparser::BufParser;
use crate::encoding::parents::{read_txn_entry, TxnMap, write_txn_entry};
use crate::encoding::parseerror::ParseError;
use crate::encoding::tools::{push_u32, push_u64, push_usize};
use crate::encoding::varint::{decode_usize, encode_usize, strip_bit_u32};
use crate::history::MinimalHistoryEntry;
use crate::list::encoding::calc_checksum;
use crate::{CRDTSpan, NewOpLog};
use bumpalo::collections::vec::Vec as BumpVec;


const CG_MAGIC_BYTES: [u8; 8] = *b"DMNDT_CG";
const CG_VERSION: [u8; 4] = 1u32.to_le_bytes();

const CG_DEFAULT_BLIT_SIZE: u64 = 64;

// Magic bytes, version then blit size.
const CG_HEADER_LENGTH: usize = CG_MAGIC_BYTES.len() + CG_VERSION.len() + 4;
const CG_HEADER_LENGTH_U64: u64 = CG_HEADER_LENGTH as u64;

const MAX_BLIT_SIZE: usize = 1024;

#[derive(Debug)]
#[non_exhaustive]
pub enum CGError {
    InvalidHeader,
    UnexpectedEOF,
    ChecksumMismatch,

    InvalidBlit,
    InvalidData,

    BlitTooLarge,

    ParseError(ParseError),
    IO(io::Error),
}

impl Display for CGError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ParseError {:?}", self)
    }
}

impl Error for CGError {}

impl From<io::Error> for CGError {
    fn from(io_err: io::Error) -> Self {
        if io_err.kind() == ErrorKind::UnexpectedEof { CGError::UnexpectedEOF }
        else { CGError::IO(io_err) }
    }
}
impl From<ParseError> for CGError {
    fn from(pe: ParseError) -> Self {
        CGError::ParseError(pe)
    }
}

#[derive(Debug, Clone)]
struct Blit<'a> {
    filesize: u64,
    counter: usize,
    data: &'a [u8],
}

impl<'a> PartialEq for Blit<'a> {
    // I don't think this is ever executed anyway.
    fn eq(&self, other: &Self) -> bool {
        self.filesize == other.filesize && self.counter == other.counter
    }
}

impl<'a> Eq for Blit<'a> {}

impl<'a> PartialOrd<Self> for Blit<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for Blit<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.filesize.cmp(&other.filesize)
            .then(self.counter.cmp(&other.counter))
    }
}

// #[derive(Debug, Eq, PartialEq, Copy, Clone)]
// enum ChunkType {
//     Parents,
//     AgentAssignment
// }

#[derive(Debug)]
struct CausalGraphStorage {
    file: File,

    blit_size: u64,

    /// The write location is the position in the file where the next written chunk will go.
    /// This is an offset from the start of the data chunk (after header & blits).
    next_write_location: u64,

    /// The counter increments by 1 every time we update a blit without flushing a new chunk. Resets
    /// to 0 every time we write a chunk (and thus the write location increases).
    next_counter: usize,

    /// Set when we've appended data to the file but haven't marked the new file length via a blit
    /// operations. Call .flush() kiddos!
    dirty: bool,
    /// False when we're ready to write blit 0, true when we're about to write blit 1.
    next_blit: bool,

    // last_entry: RleRun<bool>,

    last_parents: MinimalHistoryEntry,
    assigned_to: CRDTSpan,

    txn_map: TxnMap,
    agent_map: AgentMappingEnc,
}

impl CausalGraphStorage {
    pub fn open<P: AsRef<Path>>(path: P, into_oplog: &mut NewOpLog) -> Result<Self, CGError> {
        assert!(into_oplog.is_empty(), "Merging oplogs not yet implemented");

        let mut file = File::options()
            .read(true)
            .create(true)
            .write(true)
            .append(false)
            .open(path.as_ref())?;

        let mut total_len = file.seek(SeekFrom::End(0))?;
        file.seek(SeekFrom::Start(0))?;
        let blit_size = Self::read_header(&mut file, total_len)?;
        debug_assert_eq!(file.stream_position()?, CG_HEADER_LENGTH_U64);
        total_len = total_len.max(CG_HEADER_LENGTH_U64);

        let mut cgs = Self {
            file,

            blit_size,

            next_counter: 0,
            next_write_location: 0,
            dirty: false,
            next_blit: false,
            // last_entry: Default::default(),
            last_parents: MinimalHistoryEntry {
                span: Default::default(), parents: Default::default()
            },
            assigned_to: CRDTSpan {
                agent: 0,
                seq_range: Default::default()
            },
            txn_map: Default::default(),
            agent_map: AgentMappingEnc::new(&into_oplog.client_data),
        };

        // If the file doesn't have room for the blit data, its probably new. Just set_len().
        let ds = cgs.data_start();
        if total_len < ds {
            cgs.file.set_len(ds)?;
            total_len = ds;
            cgs.file.sync_all(); // Force update metadata to include the new size.
        }

        // Next we need to read the blit data to find out the flushed file size. Any bytes after
        // the file size specified in the last blit come from stale writes, and they're discarded.

        // The blits will be read into the provided (stack) buffer.
        let mut raw_buf = [0u8; MAX_BLIT_SIZE * 2];
        let active_blit = cgs.read_initial_blits(&mut raw_buf, blit_size);

        let committed_filesize = active_blit.filesize;

        // dbg!(&active_blit);

        assert!(committed_filesize <= total_len - cgs.data_start());

        debug_assert_eq!(cgs.file.stream_position()?, cgs.data_start());


        // Now scan all the entries in the data chunk.

        // TODO: This is suuuper duper dirty!
        let mut buf = vec![0u8; active_blit.filesize as usize];
        cgs.file.read_exact(&mut buf);
        // dbg!(&buf);

        let mut r = BufParser(&buf);
        let mut dec = AgentMappingDec::new();
        while !r.is_empty() {
            Self::read_run(&mut r, into_oplog, &mut dec)?;
        }
        cgs.agent_map.populate_from_dec(&dec);

        if !active_blit.data.is_empty() {
            let mut reader = BufParser(active_blit.data);
            let next_time = into_oplog.len();
            let txn = read_txn_entry(&mut reader, false, false, into_oplog, next_time, &mut dec)?;
            cgs.last_parents = txn;

            let span = read_agent_assignment(&mut reader, false, false, into_oplog, &mut dec)?;
            cgs.assigned_to = span;
            // dbg!(&cgs.last_parents, &cgs.assigned_to);

            assert!(reader.is_empty());
        }

        debug_assert_eq!(cgs.file.stream_position()?, cgs.data_start() + committed_filesize);

        Ok(cgs)
    }

    fn read_initial_blits<'a>(&mut self, raw_buf: &'a mut [u8; MAX_BLIT_SIZE * 2], blit_size: u64) -> Blit<'a> {
        let bs_u = blit_size as usize;
        let mut buf = &mut raw_buf[..bs_u * 2];
        self.file.read_exact(buf);

        let b1 = Self::read_blit(&buf[0..bs_u]);
        let b2 = Self::read_blit(&buf[bs_u..bs_u * 2]);
        let (active_blit, next_blit) = match (b1, b2) {
            (Ok(b1), Ok(b2)) => {
                // dbg!(&b1, &b2);
                match b1.cmp(&b2) {
                    Ordering::Less | Ordering::Equal => (b2, false),
                    Ordering::Greater => (b1, true),
                }
            },
            (Ok(b1), _) => (b1, true),
            (_, Ok(b2)) => (b2, false),
            _ => {
                (Blit {
                    filesize: 0,
                    counter: 0,
                    data: &[]
                }, false)
            }
        };

        self.next_blit = next_blit;
        self.next_counter = active_blit.counter + 1;
        self.next_write_location = active_blit.filesize;

        active_blit
    }

    fn read_blit(buf: &[u8]) -> Result<Blit, CGError> {
        // Blits always start with a checksum,
        // dbg!(buf);
        let mut pos = 0;
        let expected_checksum = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        pos += 4;

        // Length
        let (len, len_size) = decode_usize(&buf[pos..]).map_err(|e| {
            assert_eq!(e, ParseError::InvalidVarInt);
            CGError::InvalidBlit
        })?;
        pos += len_size;

        // We need to explicitly check for len == 0 because the checksum of nothing is 0 :/
        if len == 0 || buf.len() - pos < len {
            return Err(CGError::InvalidBlit);
        }

        let mut r = BufParser(&buf[pos..pos+len]);

        let actual_checksum = calc_checksum(r.0);
        if expected_checksum != actual_checksum {
            return Err(CGError::ChecksumMismatch);
        }

        let filesize = r.next_u64().map_err(|_| CGError::InvalidBlit)?;
        let counter = r.next_usize().map_err(|_| CGError::InvalidBlit)?;

        Ok(Blit {
            filesize,
            counter,
            data: r.0
        })
    }

    fn next_blit_location(&self) -> u64 {
        CG_HEADER_LENGTH_U64 + (self.blit_size * self.next_blit as u64)
    }

    fn push_data_blit(&mut self, data: &[u8]) -> Result<(), CGError> {
        self.write_blit(Blit {
            filesize: self.next_write_location,
            counter: self.next_counter,
            data
        })?;
        self.next_counter += 1;
        self.dirty = false;
        Ok(())
    }

    fn write_blit(&mut self, blit: Blit) -> Result<(), CGError> {
        debug_assert_eq!(self.file.seek(SeekFrom::Current(0)).unwrap(), self.next_write_location + self.data_start());
        self.file.seek(SeekFrom::Start(self.next_blit_location()));

        Self::write_blit_to(BufWriter::new(&mut self.file), self.blit_size, blit)?;
        self.file.flush()?;
        self.file.sync_data()?;

        self.next_blit = !self.next_blit;
        self.file.seek(SeekFrom::Start(self.next_write_location + self.data_start()))?;

        Ok(())
    }

    fn write_blit_to<W: Write>(mut w: BufWriter<W>, max_size: u64, blit: Blit) -> Result<(), CGError> {
        let mut body = Vec::new(); // Bleh. TODO: Better to allocate on the stack here.
        push_u64(&mut body, blit.filesize);
        push_usize(&mut body, blit.counter);
        body.extend_from_slice(blit.data); // TODO: Less copying!

        let checksum = calc_checksum(&body);
        w.write(&checksum.to_le_bytes())?;

        let mut buf = [0u8; 10];
        let len_len = encode_usize(body.len(), &mut buf);
        w.write(&buf[..len_len])?;

        // TODO: DO THIS BETTER!!
        if 4 + len_len + body.len() > max_size as usize {
            return Err(CGError::BlitTooLarge)
        }

        w.write(&body)?;

        Ok(())
    }

    fn flush(&mut self) -> Result<(), CGError> {
        if self.dirty {
            self.push_data_blit(&[])?;
        }
        Ok(())
    }

    fn write_data(&mut self, data: &[u8]) -> Result<(), io::Error> {
        // First we write the data to the end of the file.
        debug_assert_eq!(self.file.seek(SeekFrom::Current(0)).unwrap(), self.next_write_location + self.data_start());

        self.file.write_all(data)?;
        self.next_write_location += data.len() as u64;
        self.next_counter = 0;

        self.dirty = true;

        Ok(())
    }

    fn data_start(&self) -> u64 {
        CG_HEADER_LENGTH_U64 + self.blit_size * 2
    }

    /// Returns blit size.
    fn read_header(mut file: &mut File, total_len: u64) -> Result<u64, CGError> {
        let blitsize = if total_len < CG_HEADER_LENGTH_U64 {
            // Presumably we're creating a new file.
            let mut bw = BufWriter::new(file);
            bw.write_all(&CG_MAGIC_BYTES)?;
            bw.write_all(&CG_VERSION)?;
            bw.write_all(&(CG_DEFAULT_BLIT_SIZE as u32).to_le_bytes());

            file = bw.into_inner().map_err(|e| e.into_error())?;
            file.sync_all();

            CG_DEFAULT_BLIT_SIZE
        } else {
            // Check the WAL header.
            let mut header = [0u8; CG_HEADER_LENGTH];
            file.read_exact(&mut header)?;
            let mut pos = 0;
            if header[0..CG_MAGIC_BYTES.len()] != CG_MAGIC_BYTES {
                eprintln!("Causality graph has invalid magic bytes");
                return Err(CGError::InvalidHeader);
            }
            pos += CG_MAGIC_BYTES.len();

            if header[pos..pos + CG_VERSION.len()] != CG_VERSION {
                eprintln!("Causality graph has unknown version");
                return Err(CGError::InvalidHeader);
            }
            pos += CG_VERSION.len();

            // Read the blit size.
            // This try_into stuff will get optimized out: https://godbolt.org/z/f886W5hvW
            let blit_size = u32::from_le_bytes(header[pos..pos+4].try_into().unwrap()) as u64;
            if blit_size > MAX_BLIT_SIZE as u64 {
                eprintln!("Causality graph has invalid blit size ({blit_size} > {MAX_BLIT_SIZE})");
                return Err(CGError::InvalidHeader);
            }
            pos += 4;

            blit_size
        };

        debug_assert_eq!(file.stream_position()?, CG_HEADER_LENGTH_U64);
        Ok(blitsize)
    }

    fn read_run(reader: &mut BufParser, into_oplog: &mut NewOpLog, dec: &mut AgentMappingDec) -> Result<(), CGError> {
        // dbg!(data);
        let first_number = reader.peek_u32().map_err(|_| CGError::InvalidData)?.unwrap();
        let is_aa = strip_bit_u32(first_number).1;

        if is_aa {
            // Parse the chunk as agent assignment data
            let span = read_agent_assignment(reader, true, true, into_oplog, dec)?;

            // dbg!(span);
        } else {
            // Parse the chunk as parents.
            let next_time = into_oplog.len();
            let txn = read_txn_entry(reader, true, true, into_oplog, next_time, dec)?;
            // dbg!(txn);
        }

        Ok(())
    }

    // TODO: Consider merging tag and persist parameters here - they're always the same value.
    fn encode_last_parents<'a>(&mut self, buf: &mut BumpVec<u8>, tag: bool, persist: bool, oplog: &NewOpLog) {
        let tag = if tag { Some(false) } else { None };
        write_txn_entry(buf, tag, &self.last_parents, &mut self.txn_map, &mut self.agent_map, persist, oplog);
    }

    fn encode_last_agent_assignment<'a>(&mut self, buf: &mut BumpVec<u8>, tag: bool, persist: bool, oplog: &NewOpLog) {
        let tag = if tag { Some(true) } else { None };
        write_agent_assignment_span(buf, tag, self.assigned_to, &mut self.agent_map, persist, &oplog.client_data);
    }

    pub fn append(&mut self, bump: &Bump, parents: MinimalHistoryEntry, span: CRDTSpan, oplog: &NewOpLog) -> Result<(), CGError> {
        assert_eq!(parents.len(), span.len());
        let mut data_written = false;
        let mut buf = BumpVec::new_in(bump);

        if self.last_parents.is_empty() {
            self.last_parents = parents;
        } else if self.last_parents.can_append(&parents) {
            self.last_parents.append(parents);
        } else {
            // First flush out the current value to the end of the file.
            // eprintln!("Writing parents to data {:?}", self.last_parents);
            self.encode_last_parents(&mut buf, true, true, oplog);
            self.write_data(&buf)?;
            buf.clear();
            data_written = true;

            // Then save the new value in a fresh blit.
            self.last_parents = parents;
        }

        // And for spans.
        if self.assigned_to.is_empty() {
            self.assigned_to = span;
        } else if self.assigned_to.can_append(&span) {
            self.assigned_to.append(span);
        } else {
            // Flush the last span out too.
            // eprintln!("Writing span to data {:?}", self.assigned_to);
            self.encode_last_agent_assignment(&mut buf, true, true, oplog);
            self.write_data(&buf)?;
            buf.clear();
            data_written = true;

            // Then save the new value in a fresh blit.
            self.assigned_to = span;
        }

        if data_written {
            self.file.sync_all()?;
        }

        // Regardless of what happened above, write a new blit entry.
        // eprintln!("Writing blip {:?} / {:?}", self.last_parents, self.assigned_to);
        self.encode_last_parents(&mut buf, false, false, oplog);
        self.encode_last_agent_assignment(&mut buf, false, false, oplog);

        match self.push_data_blit(&buf) {
            Err(CGError::BlitTooLarge) => {
                // The buffered data doesn't fit in the blit region. This should basically never happen
                // in regular use - but if the user merges lots of changes for some reason, or if they
                // have super long UIDs this will happen.
                //
                // Luckily there's a reasonable fallback here - we can flush out the blit to the end of
                // the data segment anyway. We lose some compaction, but this is rare enough it doesn't
                // matter.

                // We could only write out the larger of these two, but eh.
                buf.clear();
                self.encode_last_parents(&mut buf, true, true, oplog);
                self.encode_last_agent_assignment(&mut buf, true, true, oplog);
                self.write_data(&buf)?;
                self.file.sync_all()?;

                self.last_parents.span.clear();
                self.assigned_to.seq_range.clear();

                self.push_data_blit(&[])?;
            },
            Err(e) => { return Err(e); }
            _ => {}
        }

        self.file.sync_data()?;
        Ok(())
    }

}

#[cfg(test)]
mod test {
    use bumpalo::Bump;
    use rand::{Rng, RngCore};
    use smallvec::smallvec;
    use rle::RleRun;
    use crate::history::MinimalHistoryEntry;
    use crate::{CRDTSpan, NewOpLog};
    use crate::storage::causalgraph::CausalGraphStorage;

    #[test]
    fn foo() {
        let mut oplog = NewOpLog::new();
        let seph = oplog.get_or_create_agent_id("seph");
        let mut cg = CausalGraphStorage::open("cg.log", &mut oplog).unwrap();
        dbg!(&cg);

        let bump = Bump::new();
        cg.append(&bump, MinimalHistoryEntry {
            span: (0..10).into(),
            parents: smallvec![],
        }, CRDTSpan {
            agent: seph,
            seq_range: (0..10).into()
        }, &oplog);

        // cg.append(&bump, MinimalHistoryEntry {
        //     span: (10..20).into(),
        //     parents: smallvec![5],
        // }, CRDTSpan {
        //     agent: seph,
        //     seq_range: (10..20).into()
        // }, &oplog);

        dbg!(&cg);

        //
        // cg.append_test(dbg!(RleRun {
        //     val: rand::thread_rng().gen_bool(0.5),
        //     len: (rand::thread_rng().next_u32() % 10) as usize,
        // }));
        // dbg!(&cg);
        // drop(cg);
        //
        //
        // let mut cg = CausalGraphStorage::open("cg.log").unwrap();
        // dbg!(&cg);


    }
}