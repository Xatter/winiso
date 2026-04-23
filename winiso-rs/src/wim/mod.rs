use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{Error, Result};

const WIM_MAGIC: [u8; 8] = *b"MSWIM\x00\x00\x00";
const HEADER_SIZE: usize = 208;
const LOOKUP_ENTRY_SIZE: usize = 50;
const DEFAULT_MAX_PART_SIZE: u64 = 3800 * 1024 * 1024;

#[derive(Clone)]
struct WimHeader {
    raw: [u8; HEADER_SIZE],
}

struct LookupEntry {
    compressed_size: u64,
    source_offset: u64,
    original_size: u64,
    flags: u8,
    ref_count: u32,
    sha1: [u8; 20],
}

/// Pre-computed split plan. Separates planning from writing to avoid lifetime issues.
pub struct SplitPlan {
    header: WimHeader,
    entries: Vec<LookupEntry>,
    xml_data: Vec<u8>,
    lt_size: u64,
    parts: Vec<Vec<usize>>,
    assignments: Vec<(u16, u64)>, // (part_number, new_offset) per entry
}

impl SplitPlan {
    pub fn total_parts(&self) -> u16 {
        self.parts.len() as u16
    }

    pub fn part_filename(&self, part_num: u16) -> String {
        if part_num == 1 {
            "install.swm".into()
        } else {
            format!("install{part_num}.swm")
        }
    }
}

/// Parse a WIM and compute how to split it. Does not write anything.
pub fn plan_split<R: Read + Seek>(source: &mut R, max_part_size: u64) -> Result<SplitPlan> {
    let max_part = if max_part_size == 0 {
        DEFAULT_MAX_PART_SIZE
    } else {
        max_part_size
    };

    let mut header_buf = [0u8; HEADER_SIZE];
    source.seek(SeekFrom::Start(0))?;
    source.read_exact(&mut header_buf)?;
    let header = WimHeader::parse(&header_buf)?;

    let lt_offset = header.lookup_table_offset();
    let lt_size = header.lookup_table_size();
    let num_entries = lt_size as usize / LOOKUP_ENTRY_SIZE;

    source.seek(SeekFrom::Start(lt_offset))?;
    let mut lt_data = vec![0u8; lt_size as usize];
    source.read_exact(&mut lt_data)?;

    let entries: Vec<LookupEntry> = (0..num_entries)
        .map(|i| LookupEntry::parse(&lt_data[i * LOOKUP_ENTRY_SIZE..]))
        .collect();

    let xml_offset = header.xml_offset();
    let xml_size = header.xml_size();
    source.seek(SeekFrom::Start(xml_offset))?;
    let mut xml_data = vec![0u8; xml_size as usize];
    source.read_exact(&mut xml_data)?;

    // Separate metadata (always part 1) from data resources
    let mut data_indices: Vec<usize> = (0..entries.len())
        .filter(|&i| !entries[i].is_metadata() && entries[i].compressed_size > 0)
        .collect();
    data_indices.sort_by_key(|&i| entries[i].source_offset);

    let meta_indices: Vec<usize> = (0..entries.len())
        .filter(|&i| entries[i].is_metadata() && entries[i].compressed_size > 0)
        .collect();

    let meta_size: u64 = meta_indices
        .iter()
        .map(|&i| entries[i].compressed_size)
        .sum();
    let overhead = HEADER_SIZE as u64 + lt_size + xml_size;

    // Bin-pack
    let mut parts: Vec<Vec<usize>> = vec![meta_indices];
    let mut part_sizes: Vec<u64> = vec![meta_size];

    for &idx in &data_indices {
        let res_size = entries[idx].compressed_size;
        let last = parts.len() - 1;
        if part_sizes[last] + res_size + overhead > max_part && part_sizes[last] > 0 {
            parts.push(Vec::new());
            part_sizes.push(0);
        }
        let last = parts.len() - 1;
        parts[last].push(idx);
        part_sizes[last] += res_size;
    }

    if parts.len() <= 1 {
        return Err(Error::Other(
            "WIM does not need splitting (fits in one part)".into(),
        ));
    }

    // Sort each part by source offset for sequential I/O
    for list in &mut parts {
        list.sort_by_key(|&i| entries[i].source_offset);
    }

    // Compute assignments
    let mut assignments = vec![(1u16, 0u64); entries.len()];
    for (part_idx, resources) in parts.iter().enumerate() {
        let part_num = (part_idx + 1) as u16;
        let mut offset = HEADER_SIZE as u64;
        for &idx in resources {
            assignments[idx] = (part_num, offset);
            offset += entries[idx].compressed_size;
        }
    }

    Ok(SplitPlan {
        header,
        entries,
        xml_data,
        lt_size,
        parts,
        assignments,
    })
}

