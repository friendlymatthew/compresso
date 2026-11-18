use anyhow::{Result, bail, ensure};

use crate::{
    deflate,
    utils::impl_read::{impl_read_for_datatype_le, impl_read_slice},
};

pub struct ZipReader<'a> {
    data: &'a [u8],

    directory: EndOfCentralDirectory<'a>,
}

impl<'a> ZipReader<'a> {
    pub fn try_new(data: &'a [u8]) -> Result<Self> {
        let c = Self::find_end_of_central_directory(data)?;
        let directory = EndOfCentralDirectoryReader::new(data, c).read()?;
        let directory_start = usize::try_from(directory.central_directory_offset)?;

        Ok(Self {
            data: &data[..directory_start],
            directory,
        })
    }

    fn find_end_of_central_directory(data: &'a [u8]) -> Result<usize> {
        const FIXED_SIZE: usize = 22;
        const MAX_SIZE: usize = 22 + u16::MAX as usize;

        let len = data.len();
        let last_start = len
            .checked_sub(FIXED_SIZE)
            .ok_or_else(|| anyhow::anyhow!("ZIP input is too short for an eocd"))?;

        let first_start = len.saturating_sub(MAX_SIZE);

        for offset in (first_start..=last_start).rev() {
            let record = &data[offset..offset + FIXED_SIZE];
            if &record[..4] != b"PK\x05\x06" {
                continue;
            }

            let comment_len = usize::from(u16::from_le_bytes([record[20], record[21]]));
            if comment_len == len - offset - FIXED_SIZE {
                return Ok(offset);
            }
        }

        bail!("ZIP end of central directory not found")
    }
}

struct ZipLocalFileReader<'a> {
    cursor: usize,
    data: &'a [u8],
}

impl<'a> ZipLocalFileReader<'a> {
    fn read_local_file_header(&mut self) -> Result<ZipLocalFileHeader<'a>> {
        self.read_slice_expect(b"PK\x03\x04")?;

        let mut h = ZipLocalFileHeader {
            extract_version: self.read_u16()?,
            flags: ZipFlags(self.read_u16()?),
            compression_method: ZipCompressionMethod::from(self.read_u16()?),
            last_mod_file_time: self.read_u16()?,
            last_mod_file_date: self.read_u16()?,
            crc32: self.read_u32()?,
            compressed_size: self.read_u32()?,
            uncompressed_size: self.read_u32()?,
            file_name: &[],
            extra_field: &[],
        };

        let file_name_length = self.read_u16()?;
        let extra_field_length = self.read_u16()?;

        h.file_name = self.read_slice(file_name_length as usize)?;
        h.extra_field = self.read_slice(extra_field_length as usize)?;

        Ok(h)
    }

    impl_read_slice!();

    impl_read_for_datatype_le!(read_u8, u8);
    impl_read_for_datatype_le!(read_u16, u16);
    impl_read_for_datatype_le!(read_u32, u32);
}

impl<'a, 'reader> IntoIterator for &'reader ZipReader<'a> {
    type Item = ZipEntry<'reader, 'a>;
    type IntoIter = ZipEntries<'reader, 'a>;

    fn into_iter(self) -> Self::IntoIter {
        ZipEntries {
            data: self.data,
            entries: self.directory.entries.iter(),
        }
    }
}

pub struct ZipEntries<'reader, 'a> {
    data: &'a [u8],
    entries: std::slice::Iter<'reader, ZipCentralDirectoryEntry<'a>>,
}

impl<'reader, 'a> Iterator for ZipEntries<'reader, 'a> {
    type Item = ZipEntry<'reader, 'a>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(ZipEntry {
            data: self.data,
            metadata: self.entries.next()?,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl DoubleEndedIterator for ZipEntries<'_, '_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        Some(ZipEntry {
            data: self.data,
            metadata: self.entries.next_back()?,
        })
    }
}

impl ExactSizeIterator for ZipEntries<'_, '_> {}
impl std::iter::FusedIterator for ZipEntries<'_, '_> {}

pub struct ZipEntry<'reader, 'a> {
    data: &'a [u8],
    metadata: &'reader ZipCentralDirectoryEntry<'a>,
}

impl<'reader, 'a> ZipEntry<'reader, 'a> {
    pub const fn name(&self) -> &'a [u8] {
        self.metadata.file_name
    }

