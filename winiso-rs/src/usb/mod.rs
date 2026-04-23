#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;

use crate::error::Result;
use crate::models::UsbDrive;

pub fn list_usb_drives() -> Result<Vec<UsbDrive>> {
    #[cfg(target_os = "macos")]
    {
        macos::list_drives()
    }
    #[cfg(target_os = "linux")]
    {
        linux::list_drives()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(crate::error::Error::Other(
            "USB drive detection is not supported on this platform".into(),
        ))
    }
}

pub fn unmount_drive(drive: &UsbDrive) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::unmount_drive(drive)
    }
    #[cfg(target_os = "linux")]
    {
        linux::unmount_drive(drive)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = drive;
        Err(crate::error::Error::Other("Not supported".into()))
    }
}

pub fn eject_drive(drive: &UsbDrive) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::eject_drive(drive)
    }
    #[cfg(target_os = "linux")]
    {
        linux::eject_drive(drive)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = drive;
        Err(crate::error::Error::Other("Not supported".into()))
    }
}
