use std::fs::OpenOptions;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::path::Path;

use byteorder::{LittleEndian, WriteBytesExt};

use crate::error::{Error, Result};

use super::{PartitionInfo, PartitionScheme};

const SECTOR_SIZE: u64 = 512;
const PARTITION_START_LBA: u32 = 2048; // 1MB aligned

pub fn create_partition_table(
    device_path: &Path,
    scheme: PartitionScheme,
    device_size: u64,
) -> Result<PartitionInfo> {
    match scheme {
        PartitionScheme::Mbr => create_mbr(device_path, device_size),
        PartitionScheme::Gpt => create_gpt(device_path, device_size),
    }
}

fn create_mbr(device_path: &Path, device_size: u64) -> Result<PartitionInfo> {
    let total_sectors = (device_size / SECTOR_SIZE) as u32;
    let partition_sectors = total_sectors.saturating_sub(PARTITION_START_LBA);

    let mut cursor = Cursor::new(vec![0u8; SECTOR_SIZE as usize]);
    let mut mbr = mbrman::MBR::new_from(&mut cursor, SECTOR_SIZE as u32, [0xAA, 0xBB, 0xCC, 0xDD])
        .map_err(|e| Error::Other(format!("Failed to create MBR: {e}")))?;

    mbr[1] = mbrman::MBRPartitionEntry {
        boot: mbrman::BOOT_ACTIVE,
        first_chs: mbrman::CHS::empty(),
        sys: 0x0C, // FAT32 LBA
        last_chs: mbrman::CHS::empty(),
        starting_lba: PARTITION_START_LBA,
        sectors: partition_sectors,
    };

    let mut device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(device_path)?;
    device.seek(SeekFrom::Start(0))?;
    mbr.write_into(&mut device)
        .map_err(|e| Error::Other(format!("Failed to write MBR: {e}")))?;
    device.flush()?;

    let offset = PARTITION_START_LBA as u64 * SECTOR_SIZE;
    let size = partition_sectors as u64 * SECTOR_SIZE;
    Ok(PartitionInfo { offset, size })
}

// EFI System Partition type GUID: C12A7328-F81F-11D2-BA4B-00A0C93EC93B
// Stored in mixed-endian format per GPT spec
const EFI_SYSTEM_PARTITION_GUID: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
    0x3B,
];

