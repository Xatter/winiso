use std::process::Command;

use crate::error::{Error, Result};
use crate::models::UsbDrive;

pub fn list_drives() -> Result<Vec<UsbDrive>> {
    let output = Command::new("diskutil")
        .args(["list", "-plist", "external"])
        .output()?;

    if !output.status.success() {
        return Err(Error::Other("diskutil list failed".into()));
    }

    let plist: plist::Value =
        plist::from_bytes(&output.stdout).map_err(|e| Error::Other(format!("plist parse: {e}")))?;

    let whole_disks = plist
        .as_dictionary()
        .and_then(|d| d.get("WholeDisks"))
        .and_then(|v| v.as_array())
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| v.as_string().map(String::from))
        .collect::<Vec<_>>();

    let mut drives = Vec::new();
    for disk_id in &whole_disks {
        if let Some(drive) = get_drive_info(disk_id)? {
            drives.push(drive);
        }
    }

    Ok(drives)
}

fn get_drive_info(disk_id: &str) -> Result<Option<UsbDrive>> {
    let output = Command::new("diskutil")
        .args(["info", "-plist", &format!("/dev/{disk_id}")])
        .output()?;

    if !output.status.success() {
        return Ok(None);
    }

    let info: plist::Value =
        plist::from_bytes(&output.stdout).map_err(|e| Error::Other(format!("plist parse: {e}")))?;

    let dict = match info.as_dictionary() {
        Some(d) => d,
        None => return Ok(None),
    };

    let is_removable = dict
        .get("RemovableMediaOrExternalDevice")
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);

    let is_virtual = dict
        .get("VirtualOrPhysical")
        .and_then(|v| v.as_string())
        .map(|s| s == "Virtual")
        .unwrap_or(false);

    if !is_removable || is_virtual {
        return Ok(None);
    }

    let name = dict
        .get("MediaName")
        .and_then(|v| v.as_string())
        .unwrap_or("USB Drive")
        .to_string();

    let size = dict
        .get("TotalSize")
        .and_then(|v| v.as_unsigned_integer())
        .unwrap_or(0);

    let partitions = list_partitions(disk_id)?;

    Ok(Some(UsbDrive {
        device: format!("/dev/{disk_id}"),
        name,
        size,
        partitions,
    }))
}

fn list_partitions(disk_id: &str) -> Result<Vec<String>> {
    let output = Command::new("diskutil")
        .args(["list", "-plist", &format!("/dev/{disk_id}")])
        .output()?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let plist: plist::Value =
        plist::from_bytes(&output.stdout).map_err(|e| Error::Other(format!("plist parse: {e}")))?;

    let all_disks = plist
        .as_dictionary()
        .and_then(|d| d.get("AllDisks"))
        .and_then(|v| v.as_array())
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|v| v.as_string().map(String::from))
        .filter(|d| d != disk_id)
        .map(|d| format!("/dev/{d}"))
        .collect();

    Ok(all_disks)
}

pub fn unmount_drive(drive: &UsbDrive) -> Result<()> {
    let status = Command::new("diskutil")
        .args(["unmountDisk", &drive.device])
        .status()?;

    if !status.success() {
        return Err(Error::Other(format!(
            "Failed to unmount {}",
            drive.device
        )));
    }
    Ok(())
}

pub fn eject_drive(drive: &UsbDrive) -> Result<()> {
    let status = Command::new("diskutil")
        .args(["eject", &drive.device])
        .status()?;

    if !status.success() {
        return Err(Error::Other(format!("Failed to eject {}", drive.device)));
    }
    Ok(())
}
