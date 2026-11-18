use std::fmt::Debug;

use anyhow::{Result, ensure};

use crate::{
    deflate,
    utils::impl_read::{impl_read_for_datatype_le, impl_read_slice},
};

#[derive(Debug)]
pub struct GZipHeader {
    compression_method: u8,
    flag: u8,
    modification_time: u32,
    extra_flags: u8,
    os: u8,
}

impl GZipHeader {
    pub const fn is_text(&self) -> bool {
        self.flag & 1 != 0
    }

    pub const fn has_header_crc(&self) -> bool {
        self.flag & (1 << 1) != 0
    }

    pub const fn has_extra(&self) -> bool {
        self.flag & (1 << 2) != 0
    }

    pub const fn has_name(&self) -> bool {
        self.flag & (1 << 3) != 0
    }

    pub const fn has_comment(&self) -> bool {
        self.flag & (1 << 4) != 0
    }

    pub const fn has_reserved_bits(&self) -> bool {
        self.flag & 0b1110_0000 != 0
    }
}

pub struct GZipReader<'a> {
    cursor: usize,
    data: &'a [u8],
}

impl<'a> GZipReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { cursor: 0, data }
    }

    pub fn read(&mut self) -> Result<Vec<u8>> {
        let header = self.consume_header()?;

        // note: the header parsing will enforce the compression method is set to deflate

        if header.has_extra() {
            let xlen = self.read_u16()?;
            let _extra_field = self.read_slice(xlen as usize)?;
        }

        if header.has_name() {
            let _file_name = self.read_slice_until(0)?;
        }

        if header.has_comment() {
            let _file_comment = self.read_slice_until(0)?;
        }

        if header.has_header_crc() {
            let _crc = self.read_u16()?;
        }

        let remain = self.data.len() - self.cursor;
        ensure!(remain >= 8);

        let compressed_blocks = self.read_slice(remain - 8)?;

        let _crc32 = self.read_u32()?;
        let _isize = self.read_u32()?;

        deflate::inflate(compressed_blocks)
    }

    pub fn read_header(&self) -> Result<GZipHeader> {
        Self::new(self.data).consume_header()
    }

    fn consume_header(&mut self) -> Result<GZipHeader> {
        debug_assert!(self.cursor == 0, "cursor is not properly set at 0");

        self.read_array_expect::<1>([31])?;
        self.read_array_expect::<1>([139])?;

        let h = GZipHeader {
            compression_method: {
                let c = self.read_u8()?;
                ensure!(c == 8, "compression method is not DEFLATE");
                c
            },
            flag: self.read_u8()?,
            modification_time: self.read_u32()?,
            extra_flags: self.read_u8()?,
            os: self.read_u8()?,
        };

        ensure!(!h.has_reserved_bits());

        Ok(h)
    }

    impl_read_slice!();
    impl_read_for_datatype_le!(read_u8, u8);
    impl_read_for_datatype_le!(read_u16, u16);
    impl_read_for_datatype_le!(read_u32, u32);
}

#[cfg(test)]
mod tests {
    use super::GZipReader;

    #[test]
    fn reads_payload_after_inspecting_header() {
        let data = [
            // Fixed gzip header with FTEXT set.
            0x1f, 0x8b, 8, 1, 0, 0, 0, 0, 0, 255,
            // Final stored DEFLATE block containing 'a', followed by the trailer.
            1, 1, 0, 0xfe, 0xff, b'a', 0x43, 0xbe, 0xb7, 0xe8, 1, 0, 0, 0,
        ];
        let mut reader = GZipReader::new(&data);

        assert!(reader.read_header().unwrap().is_text());
        assert_eq!(reader.read().unwrap(), b"a");
    }
}
