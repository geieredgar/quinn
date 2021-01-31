use std::{
    cmp::Ordering,
    collections::{binary_heap::PeekMut, BinaryHeap},
    mem,
};

use bytes::{Buf, Bytes, BytesMut};

use crate::range_set::RangeSet;

/// Helper to assemble unordered stream frames into an ordered stream
#[derive(Debug, Default)]
pub(crate) struct Assembler {
    state: State,
    data: BinaryHeap<Buffer>,
    buffered: usize,
    allocated: usize,
    /// Number of bytes read by the application. When only ordered reads have been used, this is the
    /// length of the contiguous prefix of the stream which has been consumed by the application,
    /// aka the stream offset.
    bytes_read: u64,
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn ensure_ordering(&mut self, ordered: bool) -> Result<(), IllegalOrderedRead> {
        if ordered && !self.state.is_ordered() {
            return Err(IllegalOrderedRead);
        } else if !ordered && self.state.is_ordered() {
            // Enter unordered mode
            let mut recvd = RangeSet::new();
            recvd.insert(0..self.bytes_read);
            for chunk in &self.data {
                recvd.insert(chunk.offset..chunk.offset + chunk.bytes.len() as u64);
            }
            self.state = State::Unordered { recvd };
        }
        Ok(())
    }

    /// Get the the next chunk
    pub(crate) fn read(&mut self, max_length: usize, ordered: bool) -> Option<Chunk> {
        loop {
            let mut chunk = match self.data.peek_mut() {
                Some(chunk) => chunk,
                None => return None,
            };

            if ordered {
                if chunk.offset > self.bytes_read {
                    // Next chunk is after current read index
                    return None;
                } else if (chunk.offset + chunk.bytes.len() as u64) <= self.bytes_read {
                    // Next chunk is useless as the read index is beyond its end
                    self.buffered -= chunk.size;
                    self.allocated -= chunk.allocation_size;
                    PeekMut::pop(chunk);
                    continue;
                }

                // Determine `start` and `len` of the slice of useful data in chunk
                let start = (self.bytes_read - chunk.offset) as usize;
                if start > 0 {
                    chunk.bytes.advance(start);
                    chunk.offset += start as u64;
                }
            }

            return Some(if max_length < chunk.bytes.len() {
                self.bytes_read += max_length as u64;
                let offset = chunk.offset;
                chunk.offset += max_length as u64;
                Chunk::new(offset, chunk.bytes.split_to(max_length))
            } else {
                self.bytes_read += chunk.bytes.len() as u64;
                self.buffered -= chunk.size;
                self.allocated -= chunk.allocation_size;
                let chunk = PeekMut::pop(chunk);
                Chunk::new(chunk.offset, chunk.bytes)
            });
        }
    }

    // Copy the buffered chunk data to new chunks backed by a single buffer to
    // make sure we're not unnecessarily holding on to many larger allocations.
    // Merge contiguous chunks in the process of doing so. Reset the `defragmented`
    // counter to the new number of chunks left in the heap so that we can decide
    // when to defragment the queue again if necessary.
    fn defragment(&mut self) {
        let fragmented_buffered = self
            .data
            .iter()
            .filter(|c| c.size < c.allocation_size)
            .map(|c| c.bytes.len())
            .sum::<usize>();
        let mut buffer = BytesMut::with_capacity(fragmented_buffered);
        let mut offset = self
            .data
            .peek()
            .as_ref()
            .expect("defragment is only called when data is buffered")
            .offset;

        let new = BinaryHeap::with_capacity(self.data.len());
        let old = mem::replace(&mut self.data, new);
        for chunk in old.into_sorted_vec().into_iter().rev() {
            if chunk.size < chunk.allocation_size {
                let end = offset + (buffer.len() as u64);
                if let Some(overlap) = end.checked_sub(chunk.offset) {
                    if let Some(bytes) = chunk.bytes.get(overlap as usize..) {
                        buffer.extend_from_slice(bytes);
                    }
                } else {
                    let bytes = buffer.split().freeze();
                    let size = bytes.len();
                    self.data.push(Buffer {
                        offset,
                        bytes,
                        size,
                        allocation_size: size,
                    });
                    offset = chunk.offset;
                    buffer.extend_from_slice(&chunk.bytes);
                }
            } else {
                self.data.push(chunk);
            }
        }

        let bytes = buffer.split().freeze();
        let size = bytes.len();
        self.data.push(Buffer {
            offset,
            bytes,
            size,
            allocation_size: size,
        });
        self.allocated = self.buffered;
    }

