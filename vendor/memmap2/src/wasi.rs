//! WASI has no `mmap`. Maps are emulated by reading the requested range of the file into memory.
//!
//! This is enough for read-only and copy-on-write maps, which is all that consumers like `gix`
//! use: the data is a private snapshot of the file, just like `MAP_PRIVATE`. Maps that would
//! write back to the file (`map_mut`) or be executable are not supported.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

pub struct MmapInner {
    data: Box<[u8]>,
}

impl MmapInner {
    fn read(len: usize, file: &File, offset: u64) -> io::Result<MmapInner> {
        let mut data = vec![0u8; len];
        let mut file = file;
        // A real map doesn't move the file's cursor, and callers keep reading from the same
        // descriptor afterwards (gix hashes the index through it), so put the cursor back.
        let position = file.stream_position()?;
        file.seek(SeekFrom::Start(offset))?;
        let read = file.read_exact(&mut data);
        file.seek(SeekFrom::Start(position))?;
        read?;
        Ok(MmapInner {
            data: data.into_boxed_slice(),
        })
    }

    pub fn map(len: usize, file: &File, offset: u64, _populate: bool, _no_reserve: bool) -> io::Result<MmapInner> {
        MmapInner::read(len, file, offset)
    }

    pub fn map_exec(_: usize, _: &File, _: u64, _: bool, _: bool) -> io::Result<MmapInner> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn map_mut(_: usize, _: &File, _: u64, _: bool, _: bool) -> io::Result<MmapInner> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn map_copy(len: usize, file: &File, offset: u64, _populate: bool, _no_reserve: bool) -> io::Result<MmapInner> {
        MmapInner::read(len, file, offset)
    }

    pub fn map_copy_read_only(
        len: usize,
        file: &File,
        offset: u64,
        _populate: bool,
        _no_reserve: bool,
    ) -> io::Result<MmapInner> {
        MmapInner::read(len, file, offset)
    }

    pub fn map_anon(len: usize, _stack: bool, _populate: bool, _huge: Option<u8>, _no_reserve: bool) -> io::Result<MmapInner> {
        Ok(MmapInner {
            data: vec![0u8; len].into_boxed_slice(),
        })
    }

    pub fn flush(&self, _: usize, _: usize) -> io::Result<()> {
        Ok(())
    }

    pub fn flush_async(&self, _: usize, _: usize) -> io::Result<()> {
        Ok(())
    }

    pub fn make_read_only(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn make_exec(&mut self) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn make_mut(&mut self) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    pub fn ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    #[inline]
    pub fn mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }
}

pub fn file_len(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.len())
}
