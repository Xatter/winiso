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
    parts: Vec<Vec<usize>>,
    assignments: Vec<(u16, u64)>, // (part_number, new_offset) per entry
    boot_metadata_idx: Option<usize>,
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

    pub fn total_resources_size(&self) -> u64 {
        self.parts
            .iter()
            .flat_map(|p| p.iter())
            .map(|&i| self.entries[i].compressed_size)
            .sum()
    }

    pub fn part_resources_size(&self, part_num: u16) -> u64 {
        self.parts[(part_num - 1) as usize]
            .iter()
            .map(|&i| self.entries[i].compressed_size)
            .sum()
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

    // Find the boot metadata entry so we can update its offset in each part's header
    let boot_offset = header.boot_metadata_offset();
    let boot_size = header.boot_metadata_compressed_size();
    let boot_metadata_idx = if boot_size > 0 {
        entries
            .iter()
            .position(|e| e.source_offset == boot_offset && e.compressed_size == boot_size)
    } else {
        None
    };

    Ok(SplitPlan {
        header,
        entries,
        xml_data,
        parts,
        assignments,
        boot_metadata_idx,
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
    let part_lt_size = (resources.len() * LOOKUP_ENTRY_SIZE) as u64;
    let lt_new_offset = HEADER_SIZE as u64 + resources_data_size;
    let xml_new_offset = lt_new_offset + part_lt_size;

    // Header
    let mut part_header = plan.header.clone();
    part_header.set_spanned_flag();
    part_header.set_part_info(part_num, total_parts);
    part_header.set_lookup_table(lt_new_offset, part_lt_size);
    part_header.set_xml_data(xml_new_offset, plan.xml_data.len() as u64);
    part_header.clear_integrity();

    if let Some(boot_idx) = plan.boot_metadata_idx {
        let (boot_part, boot_new_offset) = plan.assignments[boot_idx];
        if boot_part == part_num {
            part_header.set_boot_metadata_offset(boot_new_offset);
        } else {
            part_header.clear_boot_metadata();
        }
    }

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

    // Lookup table (only entries for this part)
    for &idx in resources {
        let (_, new_offset) = plan.assignments[idx];
        let serialized = plan.entries[idx].serialize(part_num, new_offset);
        writer.write_all(&serialized)?;
    }

    // XML data
    writer.write_all(&plan.xml_data)?;

    writer.flush()?;

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

    fn set_spanned_flag(&mut self) {
        let flags = u32::from_le_bytes(self.raw[16..20].try_into().unwrap());
        self.raw[16..20].copy_from_slice(&(flags | 0x08).to_le_bytes());
    }

    fn set_part_info(&mut self, part: u16, total: u16) {
        self.raw[40..42].copy_from_slice(&part.to_le_bytes());
        self.raw[42..44].copy_from_slice(&total.to_le_bytes());
    }

    fn set_lookup_table(&mut self, offset: u64, size: u64) {
        let orig_flags = self.raw[55]; // preserve flags byte
        let size_with_flags = size | ((orig_flags as u64) << 56);
        self.raw[48..56].copy_from_slice(&size_with_flags.to_le_bytes());
        self.raw[56..64].copy_from_slice(&offset.to_le_bytes());
        self.raw[64..72].copy_from_slice(&size.to_le_bytes());
    }

    fn set_xml_data(&mut self, offset: u64, size: u64) {
        let orig_flags = self.raw[79]; // preserve flags byte
        let size_with_flags = size | ((orig_flags as u64) << 56);
        self.raw[72..80].copy_from_slice(&size_with_flags.to_le_bytes());
        self.raw[80..88].copy_from_slice(&offset.to_le_bytes());
        self.raw[88..96].copy_from_slice(&size.to_le_bytes());
    }

    fn boot_metadata_offset(&self) -> u64 {
        u64::from_le_bytes(self.raw[104..112].try_into().unwrap())
    }

    fn boot_metadata_compressed_size(&self) -> u64 {
        u64::from_le_bytes(self.raw[96..104].try_into().unwrap()) & 0x00FF_FFFF_FFFF_FFFF
    }

    fn set_boot_metadata_offset(&mut self, offset: u64) {
        self.raw[104..112].copy_from_slice(&offset.to_le_bytes());
    }

    fn clear_boot_metadata(&mut self) {
        self.raw[96..120].fill(0);
    }

    fn clear_integrity(&mut self) {
        self.raw[124..148].fill(0);
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
            Path::new("/Users/xatter/code/mediacreationtool/winiso-rs/Win11_25H2_English_x64_v2.iso");
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
            Path::new("/Users/xatter/code/mediacreationtool/winiso-rs/Win11_25H2_English_x64_v2.iso");
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
        let part1_lt_size = (plan.parts[0].len() * LOOKUP_ENTRY_SIZE) as u64;
        hdr.set_lookup_table(HEADER_SIZE as u64 + resources_size, part1_lt_size);
        hdr.set_xml_data(
            HEADER_SIZE as u64 + resources_size + part1_lt_size,
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

    #[test]
    #[ignore]
    fn test_wim_write_verify() {
        let iso_path =
            Path::new("/Users/xatter/code/mediacreationtool/winiso-rs/Win11_25H2_English_x64_v2.iso");
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

        // Write part 1 to memory (just header + first few resources + LT + XML)
        // We'll write a truncated version: header + LT + XML only (skip resources)
        // to verify the lookup table structure
        // Write part 1 (has metadata) to verify
        let test_part: u16 = 1;
        let test_resources_size: u64 = plan.parts[(test_part - 1) as usize]
            .iter()
            .map(|&i| plan.entries[i].compressed_size)
            .sum();
        println!("Part {test_part}: {} resources, {:.2} GB",
            plan.parts[(test_part - 1) as usize].len(), test_resources_size as f64 / 1e9);

        let output_path = std::env::temp_dir().join("winiso_test_part.swm");
        {
            let mut file = std::fs::File::create(&output_path).unwrap();
            write_part(&plan, test_part, &mut reader, &mut file, &|_, _| {}).unwrap();
        }

        // Read back and verify
        let data = std::fs::read(&output_path).unwrap();
        println!("Part 2 file size: {} bytes", data.len());

        // Verify header
        assert_eq!(&data[0..8], b"MSWIM\x00\x00\x00", "Bad magic");
        let part_num = u16::from_le_bytes(data[40..42].try_into().unwrap());
        let total = u16::from_le_bytes(data[42..44].try_into().unwrap());
        println!("Part {part_num}/{total}");
        assert_eq!(part_num, test_part);
        assert_eq!(total, plan.total_parts());

        // Verify lookup table location
        let lt_offset = u64::from_le_bytes(data[56..64].try_into().unwrap());
        let lt_size_flags = u64::from_le_bytes(data[48..56].try_into().unwrap());
        let lt_size = lt_size_flags & 0x00FF_FFFF_FFFF_FFFF;
        println!("LT offset={lt_offset}, size={lt_size}");
        assert_eq!(lt_offset, HEADER_SIZE as u64 + test_resources_size);
        let expected_lt_size = (plan.parts[(test_part - 1) as usize].len() * LOOKUP_ENTRY_SIZE) as u64;
        assert_eq!(lt_size, expected_lt_size, "LT size should only include entries for this part");

        // Read lookup table from output and verify entries
        let lt_start = lt_offset as usize;
        let lt_end = lt_start + lt_size as usize;
        assert!(lt_end <= data.len(), "LT extends past file end");

        let num_lt_entries = plan.parts[(test_part - 1) as usize].len();
        for i in 0..num_lt_entries {
            let off = lt_start + i * LOOKUP_ENTRY_SIZE;
            let entry_part = u16::from_le_bytes(data[off + 24..off + 26].try_into().unwrap());
            assert_eq!(entry_part, test_part,
                "Entry {i} should have part_number={test_part}, got {entry_part}");
        }
        println!("  Part {test_part}: {num_lt_entries} entries (all with correct part_number)");

        // Verify resources in the test part are at the right offset
        let test_idx = (test_part - 1) as usize;
        for &idx in &plan.parts[test_idx][..3.min(plan.parts[test_idx].len())] {
            let (assigned_part, assigned_offset) = plan.assignments[idx];
            assert_eq!(assigned_part, test_part);
            // Check the data at assigned_offset matches what we'd read from source
            let res_start = assigned_offset as usize;
            println!("  Resource {idx}: offset={assigned_offset}, size={}", plan.entries[idx].compressed_size);
            // First 4 bytes of resource data in the output file
            if res_start + 4 <= data.len() {
                let first_bytes = &data[res_start..res_start + 4];
                // Read same from source
                reader.seek(SeekFrom::Start(plan.entries[idx].source_offset)).unwrap();
                let mut src_buf = [0u8; 4];
                reader.read_exact(&mut src_buf).unwrap();
                assert_eq!(first_bytes, &src_buf, "Resource {idx} data mismatch at offset {assigned_offset}");
                println!("    Data verified OK (first 4 bytes: {:02x}{:02x}{:02x}{:02x})",
                    first_bytes[0], first_bytes[1], first_bytes[2], first_bytes[3]);
            }
        }

        // Verify XML data
        let xml_offset = u64::from_le_bytes(data[80..88].try_into().unwrap()) as usize;
        let xml_size_flags = u64::from_le_bytes(data[72..80].try_into().unwrap());
        let xml_size = (xml_size_flags & 0x00FF_FFFF_FFFF_FFFF) as usize;
        assert_eq!(&data[xml_offset..xml_offset + 4], &plan.xml_data[..4], "XML data mismatch");
        println!("XML at offset {xml_offset}, size {xml_size} — verified OK");

        std::fs::remove_file(&output_path).ok();
        println!("\nAll verifications passed!");
    }

    #[test]
    #[ignore]
    fn test_dump_wim_header() {
        let iso_path =
            Path::new("/Users/xatter/code/mediacreationtool/winiso-rs/Win11_25H2_English_x64_v2.iso");
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
        let h = &plan.header.raw;

        println!("=== Original WIM Header ===");
        println!("Magic: {:?}", std::str::from_utf8(&h[0..8]));
        println!("Header size: {}", u32::from_le_bytes(h[8..12].try_into().unwrap()));
        println!("Version: {}", u32::from_le_bytes(h[12..16].try_into().unwrap()));
        println!("Flags: 0x{:08x}", u32::from_le_bytes(h[16..20].try_into().unwrap()));
        println!("Part: {}/{}", u16::from_le_bytes(h[40..42].try_into().unwrap()),
            u16::from_le_bytes(h[42..44].try_into().unwrap()));
        println!("Image count: {}", u32::from_le_bytes(h[44..48].try_into().unwrap()));

        // Lookup table reshdr (48-71)
        let lt_size_flags = u64::from_le_bytes(h[48..56].try_into().unwrap());
        let lt_flags = (lt_size_flags >> 56) as u8;
        let lt_size = lt_size_flags & 0x00FF_FFFF_FFFF_FFFF;
        let lt_offset = u64::from_le_bytes(h[56..64].try_into().unwrap());
        let lt_orig = u64::from_le_bytes(h[64..72].try_into().unwrap());
        println!("\nLookup table reshdr (48-71):");
        println!("  size_flags=0x{lt_size_flags:016x} (flags=0x{lt_flags:02x}, size={lt_size})");
        println!("  offset={lt_offset}");
        println!("  original_size={lt_orig}");
        println!("  entries: {}", lt_size / 50);

        // XML data reshdr (72-95)
        let xml_size_flags = u64::from_le_bytes(h[72..80].try_into().unwrap());
        let xml_flags = (xml_size_flags >> 56) as u8;
        let xml_size = xml_size_flags & 0x00FF_FFFF_FFFF_FFFF;
        let xml_offset = u64::from_le_bytes(h[80..88].try_into().unwrap());
        let xml_orig = u64::from_le_bytes(h[88..96].try_into().unwrap());
        println!("\nXML data reshdr (72-95):");
        println!("  size_flags=0x{xml_size_flags:016x} (flags=0x{xml_flags:02x}, size={xml_size})");
        println!("  offset={xml_offset}");
        println!("  original_size={xml_orig}");

        // Boot metadata reshdr (96-119)
        let boot_size_flags = u64::from_le_bytes(h[96..104].try_into().unwrap());
        let boot_flags = (boot_size_flags >> 56) as u8;
        let boot_size = boot_size_flags & 0x00FF_FFFF_FFFF_FFFF;
        let boot_offset = u64::from_le_bytes(h[104..112].try_into().unwrap());
        let boot_orig = u64::from_le_bytes(h[112..120].try_into().unwrap());
        println!("\nBoot metadata reshdr (96-119):");
        println!("  size_flags=0x{boot_size_flags:016x} (flags=0x{boot_flags:02x}, size={boot_size})");
        println!("  offset={boot_offset}");
        println!("  original_size={boot_orig}");

        // Boot index (120-123)
        println!("\nBoot index: {}", u32::from_le_bytes(h[120..124].try_into().unwrap()));

        // Integrity reshdr (124-147)
        let int_size_flags = u64::from_le_bytes(h[124..132].try_into().unwrap());
        let int_flags = (int_size_flags >> 56) as u8;
        let int_size = int_size_flags & 0x00FF_FFFF_FFFF_FFFF;
        let int_offset = u64::from_le_bytes(h[132..140].try_into().unwrap());
        let int_orig = u64::from_le_bytes(h[140..148].try_into().unwrap());
        println!("\nIntegrity reshdr (124-147):");
        println!("  size_flags=0x{int_size_flags:016x} (flags=0x{int_flags:02x}, size={int_size})");
        println!("  offset={int_offset}");
        println!("  original_size={int_orig}");

        // Check what our clear_integrity currently clears (128-152)
        println!("\nBytes 124-152 hex:");
        for i in (124..152).step_by(8) {
            let end = (i + 8).min(152);
            print!("  [{i}..{end}]: ");
            for b in &h[i..end] {
                print!("{b:02x} ");
            }
            println!();
        }
    }
}