    pub(crate) fn insert(&mut self, mut offset: u64, mut bytes: Bytes, allocation_size: usize) {
        assert!(
            bytes.len() <= allocation_size,
            "allocation_size less than bytes.len(): {:?} < {:?}",
            allocation_size,
            bytes.len()
        );
        if let State::Unordered { ref mut recvd } = self.state {
            // Discard duplicate data
            for duplicate in recvd.replace(offset..offset + bytes.len() as u64) {
                if duplicate.start > offset {
                    let bytes = bytes.split_to((duplicate.start - offset) as usize);
                    let size = bytes.len();
                    self.buffered += size;
                    self.allocated += allocation_size;
                    self.data.push(Buffer {
                        offset,
                        bytes,
                        size,
                        allocation_size,
                    });
                    offset = duplicate.start;
                }
                bytes.advance((duplicate.end - offset) as usize);
                offset = duplicate.end;
            }
        } else if offset < self.bytes_read {
            if (offset + bytes.len() as u64) <= self.bytes_read {
                return;
            } else {
                let diff = self.bytes_read - offset;
                offset += diff;
                bytes.advance(diff as usize);
            }
        }

        if bytes.is_empty() {
            return;
        }

        let size = bytes.len();
        self.buffered += size;
        self.allocated += allocation_size;
        self.data.push(Buffer {
            offset,
            bytes,
            size,
            allocation_size,
        });
        // Rationale: on the one hand, we want to defragment rarely, ideally never
        // in non-pathological scenarios. However, a pathological or malicious
        // peer could send us one-byte frames, and since we use reference-counted
        // buffers in order to prevent copying, this could result in keeping a lot
        // of memory allocated. This limits over-allocation in proportion to the
        // buffered data. The constants are chosen somewhat arbitrarily and try to
        // balance between defragmentation overhead and over-allocation.
        let over_allocation = (self.allocated - self.buffered) as f32;
        let threshold = ((self.buffered as f32) * 1.5f32).max(4096f32);
        if over_allocation > threshold {
            self.defragment()
        }
    }

    pub(crate) fn set_bytes_read(&mut self, new: u64) {
        self.bytes_read = new;
    }

    /// Number of bytes consumed by the application
    pub(crate) fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Discard all buffered data
    pub(crate) fn clear(&mut self) {
        self.data.clear();
        self.buffered = 0;
        self.allocated = 0;
    }
}

/// A chunk of data from the receive stream
#[derive(Debug, PartialEq)]
pub struct Chunk {
    /// The offset in the stream
    pub offset: u64,
    /// The contents of the chunk
    pub bytes: Bytes,
}

impl Chunk {
    fn new(offset: u64, bytes: Bytes) -> Self {
        Chunk { offset, bytes }
    }
}

#[derive(Debug, Eq)]
struct Buffer {
    offset: u64,
    bytes: Bytes,
    size: usize,
    allocation_size: usize,
}

impl Ord for Buffer {
    // Invert ordering based on offset (max-heap, min offset first),
    // prioritize longer chunks at the same offset.
    fn cmp(&self, other: &Buffer) -> Ordering {
        self.offset
            .cmp(&other.offset)
            .reverse()
            .then(self.bytes.len().cmp(&other.bytes.len()))
    }
}

impl PartialOrd for Buffer {
    fn partial_cmp(&self, other: &Buffer) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Buffer {
    fn eq(&self, other: &Buffer) -> bool {
        (self.offset, self.bytes.len()) == (other.offset, other.bytes.len())
    }
}

#[derive(Debug)]
enum State {
    Ordered,
    Unordered {
        /// The set of offsets that have been received from the peer, including portions not yet
        /// read by the application.
        recvd: RangeSet,
    },
}

impl State {
    fn is_ordered(&self) -> bool {
        matches!(self, State::Ordered)
    }
}

impl Default for State {
    fn default() -> Self {
        State::Ordered
    }
}

/// Error indicating that an ordered read was performed on a stream after an unordered read
#[derive(Debug)]
pub struct IllegalOrderedRead;

#[cfg(test)]
mod test {
    use super::*;
    use assert_matches::assert_matches;