/// Write one part of a split WIM. Call once per part after `plan_split`.
pub fn write_part<R: Read + Seek, W: Write>(
    plan: &SplitPlan,
    part_num: u16,
    source: &mut R,
    writer: &mut W,
    on_progress: &dyn Fn(u64, u64),
) -> Result<()> {
    let part_idx = (part_num - 1) as usize;
    let resources = &plan.parts[part_idx];
    let total_parts = plan.total_parts();

    let resources_data_size: u64 = resources
        .iter()
        .map(|&i| plan.entries[i].compressed_size)
        .sum();
    let lt_new_offset = HEADER_SIZE as u64 + resources_data_size;
    let xml_new_offset = lt_new_offset + plan.lt_size;

    // Header
    let mut part_header = plan.header.clone();
    part_header.set_part_info(part_num, total_parts);
    part_header.set_lookup_table(lt_new_offset, plan.lt_size);
    part_header.set_xml_data(xml_new_offset, plan.xml_data.len() as u64);
    part_header.clear_integrity();
    writer.write_all(&part_header.raw)?;

    // Resources
    let mut copy_buf = vec![0u8; 1024 * 1024];
    let mut copied = 0u64;
    for &idx in resources {
        let entry = &plan.entries[idx];
        source.seek(SeekFrom::Start(entry.source_offset))?;
        let mut remaining = entry.compressed_size;
        while remaining > 0 {
            let to_read = (remaining as usize).min(copy_buf.len());
            source.read_exact(&mut copy_buf[..to_read])?;
            writer.write_all(&copy_buf[..to_read])?;
            remaining -= to_read as u64;
        }
        copied += entry.compressed_size;
        on_progress(copied, resources_data_size);
    }

    // Lookup table (all entries, with assignments)
    for (i, entry) in plan.entries.iter().enumerate() {
        let (assigned_part, new_offset) = plan.assignments[i];
        let serialized = entry.serialize(assigned_part, new_offset);
        writer.write_all(&serialized)?;
    }

    // XML data
    writer.write_all(&plan.xml_data)?;

    Ok(())
}

// --- Internal types ---

impl WimHeader {
    fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE || data[0..8] != WIM_MAGIC {
            return Err(Error::Other("Not a valid WIM file".into()));
        }
        let mut raw = [0u8; HEADER_SIZE];
        raw.copy_from_slice(&data[..HEADER_SIZE]);
        Ok(Self { raw })
    }

    fn lookup_table_offset(&self) -> u64 {
        u64::from_le_bytes(self.raw[56..64].try_into().unwrap())
    }

    fn lookup_table_size(&self) -> u64 {
        u64::from_le_bytes(self.raw[48..56].try_into().unwrap()) & 0x00FF_FFFF_FFFF_FFFF
    }

    fn xml_offset(&self) -> u64 {
        u64::from_le_bytes(self.raw[80..88].try_into().unwrap())
    }

    fn xml_size(&self) -> u64 {
        u64::from_le_bytes(self.raw[72..80].try_into().unwrap()) & 0x00FF_FFFF_FFFF_FFFF
    }

    fn set_part_info(&mut self, part: u16, total: u16) {
        self.raw[40..42].copy_from_slice(&part.to_le_bytes());
        self.raw[42..44].copy_from_slice(&total.to_le_bytes());
    }

    fn set_lookup_table(&mut self, offset: u64, size: u64) {
        let size_with_flags = size | (0x02u64 << 56);
        self.raw[48..56].copy_from_slice(&size_with_flags.to_le_bytes());
        self.raw[56..64].copy_from_slice(&offset.to_le_bytes());
        self.raw[64..72].copy_from_slice(&size.to_le_bytes());
    }

    fn set_xml_data(&mut self, offset: u64, size: u64) {
        self.raw[72..80].copy_from_slice(&size.to_le_bytes());
        self.raw[80..88].copy_from_slice(&offset.to_le_bytes());
    }

    fn clear_integrity(&mut self) {
        self.raw[128..152].fill(0);
    }
}

impl LookupEntry {
    fn parse(data: &[u8]) -> Self {
        let size_flags = u64::from_le_bytes(data[0..8].try_into().unwrap());
        Self {
            compressed_size: size_flags & 0x00FF_FFFF_FFFF_FFFF,
            flags: ((size_flags >> 56) & 0xFF) as u8,
            source_offset: u64::from_le_bytes(data[8..16].try_into().unwrap()),
            original_size: u64::from_le_bytes(data[16..24].try_into().unwrap()),
            ref_count: u32::from_le_bytes(data[26..30].try_into().unwrap()),
            sha1: data[30..50].try_into().unwrap(),
        }
    }

    fn is_metadata(&self) -> bool {
        self.flags & 0x02 != 0
    }