fn create_gpt(device_path: &Path, device_size: u64) -> Result<PartitionInfo> {
    let total_lbas = device_size / SECTOR_SIZE;

    // GPT layout:
    // LBA 0: Protective MBR
    // LBA 1: Primary GPT Header
    // LBA 2-33: Partition Entry Array (128 entries x 128 bytes = 32 sectors)
    // LBA 34 to last-34: Data
    // LBA last-33 to last-1: Backup Partition Entry Array
    // LBA last: Backup GPT Header

    let entry_array_sectors: u64 = 32;
    let first_usable_lba: u64 = 2 + entry_array_sectors; // LBA 34
    let last_usable_lba: u64 = total_lbas - 1 - entry_array_sectors - 1; // leave room for backup

    let partition_first_lba = PARTITION_START_LBA as u64;
    let partition_last_lba = last_usable_lba;

    let disk_guid = uuid::Uuid::new_v4();
    let partition_guid = uuid::Uuid::new_v4();

    // Build partition entry (128 bytes)
    let mut part_entry = vec![0u8; 128];
    part_entry[0..16].copy_from_slice(&EFI_SYSTEM_PARTITION_GUID);
    part_entry[16..32].copy_from_slice(partition_guid.as_bytes());
    {
        let mut c = Cursor::new(&mut part_entry[32..48]);
        c.write_u64::<LittleEndian>(partition_first_lba)?;
        c.write_u64::<LittleEndian>(partition_last_lba)?;
    }
    // Name: "EFI System Partition" in UTF-16LE
    let name = "EFI System Partition";
    let name_offset = 56;
    for (i, ch) in name.encode_utf16().take(36).enumerate() {
        let pos = name_offset + i * 2;
        part_entry[pos] = ch as u8;
        part_entry[pos + 1] = (ch >> 8) as u8;
    }

    // Build full partition entry array (32 sectors = 16384 bytes)
    let mut entry_array = vec![0u8; (entry_array_sectors * SECTOR_SIZE) as usize];
    entry_array[0..128].copy_from_slice(&part_entry);

    // CRC32 of partition entries
    let entries_crc = crc32(&entry_array);

    // Build GPT header (92 bytes, padded to sector)
    let build_header = |my_lba: u64, alt_lba: u64, entry_start_lba: u64| -> Vec<u8> {
        let mut header = vec![0u8; SECTOR_SIZE as usize];
        // Signature
        header[0..8].copy_from_slice(b"EFI PART");
        // Revision 1.0
        header[8..12].copy_from_slice(&[0x00, 0x00, 0x01, 0x00]);
        // Header size (92 bytes)
        {
            let mut c = Cursor::new(&mut header[12..16]);
            c.write_u32::<LittleEndian>(92).unwrap();
        }
        // Header CRC32 (filled later)
        // Reserved (bytes 20-23 = 0)
        // My LBA
        {
            let mut c = Cursor::new(&mut header[24..32]);
            c.write_u64::<LittleEndian>(my_lba).unwrap();
        }
        // Alternate LBA
        {
            let mut c = Cursor::new(&mut header[32..40]);
            c.write_u64::<LittleEndian>(alt_lba).unwrap();
        }
        // First usable LBA
        {
            let mut c = Cursor::new(&mut header[40..48]);
            c.write_u64::<LittleEndian>(first_usable_lba).unwrap();
        }
        // Last usable LBA
        {
            let mut c = Cursor::new(&mut header[48..56]);
            c.write_u64::<LittleEndian>(last_usable_lba).unwrap();
        }
        // Disk GUID
        header[56..72].copy_from_slice(disk_guid.as_bytes());
        // Partition entry start LBA
        {
            let mut c = Cursor::new(&mut header[72..80]);
            c.write_u64::<LittleEndian>(entry_start_lba).unwrap();
        }
        // Number of partition entries
        {
            let mut c = Cursor::new(&mut header[80..84]);
            c.write_u32::<LittleEndian>(128).unwrap();
        }
        // Size of partition entry
        {
            let mut c = Cursor::new(&mut header[84..88]);
            c.write_u32::<LittleEndian>(128).unwrap();
        }
        // CRC32 of partition entries
        {
            let mut c = Cursor::new(&mut header[88..92]);
            c.write_u32::<LittleEndian>(entries_crc).unwrap();
        }
        // Compute header CRC32 (over first 92 bytes, with CRC field zeroed)
        let header_crc = crc32(&header[0..92]);
        {
            let mut c = Cursor::new(&mut header[16..20]);
            c.write_u32::<LittleEndian>(header_crc).unwrap();
        }
        header
    };

    let primary_header = build_header(1, total_lbas - 1, 2);
    let backup_entry_start = total_lbas - 1 - entry_array_sectors;
    let backup_header = build_header(total_lbas - 1, 1, backup_entry_start);

    // Write protective MBR
    let mut device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(device_path)?;

    let protective_mbr = build_protective_mbr(total_lbas);
    device.seek(SeekFrom::Start(0))?;
    device.write_all(&protective_mbr)?;

    // Write primary GPT header at LBA 1
    device.seek(SeekFrom::Start(SECTOR_SIZE))?;
    device.write_all(&primary_header)?;

    // Write partition entry array at LBA 2
    device.seek(SeekFrom::Start(2 * SECTOR_SIZE))?;
    device.write_all(&entry_array)?;

    // Write backup partition entry array
    device.seek(SeekFrom::Start(backup_entry_start * SECTOR_SIZE))?;
    device.write_all(&entry_array)?;

    // Write backup GPT header at last LBA
    device.seek(SeekFrom::Start((total_lbas - 1) * SECTOR_SIZE))?;
    device.write_all(&backup_header)?;

    device.flush()?;

    let offset = partition_first_lba * SECTOR_SIZE;
    let size = (partition_last_lba - partition_first_lba + 1) * SECTOR_SIZE;
    Ok(PartitionInfo { offset, size })
}

fn build_protective_mbr(total_lbas: u64) -> Vec<u8> {
    let mut mbr = vec![0u8; SECTOR_SIZE as usize];

    // Partition entry 1 at offset 446 (0x1BE)
    let entry = &mut mbr[446..462];
    entry[0] = 0x00; // Not bootable
    entry[1] = 0x00; // Start CHS
    entry[2] = 0x02;
    entry[3] = 0x00;
    entry[4] = 0xEE; // GPT protective type
    entry[5] = 0xFF; // End CHS
    entry[6] = 0xFF;
    entry[7] = 0xFF;
    // Starting LBA = 1
    entry[8..12].copy_from_slice(&1u32.to_le_bytes());
    // Sectors = min(total-1, 0xFFFFFFFF)
    let sectors = std::cmp::min(total_lbas - 1, 0xFFFFFFFF) as u32;
    entry[12..16].copy_from_slice(&sectors.to_le_bytes());

    // Boot signature
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    mbr
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}