    #[test]
    fn assemble_ordered() {
        let mut x = Assembler::new();
        assert_matches!(next(&mut x, 32), None);
        x.insert(0, Bytes::from_static(b"123"), 3);
        assert_matches!(next(&mut x, 1), Some(ref y) if &y[..] == b"1");
        assert_matches!(next(&mut x, 3), Some(ref y) if &y[..] == b"23");
        x.insert(3, Bytes::from_static(b"456"), 3);
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"456");
        x.insert(6, Bytes::from_static(b"789"), 3);
        x.insert(9, Bytes::from_static(b"10"), 2);
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"789");
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"10");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_unordered() {
        let mut x = Assembler::new();
        x.ensure_ordering(false).unwrap();
        x.insert(3, Bytes::from_static(b"456"), 3);
        assert_matches!(next(&mut x, 32), None);
        x.insert(0, Bytes::from_static(b"123"), 3);
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123");
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"456");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_duplicate() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"123"), 3);
        x.insert(0, Bytes::from_static(b"123"), 3);
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_duplicate_compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"123"), 3);
        x.insert(0, Bytes::from_static(b"123"), 3);
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_contained() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"12345"), 5);
        x.insert(1, Bytes::from_static(b"234"), 3);
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"12345");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_contained_compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"12345"), 5);
        x.insert(1, Bytes::from_static(b"234"), 3);
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"12345");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_contains() {
        let mut x = Assembler::new();
        x.insert(1, Bytes::from_static(b"234"), 3);
        x.insert(0, Bytes::from_static(b"12345"), 5);
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"12345");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_contains_compact() {
        let mut x = Assembler::new();
        x.insert(1, Bytes::from_static(b"234"), 3);
        x.insert(0, Bytes::from_static(b"12345"), 5);
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"12345");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_overlapping() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"123"), 3);
        x.insert(1, Bytes::from_static(b"234"), 3);
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123");
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"4");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_overlapping_compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"123"), 4);
        x.insert(1, Bytes::from_static(b"234"), 4);
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"1234");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_complex() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"1"), 1);
        x.insert(2, Bytes::from_static(b"3"), 1);
        x.insert(4, Bytes::from_static(b"5"), 1);
        x.insert(0, Bytes::from_static(b"123456"), 6);
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123456");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_complex_compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"1"), 1);
        x.insert(2, Bytes::from_static(b"3"), 1);
        x.insert(4, Bytes::from_static(b"5"), 1);
        x.insert(0, Bytes::from_static(b"123456"), 6);
        x.defragment();
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"123456");
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn assemble_old() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"1234"), 4);
        assert_matches!(next(&mut x, 32), Some(ref y) if &y[..] == b"1234");
        x.insert(0, Bytes::from_static(b"1234"), 4);
        assert_matches!(next(&mut x, 32), None);
    }

    #[test]
    fn compact() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"abc"), 4);
        x.insert(3, Bytes::from_static(b"def"), 4);
        x.insert(9, Bytes::from_static(b"jkl"), 4);
        x.insert(12, Bytes::from_static(b"mno"), 4);
        x.defragment();
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(0, Bytes::from_static(b"abcdef"))
        );
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(9, Bytes::from_static(b"jklmno"))
        );
    }

    #[test]
    fn defrag_with_missing_prefix() {
        let mut x = Assembler::new();
        x.insert(3, Bytes::from_static(b"def"), 3);
        x.defragment();
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(3, Bytes::from_static(b"def"))
        );
    }

    #[test]
    fn defrag_read_chunk() {
        let mut x = Assembler::new();
        x.insert(3, Bytes::from_static(b"def"), 4);
        x.insert(0, Bytes::from_static(b"abc"), 4);
        x.insert(7, Bytes::from_static(b"hij"), 4);
        x.insert(11, Bytes::from_static(b"lmn"), 4);
        x.defragment();
        assert_matches!(x.read(usize::MAX, true), Some(ref y) if &y.bytes[..] == b"abcdef");
        x.insert(5, Bytes::from_static(b"fghijklmn"), 9);
        assert_matches!(x.read(usize::MAX, true), Some(ref y) if &y.bytes[..] == b"ghijklmn");
        x.insert(13, Bytes::from_static(b"nopq"), 4);
        assert_matches!(x.read(usize::MAX, true), Some(ref y) if &y.bytes[..] == b"opq");
        x.insert(15, Bytes::from_static(b"pqrs"), 4);
        assert_matches!(x.read(usize::MAX, true), Some(ref y) if &y.bytes[..] == b"rs");
        assert_matches!(x.read(usize::MAX, true), None);
    }

    #[test]
    fn unordered_happy_path() {
        let mut x = Assembler::new();
        x.ensure_ordering(false).unwrap();
        x.insert(0, Bytes::from_static(b"abc"), 3);
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(0, Bytes::from_static(b"abc"))
        );
        assert_eq!(x.read(usize::MAX, false), None);
        x.insert(3, Bytes::from_static(b"def"), 3);
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(3, Bytes::from_static(b"def"))
        );
        assert_eq!(x.read(usize::MAX, false), None);
    }

    #[test]
    fn unordered_dedup() {
        let mut x = Assembler::new();
        x.ensure_ordering(false).unwrap();
        x.insert(3, Bytes::from_static(b"def"), 3);
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(3, Bytes::from_static(b"def"))
        );
        assert_eq!(x.read(usize::MAX, false), None);
        x.insert(0, Bytes::from_static(b"a"), 1);
        x.insert(0, Bytes::from_static(b"abcdefghi"), 9);
        x.insert(0, Bytes::from_static(b"abcd"), 4);
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(0, Bytes::from_static(b"a"))
        );
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(1, Bytes::from_static(b"bc"))
        );
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(6, Bytes::from_static(b"ghi"))
        );
        assert_eq!(x.read(usize::MAX, false), None);
        x.insert(8, Bytes::from_static(b"ijkl"), 4);
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(9, Bytes::from_static(b"jkl"))
        );
        assert_eq!(x.read(usize::MAX, false), None);
        x.insert(12, Bytes::from_static(b"mno"), 3);
        assert_eq!(
            next_unordered(&mut x),
            Chunk::new(12, Bytes::from_static(b"mno"))
        );
        assert_eq!(x.read(usize::MAX, false), None);
        x.insert(2, Bytes::from_static(b"cde"), 3);
        assert_eq!(x.read(usize::MAX, false), None);
    }

    #[test]
    fn chunks_dedup() {
        let mut x = Assembler::new();
        x.insert(3, Bytes::from_static(b"def"), 3);
        assert_eq!(x.read(usize::MAX, true), None);
        x.insert(0, Bytes::from_static(b"a"), 1);
        x.insert(1, Bytes::from_static(b"bcdefghi"), 9);
        x.insert(0, Bytes::from_static(b"abcd"), 4);
        assert_eq!(
            x.read(usize::MAX, true),
            Some(Chunk::new(0, Bytes::from_static(b"abcd")))
        );
        assert_eq!(
            x.read(usize::MAX, true),
            Some(Chunk::new(4, Bytes::from_static(b"efghi")))
        );
        assert_eq!(x.read(usize::MAX, true), None);
        x.insert(8, Bytes::from_static(b"ijkl"), 4);
        assert_eq!(
            x.read(usize::MAX, true),
            Some(Chunk::new(9, Bytes::from_static(b"jkl")))
        );
        assert_eq!(x.read(usize::MAX, true), None);
        x.insert(12, Bytes::from_static(b"mno"), 3);
        assert_eq!(
            x.read(usize::MAX, true),
            Some(Chunk::new(12, Bytes::from_static(b"mno")))
        );
        assert_eq!(x.read(usize::MAX, true), None);
        x.insert(2, Bytes::from_static(b"cde"), 3);
        assert_eq!(x.read(usize::MAX, true), None);
    }

    #[test]
    fn ordered_eager_discard() {
        let mut x = Assembler::new();
        x.insert(0, Bytes::from_static(b"abc"), 3);
        assert_eq!(x.data.len(), 1);
        assert_eq!(
            x.read(usize::MAX, true),
            Some(Chunk::new(0, Bytes::from_static(b"abc")))
        );
        x.insert(0, Bytes::from_static(b"ab"), 2);
        assert_eq!(x.data.len(), 0);
        x.insert(2, Bytes::from_static(b"cd"), 2);
        assert_eq!(
            x.data.peek(),
            Some(&Buffer {
                offset: 3,
                bytes: Bytes::from_static(b"d"),
                size: 1,
                allocation_size: 2
            })
        );
    }

    fn next_unordered(x: &mut Assembler) -> Chunk {
        x.read(usize::MAX, false).unwrap()
    }

    fn next(x: &mut Assembler, size: usize) -> Option<Bytes> {
        x.read(size, true).map(|chunk| chunk.bytes)
    }
}
