use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{Error, Result};

const SECTOR_SIZE: u64 = 2048;
const FAT32_MAX_FILE_SIZE: u64 = 4 * 1024 * 1024 * 1024;

pub struct IsoReader {
    file: File,
    format: IsoFormat,
}

enum IsoFormat {
    Iso9660 {
        root_extent_lba: u32,
        root_extent_len: u32,
        joliet: bool,
    },
    Udf {
        partition_start: u32,
        root_icb_lba: u32,
    },
}

#[derive(Debug, Clone)]
pub struct IsoEntry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    extent_lba: u32,
    data_length: u64,
    alloc_descs: Vec<(u32, u32)>,
}

impl IsoReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;

        // Try UDF first (check AVDP at sector 256)
        if let Some(format) = try_udf(&mut file)? {
            return Ok(Self { file, format });
        }

        // Fall back to ISO 9660
        let format = try_iso9660(&mut file)?;
        Ok(Self { file, format })
    }

    pub fn list_files(&mut self) -> Result<Vec<IsoEntry>> {
        let mut entries = Vec::new();
        match &self.format {
            IsoFormat::Iso9660 {
                root_extent_lba,
                root_extent_len,
                joliet,
            } => {
                let lba = *root_extent_lba;
                let len = *root_extent_len;
                let jol = *joliet;
                self.walk_iso9660(lba, len, jol, "", &mut entries)?;
            }
            IsoFormat::Udf {
                partition_start,
                root_icb_lba,
            } => {
                let ps = *partition_start;
                let ri = *root_icb_lba;
                self.walk_udf(ps, ri, "", &mut entries)?;
            }
        }
        Ok(entries)
    }

    pub fn has_oversized_wim(&mut self) -> Result<bool> {
        let entries = self.list_files()?;
        Ok(entries.iter().any(|e| {
            e.path.to_lowercase().ends_with("install.wim") && e.size >= FAT32_MAX_FILE_SIZE
        }))
    }

    pub fn read_file_at(&mut self, entry: &IsoEntry, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let part_start = match &self.format {
            IsoFormat::Udf {
                partition_start, ..
            } => *partition_start as u64,
            IsoFormat::Iso9660 { .. } => 0,
        };

        if offset >= entry.data_length {
            return Ok(0);
        }

        let max_read = ((entry.data_length - offset) as usize).min(buf.len());
        if max_read == 0 {
            return Ok(0);
        }

        if entry.alloc_descs.is_empty() {
            let abs = (part_start + entry.extent_lba as u64) * SECTOR_SIZE + offset;
            self.file.seek(SeekFrom::Start(abs))?;
            self.file.read_exact(&mut buf[..max_read])?;
            return Ok(max_read);
        }

        let mut file_pos = 0u64;
        let mut bytes_read = 0usize;
        let mut read_offset = offset;

        for &(pos, len) in &entry.alloc_descs {
            let extent_end = file_pos + len as u64;
            if read_offset >= extent_end {
                file_pos = extent_end;
                continue;
            }

            let offset_in_extent = read_offset - file_pos;
            let available = len as u64 - offset_in_extent;
            let to_read = (available as usize).min(max_read - bytes_read);

            let abs = (part_start + pos as u64) * SECTOR_SIZE + offset_in_extent;
            self.file.seek(SeekFrom::Start(abs))?;
            self.file.read_exact(&mut buf[bytes_read..bytes_read + to_read])?;
            bytes_read += to_read;

            if bytes_read >= max_read {
                break;
            }
            read_offset = extent_end;
            file_pos = extent_end;
        }

        Ok(bytes_read)
    }

    pub fn copy_file_to<W: io::Write>(
        &mut self,
        entry: &IsoEntry,
        writer: &mut W,
    ) -> Result<u64> {
        let part_start = match &self.format {
            IsoFormat::Udf {
                partition_start, ..
            } => *partition_start,
            IsoFormat::Iso9660 { .. } => 0,
        };

        if !entry.alloc_descs.is_empty() {
            let mut total = 0u64;
            let mut buf = vec![0u8; 1024 * 1024];
            let mut remaining = entry.data_length;
            for &(pos, len) in &entry.alloc_descs {
                let abs_lba = part_start as u64 + pos as u64;
                self.file.seek(SeekFrom::Start(abs_lba * SECTOR_SIZE))?;
                let extent_len = (len as u64).min(remaining);
                let mut ext_remaining = extent_len;
                while ext_remaining > 0 {
                    let to_read = (ext_remaining as usize).min(buf.len());
                    self.file.read_exact(&mut buf[..to_read])?;
                    writer.write_all(&buf[..to_read])?;
                    ext_remaining -= to_read as u64;
                    total += to_read as u64;
                }
                remaining -= extent_len;
            }
            Ok(total)
        } else {
            let abs_lba = part_start as u64 + entry.extent_lba as u64;
            self.file.seek(SeekFrom::Start(abs_lba * SECTOR_SIZE))?;
            let mut remaining = entry.data_length;
            let mut buf = vec![0u8; 1024 * 1024];
            let mut total = 0u64;
            while remaining > 0 {
                let to_read = (remaining as usize).min(buf.len());
                self.file.read_exact(&mut buf[..to_read])?;
                writer.write_all(&buf[..to_read])?;
                remaining -= to_read as u64;
                total += to_read as u64;
            }
            Ok(total)
        }
    }

    // --- ISO 9660 ---

    fn walk_iso9660(
        &mut self,
        extent_lba: u32,
        extent_len: u32,
        joliet: bool,
        prefix: &str,
        entries: &mut Vec<IsoEntry>,
    ) -> Result<()> {
        let mut dir_data = vec![0u8; extent_len as usize];
        self.file
            .seek(SeekFrom::Start(extent_lba as u64 * SECTOR_SIZE))?;
        self.file.read_exact(&mut dir_data)?;

        let mut subdirs = Vec::new();
        let mut offset = 0usize;

        while offset < dir_data.len() {
            let record_len = dir_data[offset] as usize;
            if record_len == 0 {
                let next = ((offset / SECTOR_SIZE as usize) + 1) * SECTOR_SIZE as usize;
                offset = next;
                continue;
            }
            if offset + record_len > dir_data.len() {
                break;
            }

            let rec = &dir_data[offset..offset + record_len];
            let file_lba = u32::from_le_bytes(rec[2..6].try_into().unwrap());
            let data_len = u32::from_le_bytes(rec[10..14].try_into().unwrap());
            let flags = rec[25];
            let is_dir = flags & 0x02 != 0;
            let id_len = rec[32] as usize;
            let file_id = &rec[33..33 + id_len];

            offset += record_len;

            if id_len == 1 && (file_id[0] == 0 || file_id[0] == 1) {
                continue;
            }

            let name = if joliet {
                decode_ucs2_be(file_id)
            } else {
                strip_version(&String::from_utf8_lossy(file_id))
            };

            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };

            if is_dir {
                entries.push(IsoEntry {
                    path: path.clone(),
                    size: 0,
                    is_dir: true,
                    extent_lba: file_lba,
                    data_length: data_len as u64,
                    alloc_descs: vec![],
                });
                subdirs.push((file_lba, data_len, path));
            } else {
                entries.push(IsoEntry {
                    path,
                    size: data_len as u64,
                    is_dir: false,
                    extent_lba: file_lba,
                    data_length: data_len as u64,
                    alloc_descs: vec![],
                });
            }
        }

        for (lba, len, path) in subdirs {
            self.walk_iso9660(lba, len, joliet, &path, entries)?;
        }

        Ok(())
    }

    // --- UDF ---

    fn walk_udf(
        &mut self,
        partition_start: u32,
        icb_lba: u32,
        prefix: &str,
        entries: &mut Vec<IsoEntry>,
    ) -> Result<()> {
        let fe = self.read_sector(partition_start as u64 + icb_lba as u64)?;
        let fe_tag = u16::from_le_bytes(fe[0..2].try_into().unwrap());
        if fe_tag != 261 {
            return Err(Error::Other(format!(
                "Expected File Entry (261) at LBA {}, got {fe_tag}",
                partition_start as u64 + icb_lba as u64
            )));
        }

        let dir_data = self.read_udf_file_data(&fe, partition_start)?;
        let mut subdirs = Vec::new();
        let mut offset = 0usize;

        while offset + 38 <= dir_data.len() {
            let fid_tag = u16::from_le_bytes(dir_data[offset..offset + 2].try_into().unwrap());
            if fid_tag != 257 {
                break;
            }

            let fid_chars = dir_data[offset + 18];
            let l_fi = dir_data[offset + 19] as usize;
            let child_icb_lba =
                u32::from_le_bytes(dir_data[offset + 24..offset + 28].try_into().unwrap());
            let l_iu = u16::from_le_bytes(dir_data[offset + 36..offset + 38].try_into().unwrap())
                as usize;

            let is_parent = fid_chars & 0x08 != 0;
            let is_dir = fid_chars & 0x02 != 0;

            let fi_start = offset + 38 + l_iu;

            let name = if is_parent || l_fi == 0 {
                None
            } else {
                Some(decode_udf_name(&dir_data[fi_start..fi_start + l_fi]))
            };

            let fid_size = (38 + l_iu + l_fi + 3) & !3;
            offset += fid_size;

            let name = match name {
                Some(n) => n,
                None => continue,
            };

            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };

            if is_dir {
                entries.push(IsoEntry {
                    path: path.clone(),
                    size: 0,
                    is_dir: true,
                    extent_lba: child_icb_lba,
                    data_length: 0,
                    alloc_descs: vec![],
                });
                subdirs.push((child_icb_lba, path));
            } else {
                let child_fe =
                    self.read_sector(partition_start as u64 + child_icb_lba as u64)?;
                let child_tag = u16::from_le_bytes(child_fe[0..2].try_into().unwrap());
                if child_tag == 261 {
                    let file_size = u64::from_le_bytes(child_fe[56..64].try_into().unwrap());
                    let alloc_descs = parse_short_ads(&child_fe);
                    entries.push(IsoEntry {
                        path,
                        size: file_size,
                        is_dir: false,
                        extent_lba: alloc_descs.first().map(|a| a.0).unwrap_or(0),
                        data_length: file_size,
                        alloc_descs,
                    });
                }
            }
        }

        for (child_icb_lba, path) in subdirs {
            self.walk_udf(partition_start, child_icb_lba, &path, entries)?;
        }

        Ok(())
    }

    fn read_udf_file_data(&mut self, fe: &[u8], partition_start: u32) -> Result<Vec<u8>> {
        let alloc_descs = parse_short_ads(fe);
        let mut data = Vec::new();
        for (pos, len) in &alloc_descs {
            let abs_lba = partition_start as u64 + *pos as u64;
            self.file.seek(SeekFrom::Start(abs_lba * SECTOR_SIZE))?;
            let mut buf = vec![0u8; *len as usize];
            self.file.read_exact(&mut buf)?;
            data.extend_from_slice(&buf);
        }
        Ok(data)
    }

    fn read_sector(&mut self, lba: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        self.file.seek(SeekFrom::Start(lba * SECTOR_SIZE))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }
}

