#![cfg_attr(not(any(feature = "std", test, doc)), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(any(feature = "alloc", doc))]
extern crate alloc;

#[cfg(any(feature = "alloc", doc))]
use alloc::vec::Vec;

extern crate core;

use block_device::BlockDevice;

pub use crate::error::{GPTError, GPTParseError, GptRepair, Result};
use crate::header::{GPTHeader, GptHeaderType};

pub const DEFAULT_PARTTABLE_SIZE: u32 = 16384;
//pub const DEFAULT_PARTTABLE_BLOCKS: u32 = DEFAULT_PARTTABLE_SIZE / BLOCK_SIZE;

macro_rules! read_le_bytes {
    ($in:tt, $size:tt, $pos:expr) => {
        // TODO: remove unwrap
        $size::from_le_bytes(($in[$pos]).try_into().unwrap())
    };

    ($in:tt, $pos:expr) => {
        ($in[$pos]).try_into().unwrap()
    };
}
pub(crate) use read_le_bytes; // trick to export to crate

mod guid;

pub mod error;
pub mod header;
pub mod mbr;
pub mod part;
#[cfg(any(feature = "std", doc))]
pub mod std;

use crate::mbr::{MBRPartitionRecord, MasterBootRecord};
use crate::part::{GPTPartHeader, GPTTypeGuid};

#[doc(inline)]
pub use guid::GUID;

pub struct GPT<T> {
    block: T,
    header: GPTHeader,
}

impl<T> GPT<T>
where
    T: BlockDevice,
    GPTError: From<T::Error>,
{
    // This checks that the other header is okay, so cannot unwrap it in if header
    #[allow(clippy::unnecessary_unwrap)]
    pub fn open(block: T) -> Result<Self, GPTParseError<T>> {
        #[cfg(not(feature = "alloc"))]
        let mut buf = [0u8; DEFAULT_PARTTABLE_SIZE as usize];

        #[cfg(feature = "alloc")]
        let mut buf = {
            let mut buf = Vec::with_capacity(DEFAULT_PARTTABLE_SIZE as usize);
            buf.try_reserve_exact(DEFAULT_PARTTABLE_SIZE as usize)?; // Catch allocation errors
            buf.resize(DEFAULT_PARTTABLE_SIZE as usize, 0);
            buf
        };

        // TODO: read address from MBR
        block.read(&mut buf, 0, 1)?;
        let mbr = unsafe { MasterBootRecord::from_buf(&buf) }?;

        mbr.verify(None)?;
        if mbr.partition[0].os_indicator != MBRPartitionRecord::GPT_PROTECTIVE_OS_TYPE {
            // This is not a protective MBR, but a possible a MBR with one or more GPT partitions.
            // Bailing out
            return Err(GPTError::NoGPT.into());
        }

        let header_lba = mbr.partition[0].starting_lba() as usize;

        // mbr has to be clean up, before buf is used again, as mbr points into buf.
        drop(mbr);
        block.read(&mut buf, header_lba, 1)?;

        let m_header = GPTHeader::parse(&buf)?;

        let p_table_size = m_header.size_of_p_entry * m_header.num_parts;
        #[cfg(not(feature = "alloc"))]
        if p_table_size > DEFAULT_PARTTABLE_SIZE {
            return Err(GPTError::NoAllocator.into());
        }

        #[cfg(feature = "alloc")]
        if p_table_size > buf.len() as u32 {
            buf.try_reserve_exact(p_table_size as usize - buf.len())?; // Catch allocation errors
            buf.resize(p_table_size as usize, 0);
        }

        let blocks = ceil64(p_table_size as u64, T::BLOCK_SIZE as u64) as usize;

        block.read(&mut buf, m_header.p_entry_lba as usize, blocks as usize)?;

        let m_header_valid = m_header.validate(header_lba as u64, &buf);

        block.read(&mut buf, m_header.other_lba as usize, 1)?;
        let b_header = GPTHeader::parse(&buf)?;

        block.read(&mut buf, b_header.p_entry_lba as usize, blocks as usize)?;

        let b_header_valid = b_header.validate(m_header.other_lba as u64, &buf);

        if m_header_valid.is_err() || b_header_valid.is_err() {
            return if m_header_valid.is_ok() {
                Err(GPTParseError::BrokenHeader(
                    Self {
                        block,
                        header: m_header,
                    },
                    GptHeaderType::Backup,
                    b_header_valid.unwrap_err(),
                ))
            } else if b_header_valid.is_ok() {
                Err(GPTParseError::BrokenHeader(
                    Self {
                        block,
                        header: b_header,
                    },
                    GptHeaderType::Main,
                    m_header_valid.unwrap_err(),
                ))
            } else {
                Err(GPTError::NoGPT.into())
            };
        }

        Ok(Self {
            block,
            header: m_header,
        })
    }

    pub fn get_block(self) -> T {
        self.block
    }

    pub fn get_partition_buf<PT, PA>(&self, idx: u32, buf: &[u8]) -> Result<GPTPartHeader<PT, PA>>
    where
        PT: GPTTypeGuid,
        GPTError: From<<PT as TryFrom<[u8; 16]>>::Error>,
        GPTError: From<<PT as TryInto<[u8; 16]>>::Error>,
        PA: TryFrom<u64>,
        GPTError: From<<PA as TryFrom<u64>>::Error>,
    {
        if idx >= self.header.num_parts {
            return Err(GPTError::InvalidData);
        }

        // TODO: check size
        let offset: u64 = self.header.size_of_p_entry as u64 * idx as u64;

        GPTPartHeader::parse(&buf[offset as usize..])
    }

    pub fn get_partition<PT, PA>(&self, idx: u32) -> Result<GPTPartHeader<PT, PA>>
    where
        PT: GPTTypeGuid,
        GPTError: From<<PT as TryFrom<[u8; 16]>>::Error>,
        GPTError: From<<PT as TryInto<[u8; 16]>>::Error>,
        PA: TryFrom<u64>,
        GPTError: From<<PA as TryFrom<u64>>::Error>,
    {
        if idx >= self.header.num_parts {
            return Err(GPTError::InvalidData);
        }

        let p_table_size = self.header.size_of_p_entry as usize * self.header.num_parts as usize;

        let blocks = ceil64(p_table_size as u64, T::BLOCK_SIZE as u64) as usize;

        let buf = read_buf(
            self.header.p_entry_lba as usize,
            p_table_size,
            &self.block,
            blocks,
        )?;

        self.get_partition_buf(idx, &buf)
    }

    pub fn get_first_partition_of_type_buf<PT, PA>(
        &self,
        guid: PT,
        buf: &[u8],
    ) -> Result<GPTPartHeader<PT, PA>>
    where
        PT: GPTTypeGuid,
        GPTError: From<<PT as TryFrom<[u8; 16]>>::Error>,
        GPTError: From<<PT as TryInto<[u8; 16]>>::Error>,
        PA: TryFrom<u64>,
        GPTError: From<<PA as TryFrom<u64>>::Error>,
        PT: Eq,
    {
        let mut idx = 0;

        loop {
            let part = self.get_partition_buf(idx, buf)?;
            if part.type_guid == guid {
                return Ok(part);
            }

            idx += 1;
        }
    }

    pub fn get_first_partition_of_type<PT, PA>(&self, guid: PT) -> Result<GPTPartHeader<PT, PA>>
    where
        PT: GPTTypeGuid,
        GPTError: From<<PT as TryFrom<[u8; 16]>>::Error>,
        GPTError: From<<PT as TryInto<[u8; 16]>>::Error>,
        PA: TryFrom<u64>,
        GPTError: From<<PA as TryFrom<u64>>::Error>,
        PT: Eq,
    {
        let p_table_size = self.header.size_of_p_entry as usize * self.header.num_parts as usize;

        let blocks = ceil64(p_table_size as u64, T::BLOCK_SIZE as u64) as usize;

        let buf = read_buf(
            self.header.p_entry_lba as usize,
            p_table_size,
            &self.block,
            blocks,
        )?;

        self.get_first_partition_of_type_buf(guid, &buf)
    }
}

