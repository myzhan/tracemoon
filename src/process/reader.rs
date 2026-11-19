//! Process memory reader abstraction

use crate::error::{Result, TracemoonError};
use std::mem;

/// Lowest address we consider a legitimate user-space mapping.
///
/// The first page is never mapped, so a smaller pointer is the signature of a
/// null/garbage field rather than something worth handing to the platform reader.
const MIN_REMOTE_ADDR: usize = 0x1000;

/// Highest address we will hand to a platform reader.
///
/// Sized to cover every real user-space layout (47-bit on x86_64, 48-bit on
/// aarch64 Linux — where the stack legitimately lives near `0xff_ffff_ffff`) while
/// still rejecting values that only make sense as corrupted/wrapped pointers.
const MAX_REMOTE_ADDR: usize = 0x00ff_ffff_ffff_ffff;

/// Trait for reading memory from a target process
///
/// Note: This trait is designed to be object-safe (dyn-compatible).
/// Generic methods are provided as standalone functions instead.
pub trait ProcessMemoryReader: Send + Sync {
    /// Read raw bytes from the target process
    fn read_memory(&self, addr: usize, len: usize) -> Result<Vec<u8>>;
}

/// Reject addresses that cannot be a real user-space mapping of `len` bytes.
///
/// Remote pointers are untrusted input: they are read out of a running process that is
/// concurrently mutating its own structures, so a field can hold a null, a stale, or a
/// half-written value. Doing this check once here means no downstream `addr + offset`
/// computation can turn into a wildly out-of-range read (or an overflow panic).
pub fn validate_addr(addr: usize, len: usize) -> Result<()> {
    let end = addr.checked_add(len).ok_or_else(|| bad_addr(addr))?;
    if addr < MIN_REMOTE_ADDR || end > MAX_REMOTE_ADDR {
        return Err(bad_addr(addr));
    }
    Ok(())
}

fn bad_addr(addr: usize) -> TracemoonError {
    TracemoonError::MemoryRead {
        addr,
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "address is not a plausible user-space mapping",
        ),
    }
}

/// Turn a platform read failure into the most specific error available.
///
/// A target that died surfaces as `ESRCH` from `process_vm_readv` / `mach_vm_read`.
/// Separating that from an ordinary torn read lets the sampling loop stop cleanly instead
/// of counting failed samples until the duration runs out.
pub fn map_read_error(addr: usize, source: std::io::Error) -> TracemoonError {
    if source.raw_os_error() == Some(libc::ESRCH) {
        TracemoonError::ProcessExited
    } else {
        TracemoonError::MemoryRead { addr, source }
    }
}

/// A fixed-width integer that can be reconstructed from arbitrary bytes.
///
/// This is deliberately a sealed list of integer primitives instead of a blanket
/// `T: Copy`: every bit pattern of these types is a valid value, so building one from
/// remote memory cannot trigger the validity/alignment requirements that a generic
/// transmute would silently violate (e.g. a `bool` holding 2, or a `Vec<u8>` buffer
/// whose alignment guarantee is only 1).
pub trait Pod: Copy + Sized {
    /// Size of the type in bytes.
    const SIZE: usize;

    /// Build a value from exactly `mem::size_of::<Self>()` native-endian bytes.
    fn from_ne_bytes(bytes: &[u8]) -> Self;
}

macro_rules! impl_pod {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Pod for $ty {
                const SIZE: usize = mem::size_of::<$ty>();

                fn from_ne_bytes(bytes: &[u8]) -> Self {
                    let mut buf = [0u8; mem::size_of::<$ty>()];
                    buf.copy_from_slice(&bytes[..mem::size_of::<$ty>()]);
                    <$ty>::from_ne_bytes(buf)
                }
            }
        )*
    };
}

impl_pod!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

/// Read a fixed-width integer from the target process.
///
/// Reads are native-endian byte order and unaligned-safe (no pointer cast), so this is
/// sound for any address the platform reader can serve.
pub fn read_struct<T: Pod>(reader: &dyn ProcessMemoryReader, addr: usize) -> Result<T> {
    validate_addr(addr, T::SIZE)?;
    let bytes = reader.read_memory(addr, T::SIZE)?;

    if bytes.len() < T::SIZE {
        return Err(TracemoonError::MemoryRead {
            addr,
            source: std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("Expected {} bytes, got {}", T::SIZE, bytes.len()),
            ),
        });
    }

    Ok(T::from_ne_bytes(&bytes))
}

/// Upper bound for [`read_string`], so a corrupt length field cannot turn into a huge read.
pub const MAX_CSTRING_LEN: usize = 4096;

