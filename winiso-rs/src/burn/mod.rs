use std::path::Path;
use std::process::Command;

use crate::cli::progress;
use crate::disk::{self, PartitionScheme};
use crate::error::{Error, Result};
use crate::iso::IsoReader;
use crate::models::UsbDrive;
use crate::usb;

fn is_root() -> bool {
    // EUID 0 = running as root (directly or via sudo)
    unsafe { libc::geteuid() == 0 }
}

/// Re-invoke ourselves via sudo for privileged device operations.
/// The user sees sudo's password prompt at this point.
pub fn sudo_burn(
    iso_path: &Path,
    drive: &UsbDrive,
    scheme: PartitionScheme,
    raw: bool,
) -> Result<()> {
    if is_root() {
        if raw {
            return write_raw_iso(iso_path, drive);
        } else {
            return create_bootable_usb(iso_path, drive, scheme);
        }
    }

    eprintln!(
        "\nCreating a bootable drive requires direct access to {} which requires \
         escalated privileges, you'll be prompted for your password so we can \
         write the bootable filesystem to the drive.\n",
        drive.device
    );

    let exe = std::env::current_exe()
        .map_err(|e| Error::Other(format!("Failed to get executable path: {e}")))?;

    let mut cmd = Command::new("sudo");
    cmd.arg(&exe)
        .arg("_burn-device")
        .arg("--iso")
        .arg(iso_path)
        .arg("--device")
        .arg(&drive.device)
        .arg("--device-name")
        .arg(&drive.name)
        .arg("--device-size")
        .arg(drive.size.to_string());

    if matches!(scheme, PartitionScheme::Gpt) {
        cmd.arg("--gpt");
    }
    if raw {
        cmd.arg("--raw");
    }

    let status = cmd.status()?;
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        if code == 1 && !is_root() {
            return Err(Error::Other(
                "Authentication failed or was cancelled. Run again to retry.".into(),
            ));
        }
        return Err(Error::Other(format!(
            "Burn failed (exit code {code}). Check the output above for details."
        )));
    }
    Ok(())
}

pub fn create_bootable_usb(
    iso_path: &Path,
    drive: &UsbDrive,
    scheme: PartitionScheme,
) -> Result<()> {
    let mut iso = IsoReader::open(iso_path)?;
    let entries = iso.list_files()?;
    let total_file_size: u64 = entries.iter().map(|e| e.size).sum();

    let usable = (drive.size as f64 * 0.95) as u64;
    if total_file_size > usable {
        return Err(Error::Other(format!(
            "ISO contents ({:.1} GB) exceed drive capacity ({:.1} GB)",
            total_file_size as f64 / 1e9,
            drive.size as f64 / 1e9,
        )));
    }

    let needs_wim_split = iso.has_oversized_wim()?;

    let device_path = Path::new(&drive.device);

    eprintln!("Unmounting drive...");
    usb::unmount_drive(drive)?;

    eprintln!(
        "Creating {} partition table...",
        match scheme {
            PartitionScheme::Mbr => "MBR",
            PartitionScheme::Gpt => "GPT",
        }
    );
    let part_info = disk::partition::create_partition_table(device_path, scheme, drive.size)?;

    eprintln!("Formatting FAT32...");
    disk::fat32::format_partition(device_path, &part_info)?;

    // macOS auto-mounts newly formatted partitions; wait for it then unmount
    std::thread::sleep(std::time::Duration::from_millis(500));
    usb::unmount_drive(drive)?;

    eprintln!("Copying files from ISO...");
    let pb = progress::transfer_bar(total_file_size, "Copying");

    disk::fat32::copy_iso_to_fat32(device_path, &part_info, &mut iso, &entries, &|current, total| {
        pb.set_length(total);
        pb.set_position(current);
    })?;

    pb.finish_with_message("Done");

    if needs_wim_split {
        // Unmount again in case macOS re-mounted after file copy
        std::thread::sleep(std::time::Duration::from_millis(500));
        usb::unmount_drive(drive)?;

        let wim_entry = entries
            .iter()
            .find(|e| {
                e.path.to_lowercase().ends_with("install.wim")
                    && e.size >= 4 * 1024 * 1024 * 1024
            })
            .ok_or_else(|| Error::Other("install.wim not found for splitting".into()))?
            .clone();

        let pb2 = progress::transfer_bar(wim_entry.size, "Splitting WIM");

        disk::fat32::split_wim_to_fat32(
            device_path,
            &part_info,
            &mut iso,
            &wim_entry,
            &|current, total| {
                pb2.set_length(total);
                pb2.set_position(current);
            },
        )?;

        pb2.finish_with_message("Done");
    }

    eprintln!("Ejecting drive...");
    usb::eject_drive(drive)?;

    eprintln!("\nBootable USB created successfully.");
    Ok(())
}

pub fn write_raw_iso(iso_path: &Path, drive: &UsbDrive) -> Result<()> {
    let iso_size = std::fs::metadata(iso_path)?.len();
    let device_path = Path::new(&drive.device);

    eprintln!("Unmounting drive...");
    usb::unmount_drive(drive)?;

    eprintln!("Writing ISO to drive...");
    let pb = progress::transfer_bar(iso_size, "Writing");

    disk::raw_write::write_raw(iso_path, device_path, &|current, total| {
        pb.set_length(total);
        pb.set_position(current);
    })?;

    pb.finish_with_message("Done");

    eprintln!("Ejecting drive...");
    usb::eject_drive(drive)?;

    eprintln!("\nISO written to drive successfully.");
    Ok(())
}

pub fn confirm_burn(drive: &UsbDrive, iso_path: &Path) -> bool {
    let size_gb = drive.size as f64 / 1e9;
    let iso_name = iso_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    eprintln!();
    eprintln!("  ┌─────────────────────────────────────────────┐");
    eprintln!("  │  WARNING: ALL DATA ON THE DRIVE WILL BE     │");
    eprintln!("  │  PERMANENTLY ERASED.                        │");
    eprintln!("  └─────────────────────────────────────────────┘");
    eprintln!();
    eprintln!("  ISO:    {iso_name}");
    eprintln!("  Drive:  {} ({:.1} GB)", drive.name, size_gb);
    eprintln!("  Device: {}", drive.device);
    eprintln!();

    let device_short = drive
        .device
        .rsplit('/')
        .next()
        .unwrap_or(&drive.device);

    eprintln!("  Type '{device_short}' to confirm, or anything else to cancel:");

    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    input.trim() == device_short
}