    pub const fn metadata(&self) -> &'reader ZipCentralDirectoryEntry<'a> {
        self.metadata
    }

    /// this is just a borrowed slice of the entry payload as it appears in the zip
    ///
    /// use [`Self::read`] to get the uncompressed bytes
    pub fn compressed_data(&self) -> Result<&'a [u8]> {
        let entry = self.metadata;

        ensure!(entry.disk_number_start == 0,);
        ensure!(!entry.flags.is_encrypted(),);
        ensure!(
            entry.local_header_offset != u32::MAX
                && entry.compressed_size != u32::MAX
                && entry.uncompressed_size != u32::MAX,
        );

        let mut reader = ZipLocalFileReader {
            cursor: usize::try_from(entry.local_header_offset)?,
            data: self.data,
        };

        let local = reader.read_local_file_header()?;

        ensure!(
            !local.flags.is_encrypted(),
            "encrypted ZIP entry unsupported"
        );

        ensure!(
            local.file_name == entry.file_name,
            "ZIP entry name mismatch"
        );

        ensure!(
            local.compression_method == entry.compression_method,
            "ZIP entry compression method mismatch"
        );

        reader.read_slice(usize::try_from(entry.compressed_size)?)
    }

    pub fn read(&self) -> Result<Vec<u8>> {
        let entry = self.metadata;
        let payload = self.compressed_data()?;
        let expected_size = usize::try_from(entry.uncompressed_size)?;

        // todo: add a per entry decompression limit
        // basically reject zip entries whose declared size exceeds the callers limit and
        // stop deflate before any write would exceed the declared size
        //
        // then we can verify the ifnal size and crc32 after decoding
        let output = match entry.compression_method {
            ZipCompressionMethod::Stored => {
                ensure!(
                    payload.len() == expected_size,
                    "ZIP uncompressed size mismatch"
                );

                payload.to_vec()
            }
            ZipCompressionMethod::Deflate => deflate::inflate(payload)?,
            ZipCompressionMethod::Unsupported(method) => {
                bail!("unsupported ZIP compression method {method}");
            }
        };

        ensure!(
            output.len() == expected_size,
            "ZIP uncompressed size mismatch"
        );

        // todo: what does crc32 look like
        // ensure!(crc32(&output) == entry.crc32, "ZIP CRC32 mismatch");

        Ok(output)
    }
}

struct EndOfCentralDirectoryReader<'a> {
    cursor: usize,
    data: &'a [u8],
}

impl<'a> EndOfCentralDirectoryReader<'a> {
    fn new(data: &'a [u8], cursor: usize) -> Self {
        Self { cursor, data }
    }

    fn read(&mut self) -> Result<EndOfCentralDirectory<'a>> {
        let eocd_offset = self.cursor;
        self.read_slice_expect(b"PK\x05\x06")?;

        let mut directory = EndOfCentralDirectory {
            disk_number: self.read_u16()?,
            central_directory_start_disk: self.read_u16()?,
            entries_on_disk: self.read_u16()?,
            total_entries: self.read_u16()?,
            central_directory_size: self.read_u32()?,
            central_directory_offset: self.read_u32()?,
            comment: {
                let len = self.read_u16()?;
                self.read_slice(len as usize)?
            },
            entries: Vec::new(),
        };

        ensure!(
            directory.disk_number == 0 && directory.central_directory_start_disk == 0,
            "multi disk zip archives are unsupported"
        );

        ensure!(
            directory.entries_on_disk == directory.total_entries,
            "zip central dir entry counts disagree"
        );

        let start = usize::try_from(directory.central_directory_offset)?;
        let size = usize::try_from(directory.central_directory_size)?;

