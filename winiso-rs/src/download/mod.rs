use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use sha1::Digest;

use crate::cli::progress;
use crate::error::Result;

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:100.0) Gecko/20100101 Firefox/100.0";
const CHUNK_SIZE: usize = 256 * 1024;

pub fn check_existing(output_path: &Path, sha1: Option<&str>) -> bool {
    if !output_path.exists() {
        return false;
    }

    if let Some(expected) = sha1 {
        eprintln!(
            "Found existing {}, verifying integrity...",
            output_path.file_name().unwrap_or_default().to_string_lossy()
        );
        if verify_sha1(output_path, expected) {
            eprintln!("SHA-1 verified — skipping download.");
            return true;
        }
        let partial = partial_path(output_path);
        if let Err(e) = fs::rename(output_path, &partial) {
            eprintln!("Warning: could not rename to partial file: {e}");
        }
        eprintln!("Hash mismatch — resuming download.");
        return false;
    }

    eprintln!(
        "{} already exists — skipping download.",
        output_path.file_name().unwrap_or_default().to_string_lossy()
    );
    true
}

fn verify_sha1(path: &Path, expected: &str) -> bool {
    let Ok(file) = File::open(path) else { return false };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);

    let pb = progress::transfer_bar(size, "Verifying");

    let mut reader = std::io::BufReader::new(file);
    let mut hasher = sha1::Sha1::new();
    let mut buf = vec![0u8; CHUNK_SIZE];

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return false,
        };
        hasher.update(&buf[..n]);
        pb.inc(n as u64);
    }

    pb.finish_and_clear();

    let hash = format!("{:x}", hasher.finalize());
    hash.eq_ignore_ascii_case(expected)
}

pub fn download_file(url: &str, output_path: &Path, expected_size: Option<u64>) -> Result<PathBuf> {
    let partial = partial_path(output_path);
    let existing_size = if partial.exists() {
        fs::metadata(&partial).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    let mut builder = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(std::time::Duration::from_secs(3600))
        .build()?
        .get(url);

    if existing_size > 0 {
        eprintln!("Resuming download from {}...", fmt_size(existing_size));
        builder = builder.header("Range", format!("bytes={existing_size}-"));
    }

    let resp = builder.send()?;

    if resp.status().as_u16() == 416 {
        if partial.exists() {
            let partial_size = fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
            if expected_size.is_none() || expected_size == Some(partial_size) {
                fs::rename(&partial, output_path)?;
                eprintln!("Download already complete.");
                return Ok(output_path.to_path_buf());
            }
        }
        fs::remove_file(&partial).ok();
        return download_file(url, output_path, expected_size);
    }

    let resp = resp.error_for_status()?;
    let is_partial = resp.status().as_u16() == 206;

    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|cl| if is_partial { cl + existing_size } else { cl })
        .or(expected_size);

    let (mode_existing, file) = if existing_size > 0 && is_partial {
        (
            existing_size,
            OpenOptions::new().append(true).open(&partial)?,
        )
    } else {
        (0, File::create(&partial)?)
    };

    let pb = progress::download_bar(total.unwrap_or(0));
    pb.set_position(mode_existing);

    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_clone = cancelled.clone();
    ctrlc::set_handler(move || {
        cancelled_clone.store(true, Ordering::SeqCst);
    })
    .ok();

    let mut reader = std::io::BufReader::new(resp);
    let mut writer = std::io::BufWriter::new(file);
    let mut buf = vec![0u8; CHUNK_SIZE];

    loop {
        if cancelled.load(Ordering::SeqCst) {
            eprintln!("\nDownload paused. Run again to resume.");
            std::process::exit(130);
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        pb.inc(n as u64);
    }

    writer.flush()?;
    pb.finish_and_clear();

    fs::rename(&partial, output_path)?;
    eprintln!("Saved to {}", output_path.display());
    Ok(output_path.to_path_buf())
}

fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

fn fmt_size(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = n as f64;
    for unit in UNITS {
        if size < 1024.0 {
            return format!("{size:.1} {unit}");
        }
        size /= 1024.0;
    }
    format!("{size:.1} PB")
}