/// Read a null-terminated string from memory (up to max_len bytes)
pub fn read_string(
    reader: &dyn ProcessMemoryReader,
    addr: usize,
    max_len: usize,
) -> Result<String> {
    let max_len = max_len.min(MAX_CSTRING_LEN);
    validate_addr(addr, max_len)?;
    let bytes = reader.read_memory(addr, max_len)?;

    // Find null terminator
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());

    String::from_utf8(bytes[..len].to_vec())
        .map_err(|e| TracemoonError::Other(format!("Invalid UTF-8: {}", e)))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// In-memory reader backed by a byte window placed at `base`.
    ///
    /// Tests can exercise the read helpers and the layout-driven walkers without a live
    /// target process: `write_*` fills the window through the same byte offsets the remote
    /// reads use, so a test builds its structure image as flat memory.
    pub(crate) struct WindowReader {
        base: usize,
        bytes: Vec<u8>,
    }

    impl WindowReader {
        pub(crate) fn new(base: usize, len: usize) -> Self {
            Self {
                base,
                bytes: vec![0u8; len],
            }
        }

        /// Copy `data` to `addr`. Panics when it does not fit the window — that is a bug in
        /// the test's image, not a condition the code under test should handle.
        pub(crate) fn write(&mut self, addr: usize, data: &[u8]) {
            let start = addr
                .checked_sub(self.base)
                .expect("write outside the window");
            self.bytes[start..start + data.len()].copy_from_slice(data);
        }

        pub(crate) fn write_u8(&mut self, addr: usize, value: u8) {
            self.write(addr, &[value]);
        }

        pub(crate) fn write_i32(&mut self, addr: usize, value: i32) {
            self.write(addr, &value.to_ne_bytes());
        }

        pub(crate) fn write_u64(&mut self, addr: usize, value: u64) {
            self.write(addr, &value.to_ne_bytes());
        }
    }

    impl ProcessMemoryReader for WindowReader {
        fn read_memory(&self, addr: usize, len: usize) -> Result<Vec<u8>> {
            let start = addr
                .checked_sub(self.base)
                .ok_or_else(|| TracemoonError::Other("below window".to_string()))?;
            let end = start
                .checked_add(len)
                .ok_or_else(|| TracemoonError::Other("overflow".to_string()))?;
            if end > self.bytes.len() {
                return Err(TracemoonError::Other("past end of window".to_string()));
            }
            Ok(self.bytes[start..end].to_vec())
        }
    }

    /// Reader that always answers with fewer bytes than requested, which is what a
    /// platform read straddling an unmapped page looks like.
    pub(crate) struct ShortReader(pub(crate) usize);

    impl ProcessMemoryReader for ShortReader {
        fn read_memory(&self, _addr: usize, _len: usize) -> Result<Vec<u8>> {
            Ok(vec![0u8; self.0])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{ShortReader, WindowReader};
    use super::*;

    const BASE: usize = 0x1000;

    #[test]
    fn test_read_struct_unaligned_matches_native_endian() {
        // Deliberately odd offset: the old pointer-cast implementation relied on the
        // Vec<u8> allocation happening to be aligned, which is not guaranteed.
        let mut reader = WindowReader::new(BASE, 32);
        reader.write_u64(BASE + 1, 0x1122_3344_5566_7788);

        let value = read_struct::<u64>(&reader, BASE + 1).unwrap();
        assert_eq!(value, 0x1122_3344_5566_7788);
    }

    #[test]
    fn test_read_struct_rejects_null_and_wild_pointers() {
        let reader = WindowReader::new(BASE, 32);

        // Null / tiny addresses are never real mappings.
        assert!(read_struct::<u64>(&reader, 0).is_err());
        assert!(read_struct::<u8>(&reader, 8).is_err());
        // Kernel-space / wrapped pointers must not panic on `addr + size`.
        assert!(read_struct::<u64>(&reader, usize::MAX - 3).is_err());
    }

    #[test]
    fn test_read_struct_short_read_is_an_error() {
        // The platform reader can serve fewer bytes than asked; that must not be silently
        // zero-extended into a plausible-looking value.
        let err = read_struct::<u64>(&ShortReader(4), BASE).unwrap_err();
        assert!(matches!(err, TracemoonError::MemoryRead { .. }));
    }

    #[test]
    fn test_read_string_bounds() {
        let mut reader = WindowReader::new(BASE, 16);
        reader.write(BASE, b"lua\0");

        assert_eq!(read_string(&reader, BASE, 16).unwrap(), "lua");
        assert!(read_string(&reader, BASE, MAX_CSTRING_LEN * 4).is_err());
    }

    #[test]
    fn test_validate_addr_accepts_48bit_user_space() {
        // aarch64 Linux keeps the stack near the top of the 48-bit user range.
        assert!(validate_addr(0x0000_ffff_ffff_0000, 8).is_ok());
        assert!(validate_addr(0x0000_8000_0000_0000, 8).is_ok());
        // …but kernel space and wrapped pointers stay rejected.
        assert!(validate_addr(0xffff_8000_0000_0000, 8).is_err());
        assert!(validate_addr(0x0100_0000_0000_0000, 8).is_err());
    }
}
