use std::path::Path;
use std::{fmt, fs};

use anyhow::{Context, Result, bail};
use ihex::Record;
use object::Endianness;
use object::elf::{PT_LOAD, SHF_ALLOC, SHT_NOBITS};
use object::read::elf::{ElfFile32, ProgramHeader, SectionHeader};
use sha2::{Digest, Sha256};

pub(crate) struct Application {
    start: u32,
    data: Vec<u8>,
}

impl Application {
    pub(crate) fn from_elf(path: &Path) -> Result<Self> {
        let file = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let elf = ElfFile32::<Endianness>::parse(&*file)
            .with_context(|| format!("failed to parse {} as 32-bit ELF", path.display()))?;
        let endian = elf.endian();

        let mut memory = Memory::default();
        for segment in elf.elf_program_headers() {
            if segment.p_type(endian) != PT_LOAD {
                continue;
            }

            let start = segment.p_offset(endian);
            let end = start + segment.p_filesz(endian);
            for section in elf.elf_section_table().iter() {
                if !section.sh_flags(endian).contains(SHF_ALLOC)
                    || section.sh_type(endian) == SHT_NOBITS
                {
                    continue;
                }

                let offset = section.sh_offset(endian);
                let size = section.sh_size(endian);
                if size == 0 || offset < start || offset + size > end {
                    continue;
                }

                let data = section
                    .data(endian, &*file)
                    .with_context(|| format!("invalid section header in {}", path.display()))?;
                memory.insert(segment.p_paddr(endian) + (offset - start), data)?;
            }
        }

        let (start, data) = memory
            .flatten()
            .with_context(|| format!("failed to load {}", path.display()))?;
        if start < MBR_END {
            bail!(
                "application in {} starts at {start:#x}, which overlaps the MBR",
                path.display()
            );
        }

        Ok(Self { start, data })
    }

    pub(crate) fn into_image(self, sd_req: Fwid) -> Image {
        let Self { start: _, data } = self;
        Image {
            kind: ImageKind::Application,
            data,
            sd_req,
        }
    }

    pub(crate) fn start(&self) -> u32 {
        self.start
    }
}

pub(crate) struct SoftDevice {
    pub(crate) fwid: Fwid,
    pub(crate) end: u32,
    pub(crate) info: SoftDeviceInfo,
    pub(crate) data: Vec<u8>,
}

impl SoftDevice {
    pub(crate) fn from_hex(path: &Path) -> Result<Self> {
        let hex = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let mut memory = Memory::default();
        let mut base = 0;
        for record in ihex::Reader::new(&hex) {
            let record = record.with_context(|| format!("failed to parse {}", path.display()))?;
            match record {
                Record::Data { offset, value } => {
                    let address = base + u32::from(offset);
                    if !(MBR_END..UICR_START).contains(&address) {
                        continue;
                    }

                    memory.insert(address, &value)?;
                }
                Record::ExtendedSegmentAddress(segment) => base = u32::from(segment) << 4,
                Record::ExtendedLinearAddress(upper) => base = u32::from(upper) << 16,
                Record::StartSegmentAddress { .. } | Record::StartLinearAddress(_) => {}
                Record::EndOfFile => break,
            }
        }

        let (start, data) = memory
            .flatten()
            .with_context(|| format!("failed to load {}", path.display()))?;
        if start != MBR_END {
            bail!(
                "SoftDevice in {} starts at {start:#x} instead of {MBR_END:#x}",
                path.display()
            );
        }

        let info = |offset: u32| -> Result<u32> {
            let index = (SD_INFO_START + offset - start) as usize;
            let Some(bytes) = data.get(index..index + 4) else {
                bail!("SoftDevice in {} is too short", path.display());
            };
            Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
        };

        if info(SD_MAGIC_OFFSET)? != SD_MAGIC {
            bail!("{} does not contain a SoftDevice", path.display());
        }

        let size = info(SD_SIZE_OFFSET)?;
        let fwid = Fwid(info(SD_FWID_OFFSET)? as u16);
        Ok(Self {
            fwid,
            end: size.next_multiple_of(PAGE_SIZE),
            info: SoftDeviceInfo {
                version: info(SD_VERSION_OFFSET)?,
                size,
            },
            data,
        })
    }

    pub(crate) fn into_image(self, sd_req: Fwid) -> Image {
        let Self {
            fwid: _,
            end: _,
            info: _,
            data,
        } = self;
        Image {
            kind: ImageKind::SoftDevice,
            data,
            sd_req,
        }
    }
}

/// Version and size from a SoftDevice's info structure
///
/// The bootloader reports these for the installed SoftDevice, but not its firmware ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SoftDeviceInfo {
    pub(crate) version: u32,
    pub(crate) size: u32,
}

impl fmt::Display for SoftDeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let version = self.version;
        write!(
            f,
            "{}.{}.{}",
            version / 1_000_000,
            version / 1000 % 1000,
            version % 1000
        )
    }
}