#[cfg(not(feature = "alloc"))]
fn read_buf<T: BlockDevice>(
    start_lba: usize,
    size: usize,
    block: &T,
    blocks: usize,
) -> Result<[u8; DEFAULT_PARTTABLE_SIZE as usize]>
where
    GPTError: From<T::Error>,
{
    let mut buf = [0u8; DEFAULT_PARTTABLE_SIZE as usize];
    if size > DEFAULT_PARTTABLE_SIZE as usize {
        return Err(GPTError::NoAllocator);
    }

    block.read(&mut buf, start_lba, blocks)?;

    Ok(buf)
}

#[cfg(feature = "alloc")]
fn read_buf<T: BlockDevice>(
    start_lba: usize,
    size: usize,
    block: &T,
    blocks: usize,
) -> Result<alloc::vec::Vec<u8>>
where
    GPTError: From<T::Error>,
{
    let mut buf = {
        let mut buf = Vec::with_capacity(DEFAULT_PARTTABLE_SIZE as usize);
        buf.try_reserve_exact(DEFAULT_PARTTABLE_SIZE as usize)?; // Catch allocation errors
        buf.resize(DEFAULT_PARTTABLE_SIZE as usize, 0);
        buf
    };

    if size > buf.len() {
        buf.try_reserve_exact(size - buf.len())?; // Catch allocation errors
        buf.resize(size, 0);
    }

    block.read(&mut buf, start_lba, blocks)?;

    Ok(buf)
}

/*fn ceil32(mut a: u32, b: u32) -> u32 {
    a += b - (a % b);
    a / b
}*/

fn ceil64(mut a: u64, b: u64) -> u64 {
    a += b - (a % b);
    a / b
}