fn parse_short_ads(fe: &[u8]) -> Vec<(u32, u32)> {
    let l_ea = u32::from_le_bytes(fe[168..172].try_into().unwrap());
    let l_ad = u32::from_le_bytes(fe[172..176].try_into().unwrap());
    let icb_flags = u16::from_le_bytes(fe[34..36].try_into().unwrap());
    let ad_type = icb_flags & 0x07;

    if ad_type != 0 {
        return vec![];
    }

    let ad_start = 176 + l_ea as usize;
    let num_ads = l_ad as usize / 8;
    let mut descs = Vec::with_capacity(num_ads);
    for i in 0..num_ads {
        let off = ad_start + i * 8;
        if off + 8 > fe.len() {
            break;
        }
        let len_raw = u32::from_le_bytes(fe[off..off + 4].try_into().unwrap());
        let pos = u32::from_le_bytes(fe[off + 4..off + 8].try_into().unwrap());
        let len = len_raw & 0x3FFF_FFFF;
        let ad_flag = (len_raw >> 30) & 0x03;
        if ad_flag == 0 && len > 0 {
            descs.push((pos, len));
        }
    }
    descs
}

// --- Detection ---

fn try_udf(file: &mut File) -> Result<Option<IsoFormat>> {
    let file_size = file.metadata()?.len();
    if file_size < 257 * SECTOR_SIZE {
        return Ok(None);
    }

    file.seek(SeekFrom::Start(256 * SECTOR_SIZE))?;
    let mut buf = [0u8; SECTOR_SIZE as usize];
    file.read_exact(&mut buf)?;

    let tag_id = u16::from_le_bytes(buf[0..2].try_into().unwrap());
    if tag_id != 2 {
        return Ok(None);
    }

    let main_extent_len = u32::from_le_bytes(buf[16..20].try_into().unwrap());
    let main_extent_loc = u32::from_le_bytes(buf[20..24].try_into().unwrap());
    let num_sectors = main_extent_len / SECTOR_SIZE as u32;

    let mut partition_start = 0u32;
    let mut fsd_lba = 0u32;

    for i in 0..num_sectors {
        file.seek(SeekFrom::Start((main_extent_loc as u64 + i as u64) * SECTOR_SIZE))?;
        file.read_exact(&mut buf)?;
        let vds_tag = u16::from_le_bytes(buf[0..2].try_into().unwrap());

        match vds_tag {
            5 => {
                partition_start = u32::from_le_bytes(buf[188..192].try_into().unwrap());
            }
            6 => {
                fsd_lba = u32::from_le_bytes(buf[252..256].try_into().unwrap());
            }
            8 => break,
            _ => {}
        }
    }

    if partition_start == 0 {
        return Ok(None);
    }

    // Read File Set Descriptor
    let fsd_abs = partition_start as u64 + fsd_lba as u64;
    file.seek(SeekFrom::Start(fsd_abs * SECTOR_SIZE))?;
    file.read_exact(&mut buf)?;
    let fsd_tag = u16::from_le_bytes(buf[0..2].try_into().unwrap());
    if fsd_tag != 256 {
        return Ok(None);
    }

    let root_icb_lba = u32::from_le_bytes(buf[404..408].try_into().unwrap());

    Ok(Some(IsoFormat::Udf {
        partition_start,
        root_icb_lba,
    }))
}