    fn serialize(&self, part_number: u16, offset: u64) -> [u8; LOOKUP_ENTRY_SIZE] {
        let mut buf = [0u8; LOOKUP_ENTRY_SIZE];
        let size_flags = self.compressed_size | ((self.flags as u64) << 56);
        buf[0..8].copy_from_slice(&size_flags.to_le_bytes());
        buf[8..16].copy_from_slice(&offset.to_le_bytes());
        buf[16..24].copy_from_slice(&self.original_size.to_le_bytes());
        buf[24..26].copy_from_slice(&part_number.to_le_bytes());
        buf[26..30].copy_from_slice(&self.ref_count.to_le_bytes());
        buf[30..50].copy_from_slice(&self.sha1);
        buf
    }
}

/// Wraps an IsoReader + IsoEntry to provide Read + Seek over a file inside an ISO.
pub struct IsoFileReader<'a> {
    iso: &'a mut crate::iso::IsoReader,
    entry: &'a crate::iso::IsoEntry,
    pos: u64,
    size: u64,
}

impl<'a> IsoFileReader<'a> {
    pub fn new(iso: &'a mut crate::iso::IsoReader, entry: &'a crate::iso::IsoEntry) -> Self {
        Self {
            size: entry.size,
            iso,
            entry,
            pos: 0,
        }
    }
}

impl std::io::Read for IsoFileReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.size {
            return Ok(0);
        }
        let n = self
            .iso
            .read_file_at(self.entry, self.pos, buf)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for IsoFileReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(delta) => self.pos as i64 + delta,
            SeekFrom::End(delta) => self.size as i64 + delta,
        };
        if new_pos < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    #[ignore]
    fn test_wim_split_plan() {
        let iso_path =
            Path::new("/Users/xatter/code/mediacreationtool/Win10_22H2_English_x32v1.iso");
        if !iso_path.exists() {
            return;
        }

        let mut iso = crate::iso::IsoReader::open(iso_path).unwrap();
        let entries = iso.list_files().unwrap();

        let wim_entry = entries
            .iter()
            .find(|e| e.path.to_lowercase().ends_with("install.wim"))
            .expect("install.wim not found");

        println!("install.wim: {:.2} GB", wim_entry.size as f64 / 1e9);

        let mut reader = IsoFileReader::new(&mut iso, wim_entry);
        let plan = plan_split(&mut reader, 0).expect("plan_split failed");

        println!("Split into {} parts:", plan.total_parts());
        for (i, resources) in plan.parts.iter().enumerate() {
            let part_size: u64 = resources
                .iter()
                .map(|&idx| plan.entries[idx].compressed_size)
                .sum();
            println!(
                "  Part {}: {} resources, {:.2} GB",
                i + 1,
                resources.len(),
                part_size as f64 / 1e9
            );
        }

        assert!(plan.total_parts() >= 2, "Should split into at least 2 parts");

        // Verify all entries have assignments
        for (i, &(part, _offset)) in plan.assignments.iter().enumerate() {
            assert!(
                part >= 1 && part <= plan.total_parts(),
                "Entry {i} has invalid part {part}"
            );
        }

        // Verify first part's header can be generated
        let mut test_header = plan.header.clone();
        test_header.set_part_info(1, plan.total_parts());
        assert_eq!(&test_header.raw[0..8], &WIM_MAGIC);
    }

    #[test]
    #[ignore]
    fn test_wim_write_part1_header() {
        let iso_path =
            Path::new("/Users/xatter/code/mediacreationtool/Win10_22H2_English_x32v1.iso");
        if !iso_path.exists() {
            return;
        }

        let mut iso = crate::iso::IsoReader::open(iso_path).unwrap();
        let entries = iso.list_files().unwrap();
        let wim_entry = entries
            .iter()
            .find(|e| e.path.to_lowercase().ends_with("install.wim"))
            .unwrap();

        let mut reader = IsoFileReader::new(&mut iso, wim_entry);
        let plan = plan_split(&mut reader, 0).unwrap();

        // Write part 1 header to a small buffer to verify structure.
        // We don't write the full part (multi-GB) — just check header construction.

        // Manual header check
        let mut hdr = plan.header.clone();
        hdr.set_part_info(1, plan.total_parts());
        let resources_size: u64 = plan.parts[0]
            .iter()
            .map(|&i| plan.entries[i].compressed_size)
            .sum();
        hdr.set_lookup_table(HEADER_SIZE as u64 + resources_size, plan.lt_size);
        hdr.set_xml_data(
            HEADER_SIZE as u64 + resources_size + plan.lt_size,
            plan.xml_data.len() as u64,
        );
        hdr.clear_integrity();

        assert_eq!(&hdr.raw[0..8], b"MSWIM\x00\x00\x00");
        let part_num = u16::from_le_bytes(hdr.raw[40..42].try_into().unwrap());
        let total = u16::from_le_bytes(hdr.raw[42..44].try_into().unwrap());
        assert_eq!(part_num, 1);
        assert_eq!(total, plan.total_parts());
        println!("Part 1 header: part {part_num}/{total}, resources: {resources_size} bytes");
        println!("Header verified OK");
    }
}
