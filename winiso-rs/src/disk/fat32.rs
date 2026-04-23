use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::Path;

use fatfs::{FatType, FileSystem, FormatVolumeOptions, FsOptions};
use fscommon::{BufStream, StreamSlice};

use crate::error::{Error, Result};
use crate::iso::{IsoEntry, IsoReader};
use crate::wim;

use super::PartitionInfo;

const FAT32_MAX_FILE_SIZE: u64 = 4 * 1024 * 1024 * 1024;

pub fn format_partition(device_path: &Path, part: &PartitionInfo) -> Result<()> {
    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(device_path)?;

    let slice = StreamSlice::new(device, part.offset, part.offset + part.size)
        .map_err(|e| Error::Other(format!("StreamSlice error: {e}")))?;
    let buf = BufStream::new(slice);

    let options = FormatVolumeOptions::new()
        .fat_type(FatType::Fat32)
        .volume_label(*b"WINUSB     ")
        .bytes_per_sector(512);

    fatfs::format_volume(buf, options).map_err(|e| Error::Other(format!("FAT32 format error: {e}")))?;

    Ok(())
}

pub fn copy_iso_to_fat32(
    device_path: &Path,
    part: &PartitionInfo,
    iso: &mut IsoReader,
    entries: &[IsoEntry],
    on_progress: &dyn Fn(u64, u64),
) -> Result<()> {
    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(device_path)?;

    let slice = StreamSlice::new(device, part.offset, part.offset + part.size)
        .map_err(|e| Error::Other(format!("StreamSlice error: {e}")))?;
    let buf = BufStream::new(slice);

    let fs = FileSystem::new(buf, FsOptions::new())
        .map_err(|e| Error::Other(format!("Failed to open FAT32: {e}")))?;
    let root = fs.root_dir();

    let total_size: u64 = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    let mut written: u64 = 0;

    let mut dirs: Vec<&IsoEntry> = entries.iter().filter(|e| e.is_dir).collect();
    dirs.sort_by_key(|e| e.path.matches('/').count());
    for entry in &dirs {
        root.create_dir(&entry.path)
            .map_err(|e| Error::Other(format!("Failed to create dir '{}': {e}", entry.path)))?;
    }

    for entry in entries {
        if entry.is_dir {
            continue;
        }

        if entry.size >= FAT32_MAX_FILE_SIZE {
            continue;
        }

        let mut fat_file = root
            .create_file(&entry.path)
            .map_err(|e| Error::Other(format!("Failed to create '{}': {e}", entry.path)))?;

        let mut progress_writer = ProgressWriter {
            inner: &mut fat_file,
            written: &mut written,
            total: total_size,
            callback: on_progress,
        };

        iso.copy_file_to(entry, &mut progress_writer)?;
        progress_writer.flush().map_err(|e| {
            Error::Other(format!("Failed to flush '{}': {e}", entry.path))
        })?;
    }

    on_progress(total_size, total_size);
    Ok(())
}

struct ProgressWriter<'a, W: io::Write> {
    inner: &'a mut W,
    written: &'a mut u64,
    total: u64,
    callback: &'a dyn Fn(u64, u64),
}

impl<W: io::Write> io::Write for ProgressWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        *self.written += n as u64;
        (self.callback)(*self.written, self.total);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn split_wim_to_fat32(
    device_path: &Path,
    part: &PartitionInfo,
    iso: &mut IsoReader,
    wim_entry: &IsoEntry,
    on_progress: &dyn Fn(u64, u64),
) -> Result<Vec<String>> {
    let mut wim_reader = wim::IsoFileReader::new(iso, wim_entry);
    let plan = wim::plan_split(&mut wim_reader, 0)?;
    let total_parts = plan.total_parts();

    eprintln!(
        "Splitting install.wim into {total_parts} parts ({:.1} GB — FAT32 is required \
         for UEFI boot but has a 4 GB file size limit)...",
        wim_entry.size as f64 / 1e9
    );

    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(device_path)?;
    let slice = StreamSlice::new(device, part.offset, part.offset + part.size)
        .map_err(|e| Error::Other(format!("StreamSlice error: {e}")))?;
    let buf = BufStream::new(slice);
    let fs = FileSystem::new(buf, FsOptions::new())
        .map_err(|e| Error::Other(format!("Failed to open FAT32: {e}")))?;
    let root = fs.root_dir();

    let mut filenames = Vec::new();
    let total_size = plan.total_resources_size();
    let mut base_written: u64 = 0;

    for part_num in 1..=total_parts {
        let filename = plan.part_filename(part_num);
        let fat_path = format!("sources/{filename}");

        let mut fat_file = root
            .create_file(&fat_path)
            .map_err(|e| Error::Other(format!("Failed to create '{fat_path}': {e}")))?;

        eprintln!("  Writing {filename}...");
        let base = base_written;
        wim::write_part(&plan, part_num, &mut wim_reader, &mut fat_file, &|current, _| {
            on_progress(base + current, total_size);
        })?;

        base_written += plan.part_resources_size(part_num);
        filenames.push(filename);
    }

    Ok(filenames)
}