#[derive(Default)]
struct Memory {
    chunks: Vec<(u32, Vec<u8>)>,
}

impl Memory {
    fn insert(&mut self, address: u32, data: &[u8]) -> Result<()> {
        let end = u64::from(address) + data.len() as u64;
        if end > u64::from(FLASH_END) {
            bail!("{} bytes at {address:#x} do not fit in flash", data.len());
        }

        self.chunks.push((address, data.to_vec()));
        Ok(())
    }

    fn flatten(self) -> Result<(u32, Vec<u8>)> {
        let Self { mut chunks } = self;
        chunks.sort_by_key(|(address, _)| *address);
        let Some(&(start, _)) = chunks.first() else {
            bail!("no data to flash");
        };

        let mut data = Vec::new();
        for (address, chunk) in chunks {
            let offset = (address - start) as usize;
            if offset < data.len() {
                bail!("overlapping data at {address:#x}");
            }

            data.resize(offset, ERASED);
            data.extend_from_slice(&chunk);
        }

        data.resize(data.len().next_multiple_of(4), ERASED);
        Ok((start, data))
    }
}

pub(crate) struct Image {
    kind: ImageKind,
    data: Vec<u8>,
    sd_req: Fwid,
}

impl Image {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut digest = Sha256::digest(&self.data).to_vec();
        digest.reverse();
        let mut hash = Vec::new();
        put_varint_field(&mut hash, 1, HASH_TYPE_SHA256);
        put_bytes_field(&mut hash, 2, &digest);

        let mut sd_req = Vec::new();
        put_varint(&mut sd_req, u64::from(self.sd_req.0));

        let size = self.data.len() as u64;
        let (sd_size, app_size) = match self.kind {
            ImageKind::Application => (0, size),
            ImageKind::SoftDevice => (size, 0),
        };

        let mut init = Vec::new();
        put_varint_field(&mut init, 1, FIRMWARE_VERSION);
        put_varint_field(&mut init, 2, HARDWARE_VERSION);
        put_bytes_field(&mut init, 3, &sd_req);
        put_varint_field(&mut init, 4, self.kind as u64);
        put_varint_field(&mut init, 5, sd_size);
        put_varint_field(&mut init, 6, 0);
        put_varint_field(&mut init, 7, app_size);
        put_bytes_field(&mut init, 8, &hash);
        put_varint_field(&mut init, 9, 0);

        let mut command = Vec::new();
        put_varint_field(&mut command, 1, OP_CODE_INIT);
        put_bytes_field(&mut command, 2, &init);

        let mut packet = Vec::new();
        put_bytes_field(&mut packet, 1, &command);
        packet
    }

    pub(crate) fn kind(&self) -> ImageKind {
        self.kind
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self.data
    }
}

fn put_bytes_field(buf: &mut Vec<u8>, field: u64, value: &[u8]) {
    put_varint(buf, field << 3 | WIRE_TYPE_LEN);
    put_varint(buf, value.len() as u64);
    buf.extend_from_slice(value);
}

fn put_varint_field(buf: &mut Vec<u8>, field: u64, value: u64) {
    put_varint(buf, field << 3 | WIRE_TYPE_VARINT);
    put_varint(buf, value);
}

fn put_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push(value as u8 | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// SoftDevice firmware ID, as used in the `sd_req` field of the init packet
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Fwid(u16);

impl Fwid {
    pub(crate) fn parse(s: &str) -> Result<Self, String> {
        let fwid = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(hex) => u16::from_str_radix(hex, 16),
            None => s.parse(),
        };

        fwid.map(Self)
            .map_err(|error| format!("invalid firmware ID {s:?}: {error}"))
    }

    pub(crate) const NONE: Self = Self(0);
}

impl fmt::Display for Fwid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#06x}", self.0)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ImageKind {
    Application = 0,
    SoftDevice = 1,
}

impl fmt::Display for ImageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Application => "application",
            Self::SoftDevice => "SoftDevice",
        })
    }
}

pub(crate) const MBR_END: u32 = 0x1000;
const PAGE_SIZE: u32 = 0x1000;
const FLASH_END: u32 = 0x10_0000;
const UICR_START: u32 = 0x1000_0000;
const ERASED: u8 = 0xff;

const SD_INFO_START: u32 = MBR_END + 0x2000;
const SD_MAGIC_OFFSET: u32 = 0x04;
const SD_SIZE_OFFSET: u32 = 0x08;
const SD_FWID_OFFSET: u32 = 0x0c;
const SD_VERSION_OFFSET: u32 = 0x14;
const SD_MAGIC: u32 = 0x51b1_e5db;

const OP_CODE_INIT: u64 = 1;
const FIRMWARE_VERSION: u64 = 0xffff_ffff;
const HARDWARE_VERSION: u64 = 52;
const HASH_TYPE_SHA256: u64 = 3;
const WIRE_TYPE_VARINT: u64 = 0;
const WIRE_TYPE_LEN: u64 = 2;
