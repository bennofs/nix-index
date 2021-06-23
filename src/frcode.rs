//! A compact encoding for file tree entries based on sharing prefixes.
//!
//! This module contains a rust implementation of a variant of the `frcode` tool
//! used by GNU findutils' locate. It has been extended to allow meta information
//! to be attached to each entry so it is no longer compatible with the original
//! frcode format.
//! (See http://www.delorie.com/gnu/docs/findutils/locatedb.5.html for a description of the frcode format.)
//!
//! The basic building block of the encoding is a line. Each line has the following format:
//! (the spaces are for readability only, they are not present in the encoding)
//!
//! ```text
//! <metadata> <\x00 byte> <shared prefix differential> <additional path bytes> <newline character>
//! ```
//!
//! Each entry holds two parts of data: metadata, which is just some arbitrary blob of NUL-terminated bytes
//! and a path. Because we are storing file trees, the path will likely share a long prefix with the previous
//! entry's path (we traverse directory entries in sorted order to maximize this chance), so we first store
//! the length of the shared prefix.
//!
//! Since this length will likely be similar to the previous one (if there are many entries in `/foo/bar`, then they will
//! all share a prefix of at least the length of `/foo/bar`) we only store the signed *difference* to the previous shared prefix length
//! (This is why it's called a differential). For differences smaller than +/-127 we store them directly as a single byte. If the
//! difference is greater than that, the first byte will by `0x80` (-128) indicating that the following two bytes represent the
//! difference (with the high byte first [big endian]).
//!
//! As an example, consider the following non-encoded plaintext, where `:` separates the metadata from the path:
//!
//! ```text
//! d:/
//! d:/foo
//! d:/foo/bar
//! f:/foo/bar/test.txt
//! f:/foo/bar/text.txt
//! d:/foo/baz
//! ```
//!
//! This text would be encoded as (using `[v]` to indicate a byte with the value of v)
//!
//! ```text
//! d[0][0]/
//! d[0][1]foo
//! d[0][3]/bar
//! f[0][4]/test.txt
//! f[0][3]xt.txt
//! d[0][-4]z
//! ```
//!
//! At the beginning, there is no previous entry, so the shared prefix length must always be `0` (and so must the shared prefix differential).
//! The second entry shares `1` byte with the first path so the difference is `1`. The third entry shares `4` bytes with the second one, which
//! is `3` more than the shared length of the second one, so we encode a `3` followed by the non-shared bytes, and so on for the remaining entries.
//! The last entry shares four bytes less than the second to last one did with its predecessor, so here the differential is negative.
//!
//! Through this encoding, the size of the index is typically reduces by a factor of 3 to 5.
use std::io::{self, Write, BufRead};
use std::cmp;
use std::ops::{Deref, DerefMut};
use memchr;

// error_chain!{
//     foreign_links {
//         Io(io::Error);
//     }
//     errors {
//         SharedOutOfRange { previous_len: usize, shared_len: isize } {
//             description("shared prefix length out of bounds")
//             
//         }
//         SharedOverflow { shared_len: isize, diff: isize } {
//             description("shared prefix length too big (overflow)")
//             
//         } 
//         MissingNul {
//             
//         }
//         MissingNewline {
//             
//         }
//         MissingPrefixDifferential {
//             
//         }
//     }
// }

use thiserror::Error as ThisError;

