use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use crate::error::Result;

const BLOCK_SIZE: usize = 4 * 1024 * 1024; // 4MB

pub fn write_raw(
    iso_path: &Path,
    device_path: &Path,
    on_progress: &dyn Fn(u64, u64),
) -> Result<()> {
    let mut src = File::open(iso_path)?;
    let total = src.metadata()?.len();

    let raw_path = raw_device_path(device_path);
    let mut dst = OpenOptions::new().write(true).open(&raw_path)?;

    let mut buf = vec![0u8; BLOCK_SIZE];
    let mut written: u64 = 0;

    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        written += n as u64;
        on_progress(written, total);
    }

    dst.flush()?;
    Ok(())
}

fn raw_device_path(device_path: &Path) -> String {
    if cfg!(target_os = "macos") {
        device_path
            .to_string_lossy()
            .replace("/dev/disk", "/dev/rdisk")
    } else {
        device_path.to_string_lossy().to_string()
    }
}