fn try_iso9660(file: &mut File) -> Result<IsoFormat> {
    let mut buf = [0u8; SECTOR_SIZE as usize];
    let mut root_lba = 0u32;
    let mut root_len = 0u32;
    let mut joliet = false;

    let mut sector = 16u64;
    loop {
        file.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
        file.read_exact(&mut buf)?;

        if &buf[1..6] != b"CD001" {
            break;
        }

        match buf[0] {
            1 => {
                let root_rec = &buf[156..190];
                root_lba = u32::from_le_bytes(root_rec[2..6].try_into().unwrap());
                root_len = u32::from_le_bytes(root_rec[10..14].try_into().unwrap());
            }
            2 => {
                let esc = &buf[88..91];
                if esc == b"%/@" || esc == b"%/C" || esc == b"%/E" {
                    let root_rec = &buf[156..190];
                    root_lba = u32::from_le_bytes(root_rec[2..6].try_into().unwrap());
                    root_len = u32::from_le_bytes(root_rec[10..14].try_into().unwrap());
                    joliet = true;
                }
            }
            255 => break,
            _ => {}
        }

        sector += 1;
        if sector - 16 > 100 {
            break;
        }
    }

    if root_lba == 0 {
        return Err(Error::Other("No volume descriptor found in ISO".into()));
    }

    Ok(IsoFormat::Iso9660 {
        root_extent_lba: root_lba,
        root_extent_len: root_len,
        joliet,
    })
}