#[derive(ThisError, Debug)]
pub enum  Error {
    #[error("length of shared prefix must be >= 0 and <= {} (length of previous item), but found: {}", previous_len, shared_len)]
    SharedOutOfRange { previous_len: usize, shared_len: isize },
    #[error("length of shared prefix too big: cannot add {} to {} without overflow", shared_len, diff)]
    SharedOverflow{ shared_len: isize, diff: isize },
    #[error("missing terminating NUL byte for entry")]
    MissingNul,
    #[error("missing newline separator for entry")]
    MissingNewline,
    #[error("missing the shared prefix length differential for entry")]
    MissingPrefixDifferential,
    #[error("IO Error {0}")]
    Io(#[from] io::Error)
}

/// A buffer that may be resizable or not. This is used for decoding,
/// where we want to make the buffer resizable as long as we haven't decoded
/// a full entry yet but want to lock it as soon as we got a full entry.
///
/// This is necessary because we always need to be able to decode at least
/// one entry to make progress, as we never return partial entries during decoding.
struct ResizableBuf {
    allow_resize: bool,
    data: Vec<u8>,
}

impl ResizableBuf {
    /// Allocates a new resizable buffer with the given initial size.
    ///
    /// The new buffer will allow resizing initially.
    fn new(capacity: usize) -> ResizableBuf {
        ResizableBuf {
            data: vec![0; capacity],
            allow_resize: true,
        }
    }

    /// Resizes the buffer to hold at least `new_size` elements. Returns `true`
    /// if resizing was successful (so that buffer can now hold at least `new_size` elements)
    /// or `false` if not (meaning `new_size` is greater than the current size and resizing
    /// was not allowed).
    fn resize(&mut self, new_size: usize) -> bool {
        if new_size <= self.data.len() {
            return true;
        }

        if !self.allow_resize {
            return false;
        }

        self.data.resize(new_size, b'\x00');
        true
    }
}

impl Deref for ResizableBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl DerefMut for ResizableBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

/// A decoder for the frcode format. It reads data from some input source
/// and returns blocks of decoded entries.
///
/// It will not split the metadata/path parts of individual entries since
/// the primary use case for this is searching, where it is enough to decode
/// the entries that match.
pub struct Decoder<R> {
    /// The input source from which we decode
    reader: R,
    /// Position of the first byte of the path part of the last entry.
    /// We need this to copy the shared prefix.
    last_path: usize,
    /// Position of the start of the entry that didn't fully fit in the buffer in the
    /// last decode iteration. Since this entry was partial, it hasn't been returned to
    /// the user yet and we need to continue decoding this entry in this iteration.
    partial_entry_start: usize,
    /// The length of the shared prefix for the current entry. This is necessary because
    /// the shared length is stored as a difference, so we need the previous value to update it.
    shared_len: isize,
    /// The buffer into which we store the decoded bytes.
    buf: ResizableBuf,
    /// Current write position in buf. The next decoded byte should be written to buf[pos].
    pos: usize,
}

impl<R: BufRead> Decoder<R> {
    /// Construct a new decoder for the given source.
    pub fn new(reader: R) -> Decoder<R> {
        let capacity = 1_000_000;
        Decoder {
            reader: reader,
            buf: ResizableBuf::new(capacity),
            pos: 0,
            last_path: 0,
            shared_len: 0,
            partial_entry_start: 0,
        }
    }

    /// Copies `self.shared_len` bytes from the previous entry's path into the output buffer.
    ///
    /// Returns false if the buffer was too small and could not be resized. In this case, no
    /// bytes will be copied.
    fn copy_shared(&mut self) -> Result<bool, Error> {
        let shared_len = self.shared_len as usize;
        let new_pos = self.pos + shared_len;
        let new_last_path = self.pos;
        if !self.buf.resize(new_pos) {
            return Ok(false);
        }


        if self.shared_len < 0 || self.last_path + shared_len > self.pos {
            return Err(Error::SharedOutOfRange {
                previous_len: self.pos - self.last_path,
                shared_len: self.shared_len,
            });
        }

        let (_, last) = self.buf.split_at_mut(self.last_path);
        let (last, new) = last.split_at_mut(self.pos - self.last_path);
        new[..shared_len].copy_from_slice(&last[..shared_len]);

        self.pos += shared_len;
        self.last_path = new_last_path;
        Ok(true)
    }