        let end = start
            .checked_add(size)
            .filter(|&end| end <= eocd_offset)
            .ok_or_else(|| anyhow::anyhow!("zip central dir is oob"))?;

        let count = usize::from(directory.total_entries);

        ensure!(
            count <= size / 46,
            "ZIP central directory is too short for its entry count"
        );

        // this is just for extra safety, this way we never exceed from the start..end range
        let mut reader = Self::new(&self.data[start..end], 0);
        directory.entries = reader.read_vec(count, Self::read_entry)?;

        Ok(directory)
    }

    fn read_entry(&mut self) -> Result<ZipCentralDirectoryEntry<'a>> {
        self.read_slice_expect(b"PK\x01\x02")?;

        let version_made_by = self.read_u16()?;
        let extract_version = self.read_u16()?;
        let flags = ZipFlags(self.read_u16()?);
        let compression_method = ZipCompressionMethod::from(self.read_u16()?);
        let last_mod_file_time = self.read_u16()?;
        let last_mod_file_date = self.read_u16()?;
        let crc32 = self.read_u32()?;
        let compressed_size = self.read_u32()?;
        let uncompressed_size = self.read_u32()?;
        let file_name_length = usize::from(self.read_u16()?);
        let extra_field_length = usize::from(self.read_u16()?);
        let comment_length = usize::from(self.read_u16()?);

        Ok(ZipCentralDirectoryEntry {
            version_made_by,
            extract_version,
            flags,
            compression_method,
            last_mod_file_time,
            last_mod_file_date,
            crc32,
            compressed_size,
            uncompressed_size,
            disk_number_start: self.read_u16()?,
            internal_file_attributes: self.read_u16()?,
            external_file_attributes: self.read_u32()?,
            local_header_offset: self.read_u32()?,
            file_name: self.read_slice(file_name_length)?,
            extra_field: self.read_slice(extra_field_length)?,
            comment: self.read_slice(comment_length)?,
        })
    }

    impl_read_slice!();

    impl_read_for_datatype_le!(read_u8, u8);
    impl_read_for_datatype_le!(read_u16, u16);
    impl_read_for_datatype_le!(read_u32, u32);
}

struct ZipLocalFileHeader<'a> {
    extract_version: u16,
    flags: ZipFlags,
    compression_method: ZipCompressionMethod,
    last_mod_file_time: u16,
    last_mod_file_date: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    file_name: &'a [u8],
    extra_field: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZipFlags(u16);

impl ZipFlags {
    pub const fn is_encrypted(self) -> bool {
        self.0 & (1 << 0) != 0
    }

    pub const fn has_data_descriptor(self) -> bool {
        self.0 & (1 << 3) != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZipCompressionMethod {
    Stored,
    Deflate,
    Unsupported(u16),
}

impl From<u16> for ZipCompressionMethod {
    fn from(code: u16) -> Self {
        match code {
            0 => Self::Stored,
            8 => Self::Deflate,
            code => Self::Unsupported(code),
        }
    }
}

#[derive(Debug)]
struct EndOfCentralDirectory<'a> {
    disk_number: u16,
    central_directory_start_disk: u16,
    entries_on_disk: u16,
    total_entries: u16,
    central_directory_size: u32,
    central_directory_offset: u32,
    comment: &'a [u8],
    entries: Vec<ZipCentralDirectoryEntry<'a>>,
}

#[derive(Debug)]
pub struct ZipCentralDirectoryEntry<'a> {
    pub version_made_by: u16,
    pub extract_version: u16,
    pub flags: ZipFlags,
    pub compression_method: ZipCompressionMethod,
    pub last_mod_file_time: u16,
    pub last_mod_file_date: u16,
    pub crc32: u32,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub disk_number_start: u16,
    pub internal_file_attributes: u16,
    pub external_file_attributes: u32,
    pub local_header_offset: u32,
    pub file_name: &'a [u8],
    pub extra_field: &'a [u8],
    pub comment: &'a [u8],
}
