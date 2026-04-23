use std::fs;
use std::process::Command;

use crate::error::{Error, Result};
use crate::models::UsbDrive;

pub fn list_drives() -> Result<Vec<UsbDrive>> {
    let mut drives = Vec::new();

    let entries =
        fs::read_dir("/sys/block").map_err(|e| Error::Other(format!("Cannot read /sys/block: {e}")))?;

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().into_string().unwrap_or_default();

        if !name.starts_with("sd") {
            continue;
        }

        let sys_path = format!("/sys/block/{name}");

        let removable = fs::read_to_string(format!("{sys_path}/removable"))
            .unwrap_or_default()
            .trim()
            == "1";
        if !removable {
            continue;
        }

        let device_link = fs::read_link(format!("{sys_path}/device")).unwrap_or_default();
        let device_str = device_link.to_string_lossy();
        if !device_str.contains("/usb") {
            continue;
        }

        let sectors: u64 = fs::read_to_string(format!("{sys_path}/size"))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0);
        let size = sectors * 512;

        let model = fs::read_to_string(format!("{sys_path}/device/model"))
            .unwrap_or_else(|_| "USB Drive".to_string())
            .trim()
            .to_string();

        let partitions = list_partitions(&name)?;

        drives.push(UsbDrive {
            device: format!("/dev/{name}"),
            name: model,
            size,
            partitions,
        });
    }

    Ok(drives)
}

fn list_partitions(disk_name: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();

    if let Ok(entries) = fs::read_dir(format!("/sys/block/{disk_name}")) {
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().into_string().unwrap_or_default();
            if name.starts_with(disk_name) && name.len() > disk_name.len() {
                parts.push(format!("/dev/{name}"));
            }
        }
    }

    Ok(parts)
}

pub fn unmount_drive(drive: &UsbDrive) -> Result<()> {
    for partition in &drive.partitions {
        let _ = Command::new("umount").arg(partition).status();
    }
    Ok(())
}

pub fn eject_drive(drive: &UsbDrive) -> Result<()> {
    let status = Command::new("eject").arg(&drive.device).status()?;

    if !status.success() {
        return Err(Error::Other(format!("Failed to eject {}", drive.device)));
    }
    Ok(())
}