    /// Copies bytes from the input reader to the output buffer until a `\x00` byte is read.
    /// The NUL byte is included in the output buffer.
    ///
    /// Returns false if the output buffer was exhausted before a NUL byte could be found and
    /// could not be resized. All bytes that were read before this situation was detected will
    /// have already been copied to the output buffer in this case.
    ///
    /// It will also return false if the end of the input was reached.
    fn read_to_nul(&mut self) -> Result<bool, Error> {
        loop {
            let (done, len) = {
                let &mut Decoder {
                    ref mut reader,
                    ref mut buf,
                    ref mut pos,
                    ..
                } = self;
                let input = match reader.fill_buf() {
                    Ok(data) => data,
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(Error::from(e)),
                };

                if input.is_empty() {
                    return Ok(false);
                }

                let (done, len) = match memchr::memchr(b'\x00', input) {
                    Some(i) => (true, i + 1),
                    None => (false, input.len()),
                };

                let new_pos = *pos + len;
                if buf.resize(new_pos) {
                    buf[*pos..new_pos].copy_from_slice(&input[..len]);
                    *pos = new_pos;
                    (done, len)
                } else {
                    return Ok(false);
                }
            };
            self.reader.consume(len);
            if done {
                return Ok(true);
            }

        }
    }

    /// Read the differential from the input reader. This function will return an error
    /// if the end of input has been reached.
    fn decode_prefix_diff(&mut self) -> Result<i16, Error> {
        let mut buf = [0; 1];
        self.reader.read_exact(&mut buf).map_err(|e| {
            Error::MissingPrefixDifferential
        })?;

        if buf[0] != 0x80 {
            Ok((buf[0] as i8) as i16)
        } else {
            let mut buf = [0; 2];
            self.reader.read_exact(&mut buf).map_err(|e| {
                Error::MissingPrefixDifferential
            })?;
            let high = buf[0] as i16;
            let low = buf[1] as i16;
            Ok(high << 8 | low)
        }
    }

