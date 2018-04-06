use hash::tree::{self, HashTreeBackend};

use std::borrow::Cow;
use std::mem;

pub struct FileReader {
    rest: Option<Box<Iterator<Item = Vec<u8>>>>,
    offset: u64,
    buf: Vec<u8>,
    eof: bool,
}

impl FileReader {
    pub fn new<B>(backend: B, file: tree::HashRef) -> Result<FileReader, B::Err>
    where
        B: HashTreeBackend + 'static,
    {
        let tree = tree::LeafIterator::new(backend, file)?
            .map(|t| Box::new(t) as Box<Iterator<Item = Vec<u8>>>);

        Ok(FileReader {
            eof: tree.is_none(),
            rest: tree,
            offset: 0,
            buf: Vec::with_capacity(16 * 1024),
        })
    }

    fn next(&mut self) -> Vec<u8> {
        if let Some(ref mut rest) = self.rest {
            self.offset += self.buf.len() as u64;
            if let Some(buf) = rest.next() {
                return mem::replace(&mut self.buf, buf);
            }
        }
        self.buf.clear();
        self.eof = true;
        vec![]
    }

    fn advance(&mut self, offset: u64) {
        while self.offset + (self.buf.len() as u64) < offset || self.buf.is_empty() {
            self.next();
            if self.eof {
                break;
            }
        }
    }

    fn from(&mut self, offset: u64) -> &[u8] {
        assert!(self.offset <= offset);
        assert!(offset - self.offset <= (self.buf.len() as u64));
        &self.buf[(offset as usize) - (self.offset as usize)..]
    }

    fn take(&mut self, offset: u64, size: usize) -> &[u8] {
        &self.from(offset)[..size]
    }

    pub fn read(&mut self, offset: u64, size: usize) -> Option<Cow<[u8]>> {
        self.advance(offset);

        if self.eof || self.from(offset).is_empty() {
            return None;
        }

        let avail = self.from(offset).len();

        if size <= avail {
            Some(Cow::Borrowed(self.take(offset, size)))
        } else {
            let mut buf = Vec::with_capacity(size as usize);
            buf.extend_from_slice(self.take(offset, avail));
            if let Some(slice) = self.read(offset + (avail as u64), size - avail) {
                buf.extend_from_slice(&slice);
            }
            Some(Cow::Owned(buf))
        }
    }
}