fn decode_ucs2_be(data: &[u8]) -> String {
    let chars: Vec<u16> = data
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    strip_version(&String::from_utf16_lossy(&chars))
}

fn decode_udf_name(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    let comp_id = data[0];
    if comp_id == 16 {
        let chars: Vec<u16> = data[1..]
            .chunks(2)
            .map(|c| {
                if c.len() == 2 {
                    u16::from_be_bytes([c[0], c[1]])
                } else {
                    c[0] as u16
                }
            })
            .collect();
        String::from_utf16_lossy(&chars)
    } else if comp_id == 8 {
        String::from_utf8_lossy(&data[1..]).into_owned()
    } else {
        String::from_utf8_lossy(data).into_owned()
    }
}

fn strip_version(s: &str) -> String {
    match s.rfind(';') {
        Some(pos) => s[..pos].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn test_read_real_iso() {
        let path = Path::new("/Users/xatter/code/mediacreationtool/Win10_22H2_English_x32v1.iso");
        if !path.exists() {
            eprintln!("ISO not found, skipping");
            return;
        }

        let mut reader = IsoReader::open(path).expect("Failed to open ISO");
        let entries = reader.list_files().expect("Failed to list files");

        assert!(!entries.is_empty(), "ISO should have files");
        let format = match &reader.format {
            IsoFormat::Udf { .. } => "UDF",
            IsoFormat::Iso9660 { joliet, .. } => {
                if *joliet {
                    "ISO9660+Joliet"
                } else {
                    "ISO9660"
                }
            }
        };
        println!("Found {} entries (format={})", entries.len(), format);

        for entry in entries.iter().take(30) {
            let kind = if entry.is_dir { "DIR " } else { "FILE" };
            println!("  {} {:>12}  {}", kind, entry.size, entry.path);
        }

        let total: u64 = entries.iter().map(|e| e.size).sum();
        println!("Total: {:.2} GB", total as f64 / 1e9);

        let has_big_wim = reader.has_oversized_wim().expect("wim check");
        println!("Has oversized WIM: {}", has_big_wim);
    }
}
