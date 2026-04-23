from __future__ import annotations

import json
import platform
import plistlib
import signal
import subprocess
from pathlib import Path
from typing import Callable

from .models import UsbDrive


class UsbError(Exception):
    pass


def list_usb_drives() -> list[UsbDrive]:
    system = platform.system()
    if system == "Darwin":
        return _list_drives_macos()
    elif system == "Linux":
        return _list_drives_linux()
    raise UsbError(f"USB drive detection is not supported on {system}")


def write_iso_to_drive(
    iso_path: Path,
    drive: UsbDrive,
    on_progress: Callable[[int, int], None] | None = None,
) -> None:
    system = platform.system()
    if system == "Darwin":
        _write_macos(iso_path, drive, on_progress)
    elif system == "Linux":
        _write_linux(iso_path, drive, on_progress)
    else:
        raise UsbError(f"USB writing is not supported on {system}")


# --- macOS ---


def _list_drives_macos() -> list[UsbDrive]:
    try:
        result = subprocess.run(
            ["diskutil", "list", "-plist", "external"],
            capture_output=True, check=True,
        )
    except subprocess.CalledProcessError:
        result = subprocess.run(
            ["diskutil", "list", "-plist"],
            capture_output=True, check=True,
        )

    plist = plistlib.loads(result.stdout)
    disk_names = plist.get("AllDisksAndPartitions", [])

    drives = []
    for disk in disk_names:
        device_id = disk.get("DeviceIdentifier", "")
        if not device_id:
            continue
        device_path = f"/dev/{device_id}"

        info = _diskutil_info(device_path)
        if not info:
            continue

        is_removable = info.get("RemovableMediaOrExternalDevice", False)
        if not is_removable:
            continue

        if info.get("VirtualOrPhysical") == "Virtual":
            continue
        if info.get("BusProtocol") == "Disk Image":
            continue

        name = info.get("MediaName", "") or info.get("IORegistryEntryName", "Unknown")
        size = info.get("TotalSize", 0) or info.get("Size", 0)

        partitions = []
        for part in disk.get("Partitions", []):
            part_id = part.get("DeviceIdentifier", "")
            if part_id:
                partitions.append(f"/dev/{part_id}")

        drives.append(UsbDrive(
            device=device_path,
            name=name,
            size=size,
            removable=True,
            partitions=partitions,
        ))

    return drives


def _diskutil_info(device: str) -> dict | None:
    try:
        result = subprocess.run(
            ["diskutil", "info", "-plist", device],
            capture_output=True, check=True,
        )
        return plistlib.loads(result.stdout)
    except (subprocess.CalledProcessError, plistlib.InvalidFileException):
        return None


def _write_macos(
    iso_path: Path,
    drive: UsbDrive,
    on_progress: Callable[[int, int], None] | None = None,
) -> None:
    subprocess.run(
        ["diskutil", "unmountDisk", drive.device],
        capture_output=True, check=True,
    )

    raw_device = drive.device.replace("/dev/disk", "/dev/rdisk")
    total_size = iso_path.stat().st_size

    proc = subprocess.Popen(
        ["sudo", "dd", f"if={iso_path}", f"of={raw_device}", "bs=4m"],
        stderr=subprocess.PIPE,
    )

    try:
        if on_progress and proc.pid:
            import threading
            import time

            stop_event = threading.Event()

            def _poll_progress() -> None:
                while not stop_event.is_set():
                    stop_event.wait(2.0)
                    if stop_event.is_set():
                        break
                    try:
                        # SIGINFO makes macOS dd print progress to stderr
                        import os
                        os.kill(proc.pid, signal.SIGINFO)
                    except (ProcessLookupError, OSError):
                        break

            poller = threading.Thread(target=_poll_progress, daemon=True)
            poller.start()

            stderr_lines = []
            for line in iter(proc.stderr.readline, b""):
                decoded = line.decode("utf-8", errors="replace").strip()
                stderr_lines.append(decoded)
                # dd progress line: "12345678 bytes transferred ..."
                if "bytes transferred" in decoded or "bytes" in decoded:
                    parts = decoded.split()
                    try:
                        written = int(parts[0])
                        on_progress(written, total_size)
                    except (ValueError, IndexError):
                        pass

            stop_event.set()
            poller.join(timeout=3)
        else:
            proc.wait()

        returncode = proc.wait()
        if returncode != 0:
            raise UsbError(f"dd failed with exit code {returncode}")
    except KeyboardInterrupt:
        proc.terminate()
        proc.wait()
        raise

    subprocess.run(["diskutil", "eject", drive.device], capture_output=True)


# --- Linux ---


def _list_drives_linux() -> list[UsbDrive]:
    result = subprocess.run(
        ["lsblk", "--json", "--output", "NAME,SIZE,TYPE,RM,MODEL,TRAN", "--bytes"],
        capture_output=True, check=True, text=True,
    )
    data = json.loads(result.stdout)

    drives = []
    for device in data.get("blockdevices", []):
        if device.get("type") != "disk":
            continue
        if not device.get("rm", False):
            continue
        if device.get("tran") != "usb":
            continue

        name = (device.get("model") or "USB Drive").strip()
        size = int(device.get("size", 0))
        dev_path = f"/dev/{device['name']}"

        partitions = []
        for child in device.get("children", []):
            partitions.append(f"/dev/{child['name']}")

        drives.append(UsbDrive(
            device=dev_path,
            name=name,
            size=size,
            removable=True,
            partitions=partitions,
        ))

    return drives


def _write_linux(
    iso_path: Path,
    drive: UsbDrive,
    on_progress: Callable[[int, int], None] | None = None,
) -> None:
    for partition in drive.partitions:
        subprocess.run(["sudo", "umount", partition], capture_output=True)

    total_size = iso_path.stat().st_size

    proc = subprocess.Popen(
        ["sudo", "dd", f"if={iso_path}", f"of={drive.device}", "bs=4M",
         "status=progress", "oflag=sync"],
        stderr=subprocess.PIPE,
    )

    try:
        stderr_lines = []
        for line in iter(proc.stderr.readline, b""):
            decoded = line.decode("utf-8", errors="replace").strip()
            stderr_lines.append(decoded)
            # Linux dd progress: "1234567890 bytes (1.2 GB, 1.1 GiB) copied, ..."
            if "bytes" in decoded and on_progress:
                parts = decoded.split()
                try:
                    written = int(parts[0])
                    on_progress(written, total_size)
                except (ValueError, IndexError):
                    pass

        returncode = proc.wait()
        if returncode != 0:
            raise UsbError(f"dd failed with exit code {returncode}")
    except KeyboardInterrupt:
        proc.terminate()
        proc.wait()
        raise

    subprocess.run(["sync"], check=True)
    subprocess.run(["sudo", "eject", drive.device], capture_output=True)