    /// Decodes some entries to fill the buffer and returns a block of decoded entries.
    ///
    /// It will decode as many entries as fit into the internal buffer, but at least one.
    /// In the returned block of bytes, an entry's metadata and path will be separated by a NUL byte
    /// and entries will be terminated with a newline character. This allows for fast searching with
    /// a line based searcher.
    ///
    /// The function does not return partially decoded entries. Because of this, the size of returned
    /// slice will vary from call to call. The last entry which did not fully fit into the buffer yet
    /// will be returned as the first entry at the next call.
    pub fn decode(&mut self) -> Result<&mut [u8], Error> {
        // Save end pointer from previous iteration and reset write position
        let end = self.pos;
        self.pos = 0;

        // We need to preserve some data from the previous iteration, namely:
        //
        // * all data after the `self.last_path` position, for copying the shared prefix
        // * everything from the start of the partial entry, since this entry wasn't fully decoded
        //   in the last iteration and we want to continue decoding it now
        //
        // If we stopped decoding the partial entry after already copying the shared prefix, then
        // `last_path` will already point to the partial entry so it will be greater than `partial_entry_start`.
        //
        // If we stopped decoding during copying the metadata though, which comes before we copy the shared
        // prefix, then `last_path` will point to the previous entry's path, so it will be smaller than
        // `partial_entry_start`.
        //
        // To support both these cases, we take the minimum here.
        let mut copy_pos = cmp::min(self.partial_entry_start, self.last_path);

        // Since we sometimes copy more than just the partial entry, we need to know where the partial entry
        // starts as that is the first position that we want to return (everything before that was already
        // part of an entry returned in the last iteration).
        let item_start = self.partial_entry_start - copy_pos;

        // Shift the last path, because we copy it from copy_pos.. to 0..
        self.last_path -= copy_pos;

        // Now we can do the actual copying. We cannot use copy_from_slice here since source and target
        // may overlap.
        while copy_pos < end {
            self.buf[self.pos] = self.buf[copy_pos];
            self.pos += 1;
            copy_pos += 1;
        }

        // Allow resizing the buffer, since we haven't decoded a full entry yet
        self.buf.allow_resize = true;

        // If the the last decoded byte in the buffer is a NUL byte, that means that
        // we are now at the start of the path part of the entry. This means that
        // we need to copy the shared prefix now.
        let mut found_nul = self.pos > 0 && self.buf[self.pos - 1] == b'\x00';
        if found_nul {
            self.copy_shared()?;
        }

        // At this point, we are guaranteed to be in either the metadata part or the non-shared part
        // of an entry. In both cases, the action that we need to take is the same: copy data till
        // the next NUL byte. After the NUL byte, we know that we are at the end of the metadata part,
        // so we read a differential and copy the shared prefix, and repeat.
        //
        // Note that this loop doesn't care about where entries end. Only the path part of each entry requires
        // special processing, so we can jump from NUL byte to NUL byte, decode the path and then just copy
        // the data from the source when jumping to the next NUL byte.
        loop {
            // Read data up to the next nul byte.
            if !self.read_to_nul()? {
                break;
            }

            // If we have already found a NUL byte before this, so we've now got two NUL bytes, so
            // we've got at least one full entry in between.
            self.buf.allow_resize = !found_nul;

            // We found a NUL byte. Note that we need to set this *after* updating allow_resize,
            // since allow_resize should be set to false only after we've found two NUL bytes.
            found_nul = true;

            // Parse the next prefix length difference
            let diff = self.decode_prefix_diff()? as isize;

            // Update the shared len
            self.shared_len = self.shared_len.checked_add(diff).ok_or_else(|| {
                Error::SharedOverflow {
                    shared_len: self.shared_len,
                    diff: diff,
                }
            })?;

            // Copy the shared prefix
            if !self.copy_shared()? {
                break;
            }
        }

        // Since we don't want to return partially decoded items, we need to find the end of the last entry.
        self.partial_entry_start = memchr::memrchr(b'\n', &self.buf[..self.pos]).ok_or_else(
            || {
                Error::MissingNewline
            },
        )? + 1;
        Ok(&mut self.buf[item_start..self.partial_entry_start])
    }
}

/// This struct implements an encoder for the frcode format. The encoder
/// writes directly to the underlying `Write` instance.
///
/// To encode an entry you should first call `write_meta` a number of times
/// to fill the meta data portion. Then, call `write_path` once to finialize the entry.
///
/// One important property of this encoder is that it is safe to open and close
/// it multiple times on the same stream, like this:
///
/// ```text
/// {
///   let encoder1 = Encoder::new(&mut stream);
/// } // encoder1 gets dropped here
/// {
///   let encoder2 = Encoder::new(&mut stream);
/// }
/// ```
///
/// To support this, the encoder has a "footer" item that will get written when it is dropped.
/// This is necessary because we need to write at least one more entry to reset the shared prefix
/// length to zero, since the next encoder will expect that as initial state.
pub struct Encoder<W: Write> {
    writer: W,
    last: Vec<u8>,
    shared_len: i16,
    footer_meta: Vec<u8>,
    footer_path: Vec<u8>,
    footer_written: bool,
}

impl<W: Write> Drop for Encoder<W> {
    fn drop(&mut self) {
        self.write_footer().expect("failed to write footer")
    }
}

impl<W: Write> Encoder<W> {
    /// Constructs a new encoder for the specific writer.
    ///
    /// The encoder will write the given `footer_meta` and `footer_path` as the last entry.
    ///
    /// # Panics
    ///
    /// If either `footer_meta` or `footer_path` contain NUL or newline bytes.
    pub fn new(writer: W, footer_meta: Vec<u8>, footer_path: Vec<u8>) -> Encoder<W> {
        assert!(
            !footer_meta.contains(&b'\x00'),
            "footer meta must not contain null bytes"
        );
        assert!(
            !footer_path.contains(&b'\x00'),
            "footer path must not contain null bytes"
        );
        assert!(
            !footer_meta.contains(&b'\n'),
            "footer meta must not contain newlines"
        );
        assert!(
            !footer_path.contains(&b'\n'),
            "footer path must not contain newlines"
        );
        Encoder {
            writer: writer,
            last: Vec::new(),
            shared_len: 0,
            footer_meta: footer_meta,
            footer_path: footer_path,
            footer_written: false,
        }
    }

