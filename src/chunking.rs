//! Storage chunk boundaries. Logical file hashes never depend on this module.
use anyhow::{Result, ensure};
use clap::ValueEnum;
use fastcdc::v2020::{Normalization, StreamCDC};
use std::io::{self, Read};

pub const MAX_CHUNK: usize = 4 * 1024 * 1024;
pub const DEFAULT_CHUNK: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum Chunker {
    /// Fixed-size blocks; preserves existing capture behavior.
    #[default]
    Fixed,
    /// FastCDC 2020: min=target/4, max=target*4, normalization level 1, seed 0.
    #[value(name = "fastcdc")]
    FastCdc,
}
impl Chunker {
    pub fn validate(self, size: usize) -> Result<()> {
        ensure!(
            (4096..=MAX_CHUNK).contains(&size),
            "chunk size must be 4096–4194304 bytes"
        );
        if self == Self::FastCdc {
            ensure!(
                size.is_power_of_two() && size <= DEFAULT_CHUNK,
                "FastCDC target must be a power of two from 4096 to 1048576 bytes (maximum chunk is four times target)"
            );
        }
        Ok(())
    }
    pub fn chunks<R: Read>(self, source: R, size: usize) -> Result<Chunks<R>> {
        self.chunks_for_file(source, size, None)
    }
    pub fn chunks_for_file<R: Read>(
        self,
        source: R,
        size: usize,
        length: Option<u64>,
    ) -> Result<Chunks<R>> {
        self.validate(size)?;
        // Files no larger than the minimum have exactly one chunk. Avoid a
        // max-sized CDC buffer for them; never trust the hint as an EOF boundary.
        let tiny = self == Self::FastCdc && length.is_some_and(|n| n <= (size / 4) as u64);
        let reader = if self == Self::Fixed || tiny {
            Kind::Fixed {
                source: RetryRead(source),
                size: if tiny { size / 4 } else { size },
            }
        } else {
            Kind::Fast(Box::new(StreamCDC::with_level_and_seed(
                RetryRead(source),
                size / 4,
                size,
                size * 4,
                Normalization::Level1,
                0,
            )))
        };
        Ok(Chunks {
            reader,
            finished: false,
        })
    }
}

// StreamCDC propagates Interrupted; retry it at the reader boundary without
// restarting chunk state. Real I/O failures remain fatal to this capture.
struct RetryRead<R>(R);
impl<R: Read> Read for RetryRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
    }
}
enum Kind<R: Read> {
    Fixed { source: RetryRead<R>, size: usize },
    Fast(Box<StreamCDC<RetryRead<R>>>),
}
pub struct Chunks<R: Read> {
    reader: Kind<R>,
    finished: bool,
}
impl<R: Read> Iterator for Chunks<R> {
    type Item = io::Result<Vec<u8>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let result = match &mut self.reader {
            Kind::Fast(chunker) => chunker
                .next()
                .map(|chunk| chunk.map(|c| c.data).map_err(io::Error::from)),
            Kind::Fixed { source, size } => {
                let mut buffer = vec![0; *size];
                let mut n = 0;
                while n < buffer.len() {
                    match source.read(&mut buffer[n..]) {
                        Ok(0) => break,
                        Ok(got) => n += got,
                        Err(error) => {
                            self.finished = true;
                            return Some(Err(error));
                        }
                    }
                }
                buffer.truncate(n);
                (n != 0).then_some(Ok(buffer))
            }
        };
        if result.as_ref().is_none_or(|r| r.is_err()) {
            self.finished = true;
        }
        result
    }
}
impl<R: Read> std::iter::FusedIterator for Chunks<R> {}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngCore, SeedableRng};
    use std::io::Cursor;
    struct Fragmented {
        data: Cursor<Vec<u8>>,
        interrupt: bool,
        limit: usize,
    }
    impl Read for Fragmented {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.interrupt = !self.interrupt;
            if self.interrupt {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = buffer.len().min(self.limit);
            self.data.read(&mut buffer[..n])
        }
    }
    fn data() -> Vec<u8> {
        let mut data = vec![0; 1024 * 1024 + 37];
        rand::rngs::StdRng::seed_from_u64(193).fill_bytes(&mut data);
        data
    }
    #[test]
    fn boundaries_ignore_short_reads_and_interrupted_syscalls() -> Result<()> {
        let data = data();
        for mode in [Chunker::Fixed, Chunker::FastCdc] {
            let expected = mode
                .chunks(data.as_slice(), 16384)?
                .collect::<io::Result<Vec<_>>>()?;
            let actual = mode
                .chunks(
                    Fragmented {
                        data: Cursor::new(data.clone()),
                        interrupt: false,
                        limit: 37,
                    },
                    16384,
                )?
                .collect::<io::Result<Vec<_>>>()?;
            assert_eq!(actual, expected);
            assert_eq!(actual.concat(), data);
            assert!(actual.iter().all(|c| !c.is_empty() && c.len() <= 65536));
        }
        Ok(())
    }
    #[test]
    fn fastcdc_matches_upstream_and_tiny_file_optimization() -> Result<()> {
        let data = data();
        let expected: Vec<_> = fastcdc::v2020::FastCDC::new(&data, 4096, 16384, 65536)
            .map(|c| c.length)
            .collect();
        let actual: Vec<_> = Chunker::FastCdc
            .chunks(data.as_slice(), 16384)?
            .map(|r| r.map(|c| c.len()))
            .collect::<io::Result<_>>()?;
        assert_eq!(actual, expected);
        for n in [0, 1, 4095, 4096] {
            let tiny = Chunker::FastCdc
                .chunks_for_file(&data[..n], 16384, Some(n as u64))?
                .collect::<io::Result<Vec<_>>>()?;
            let reference = Chunker::FastCdc
                .chunks(&data[..n], 16384)?
                .collect::<io::Result<Vec<_>>>()?;
            assert_eq!(tiny, reference);
        }
        // An inaccurate length hint must never silently discard trailing bytes.
        assert_eq!(
            Chunker::FastCdc
                .chunks_for_file(data.as_slice(), 16384, Some(1))?
                .collect::<io::Result<Vec<_>>>()?
                .concat(),
            data
        );
        Ok(())
    }
    #[test]
    fn invalid_sizes_fail_without_panicking_or_allocating() {
        for size in [0, 4095, MAX_CHUNK + 1, usize::MAX] {
            assert!(Chunker::Fixed.chunks(io::empty(), size).is_err());
            assert!(Chunker::FastCdc.chunks(io::empty(), size).is_err());
        }
        for size in [4097, 1024 * 1024 + 1, MAX_CHUNK] {
            assert!(Chunker::FastCdc.chunks(io::empty(), size).is_err());
        }
    }
    #[test]
    fn real_read_errors_terminate_both_iterators() -> Result<()> {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::PermissionDenied.into())
            }
        }
        for mode in [Chunker::Fixed, Chunker::FastCdc] {
            let mut chunks = mode.chunks(Broken, 4096)?;
            assert_eq!(
                chunks.next().unwrap().unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert!(chunks.next().is_none());
        }
        Ok(())
    }
}
