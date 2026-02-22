use std::io::{self, Read, Write};

/// Default buffer size: 64KB
const DEFAULT_BUFFER_SIZE: usize = 64 * 1024;

/// A pre-allocated, reusable buffer for network I/O.
///
/// This buffer is designed for zero-allocation in the hot path:
/// - Fixed capacity, never reallocates
/// - Tracks read/write positions separately
/// - Supports compact operation to reclaim consumed space
#[derive(Debug)]
pub struct Buffer {
    data: Box<[u8]>,
    /// Read position: data before this has been consumed
    read_pos: usize,
    /// Write position: data has been written up to here
    write_pos: usize,
}

impl Buffer {
    /// Create a new buffer with the default size (64KB).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUFFER_SIZE)
    }

    /// Create a new buffer with the specified capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: vec![0u8; capacity].into_boxed_slice(),
            read_pos: 0,
            write_pos: 0,
        }
    }

    /// Returns the total capacity of the buffer.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Returns the number of bytes available to read.
    #[inline]
    pub fn readable(&self) -> usize {
        self.write_pos - self.read_pos
    }

    /// Returns the number of bytes available to write.
    #[inline]
    pub fn writable(&self) -> usize {
        self.data.len() - self.write_pos
    }

    /// Returns true if there is no data to read.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.read_pos == self.write_pos
    }

    /// Returns a slice of the readable data.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.data[self.read_pos..self.write_pos]
    }

    /// Returns a mutable slice of the writable space.
    #[inline]
    pub fn spare_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.write_pos..]
    }

    /// Advances the read position, consuming data.
    ///
    /// # Panics
    /// Panics if `n` exceeds the readable bytes.
    #[inline]
    pub fn consume(&mut self, n: usize) {
        assert!(n <= self.readable(), "consume exceeds readable bytes");
        self.read_pos += n;

        // If we've consumed everything, reset to start
        if self.read_pos == self.write_pos {
            self.read_pos = 0;
            self.write_pos = 0;
        }
    }

    /// Advances the write position after writing data.
    ///
    /// # Panics
    /// Panics if `n` exceeds the writable space.
    #[inline]
    pub fn advance(&mut self, n: usize) {
        assert!(n <= self.writable(), "advance exceeds writable space");
        self.write_pos += n;
    }

    /// Compacts the buffer by moving unread data to the start.
    ///
    /// This reclaims space from consumed data without allocating.
    pub fn compact(&mut self) {
        if self.read_pos == 0 {
            return;
        }

        let readable = self.readable();
        if readable > 0 {
            self.data.copy_within(self.read_pos..self.write_pos, 0);
        }
        self.read_pos = 0;
        self.write_pos = readable;
    }

    /// Clears the buffer, resetting both positions.
    #[inline]
    pub fn clear(&mut self) {
        self.read_pos = 0;
        self.write_pos = 0;
    }

    /// Grows the buffer to ensure at least `needed` bytes of writable space.
    ///
    /// This allocates a new buffer with sufficient capacity, copies existing
    /// data, and replaces the old buffer. The new capacity is the maximum of:
    /// - Current capacity * 2 (to avoid frequent reallocations)
    /// - Current readable data + needed bytes
    pub fn grow(&mut self, needed: usize) {
        let readable = self.readable();
        let min_capacity = readable + needed;
        let new_capacity = (self.capacity() * 2).max(min_capacity);

        let mut new_data = vec![0u8; new_capacity].into_boxed_slice();

        // Copy existing readable data to the start of the new buffer
        if readable > 0 {
            new_data[..readable].copy_from_slice(&self.data[self.read_pos..self.write_pos]);
        }

        self.data = new_data;
        self.read_pos = 0;
        self.write_pos = readable;
    }

    /// Reads from the given reader into the buffer.
    ///
    /// Returns the number of bytes read, or an error.
    /// If the buffer is full, compacts first to make space.
    pub fn read_from<R: Read>(&mut self, reader: &mut R) -> io::Result<usize> {
        // Compact if we're running low on writable space
        if self.writable() < self.capacity() / 4 {
            self.compact();
        }

        if self.writable() == 0 {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, "buffer is full"));
        }

        let n = reader.read(self.spare_mut())?;
        self.advance(n);
        Ok(n)
    }

    /// Writes from the buffer to the given writer.
    ///
    /// Returns the number of bytes written, or an error.
    pub fn write_to<W: Write>(&mut self, writer: &mut W) -> io::Result<usize> {
        if self.is_empty() {
            return Ok(0);
        }

        let n = writer.write(self.as_slice())?;
        self.consume(n);
        Ok(n)
    }

    /// Extends the buffer with the given data.
    ///
    /// # Panics
    /// Panics if the data exceeds the writable space.
    pub fn extend_from_slice(&mut self, data: &[u8]) {
        assert!(
            data.len() <= self.writable(),
            "extend_from_slice exceeds writable space"
        );
        self.spare_mut()[..data.len()].copy_from_slice(data);
        self.advance(data.len());
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

/// A pair of buffers for bidirectional I/O.
#[derive(Debug)]
pub struct BufferPair {
    pub send: Buffer,
    pub recv: Buffer,
}

impl BufferPair {
    pub fn new() -> Self {
        Self {
            send: Buffer::new(),
            recv: Buffer::new(),
        }
    }

    pub fn with_capacity(send_cap: usize, recv_cap: usize) -> Self {
        Self {
            send: Buffer::with_capacity(send_cap),
            recv: Buffer::with_capacity(recv_cap),
        }
    }
}

impl Default for BufferPair {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_basic() {
        let mut buf = Buffer::with_capacity(1024);

        assert_eq!(buf.capacity(), 1024);
        assert_eq!(buf.readable(), 0);
        assert_eq!(buf.writable(), 1024);
        assert!(buf.is_empty());

        buf.extend_from_slice(b"hello");
        assert_eq!(buf.readable(), 5);
        assert_eq!(buf.as_slice(), b"hello");

        buf.consume(2);
        assert_eq!(buf.readable(), 3);
        assert_eq!(buf.as_slice(), b"llo");
    }

    #[test]
    fn test_buffer_compact() {
        let mut buf = Buffer::with_capacity(16);

        buf.extend_from_slice(b"hello world!");
        assert_eq!(buf.writable(), 4);

        buf.consume(6);
        assert_eq!(buf.as_slice(), b"world!");

        buf.compact();
        assert_eq!(buf.as_slice(), b"world!");
        assert_eq!(buf.writable(), 10);
    }

    #[test]
    fn test_buffer_consume_all_resets() {
        let mut buf = Buffer::with_capacity(1024);

        buf.extend_from_slice(b"test");
        buf.consume(4);

        // Should auto-reset when all data consumed
        assert_eq!(buf.writable(), 1024);
    }
}