    /// Writes the specific shared prefix differential to the output stream.
    ///
    /// This function takes care of the variable-length encoding using for prefix differentials
    /// in the frcode format.
    fn encode_diff(&mut self, diff: i16) -> io::Result<()> {
        let low = (diff & 0xFF) as u8;
        if diff.abs() < i8::max_value() as i16 {
            self.writer.write_all(&[low])?;
        } else {
            let high = ((diff >> 8) & 0xFF) as u8;
            self.writer.write_all(&[0x80, high, low])?;
        }
        Ok(())
    }

    /// Writes the meta data of an entry to the output stream.
    ///
    /// This function can be called multiple times to extend the current meta data part.
    /// Since the meta data is written as-is to the output stream, calling the function
    /// multiple times will concatenate the meta data of all calls.
    ///
    /// # Panics
    ///
    /// If the meta data contains NUL bytes or newlines.
    pub fn write_meta(&mut self, meta: &[u8]) -> io::Result<()> {
        assert!(
            !meta.contains(&b'\x00'),
            "entry must not contain null bytes"
        );
        assert!(!meta.contains(&b'\n'), "entry must not contain newlines");

        self.writer.write_all(meta)?;
        Ok(())
    }

    /// Finalizes an entry by encoding its path to the output stream.
    ///
    /// This function should be called after you've finished writing the meta data for
    /// the current entry. It will terminate the meta data part by writing the NUL byte
    /// and then encode the path into the output stream.
    ///
    /// The entry will be terminated with a newline.
    ///
    /// # Panics
    ///
    /// If the path contains NUL bytes or newlines.
    pub fn write_path(&mut self, path: Vec<u8>) -> io::Result<()> {
        assert!(
            !path.contains(&b'\x00'),
            "entry must not contain null bytes"
        );
        assert!(!path.contains(&b'\x00'), "entry must not contain newlines");
        self.writer.write_all(&[b'\x00'])?;

        let mut shared: isize = 0;
        let max_shared = i16::max_value() as isize;
        for (a, b) in self.last.iter().zip(path.iter()) {
            if a != b || shared > max_shared {
                break;
            }
            shared += 1;
        }
        let shared = shared as i16;

        let diff = shared - self.shared_len;
        self.encode_diff(diff)?;

        self.last = path;
        self.shared_len = shared;

        let pos = shared as usize;
        self.writer.write_all(&self.last[pos..])?;
        self.writer.write_all(b"\n")?;

        Ok(())
    }

    /// Writes the footer entry.
    ///
    /// The footer entry will not share any prefix with the preceding entry,
    /// so after this function, the shared prefix length is zero. This guarantees
    /// that we can start another Encoder after this item, since the Encoder expects
    /// the initial shared prefix length to be zero.
    fn write_footer(&mut self) -> io::Result<()> {
        if self.footer_written {
            return Ok(());
        }

        let diff = -self.shared_len;
        self.writer.write_all(&self.footer_meta)?;
        self.writer.write_all(b"\x00")?;
        self.encode_diff(diff)?;
        self.writer.write_all(&self.footer_path)?;
        self.writer.write_all(b"\n")?;
        self.footer_written = true;
        Ok(())
    }

    /// Finishes the encoder by writing the footer entry.
    ///
    /// This function is called by drop, but calling it explictly is recommended as
    /// drop has no way to report IO errors that may occur during writing the footer.
    pub fn finish(mut self) -> io::Result<()> {
        self.write_footer()?;

        Ok(())
    }
}
