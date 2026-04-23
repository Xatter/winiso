from __future__ import annotations

import os
import platform
import plistlib
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Callable

from .models import UsbDrive
from .usb import UsbError

FAT32_MAX_FILE_SIZE = 4 * 1024**3  # 4 GiB
WIM_SPLIT_SIZE_MB = 3800


def preflight_check(iso_path: Path, drive: UsbDrive) -> None:
    iso_size = iso_path.stat().st_size
    usable = int(drive.size * 0.95)
    if iso_size > usable:
        iso_gb = iso_size / 1024**3
        drive_gb = drive.size / 1024**3
        raise UsbError(
            f"ISO ({iso_gb:.1f} GB) may not fit on drive ({drive_gb:.1f} GB) "
            f"after FAT32 formatting overhead."
        )

    needs_split = _iso_has_large_wim(iso_path)
    if needs_split and not _wimlib_available():
        system = platform.system()
        if system == "Darwin":
            install_cmd = "brew install wimlib"
        else:
            install_cmd = "sudo apt install wimlib-tools"
        raise UsbError(
            f"This ISO contains install.wim larger than 4 GB, which must be\n"
            f"split to fit on FAT32. Install wimlib to enable this:\n\n"
            f"  {install_cmd}\n\n"
            f"Or use --raw to write the ISO as a raw image (may not boot on all systems)."
        )


def create_bootable_usb(
    iso_path: Path,
    drive: UsbDrive,
    on_phase: Callable[[str, int, int], None] | None = None,
) -> None:
    system = platform.system()
    if system not in ("Darwin", "Linux"):
        raise UsbError(f"Bootable USB creation is not supported on {system}")

    iso_mount: Path | None = None
    linux_usb_mount: Path | None = None

    try:
        if on_phase:
            on_phase("Formatting drive", 0, 1)

        if system == "Darwin":
            usb_mount = _format_drive_macos(drive)
        else:
            linux_usb_mount = _format_drive_linux(drive)
            usb_mount = linux_usb_mount

        if on_phase:
            on_phase("Formatting drive", 1, 1)

        if system == "Darwin":
            iso_mount = _mount_iso_macos(iso_path)
        else:
            iso_mount = _mount_iso_linux(iso_path)

        wim_path = _find_install_wim(iso_mount)
        needs_split = wim_path is not None and wim_path.stat().st_size >= FAT32_MAX_FILE_SIZE

        total_size, files = _scan_files(iso_mount, skip_wim=wim_path if needs_split else None)
        if needs_split and wim_path:
            total_size += wim_path.stat().st_size

        copied = 0

        def copy_progress(chunk_size: int) -> None:
            nonlocal copied
            copied += chunk_size
            if on_phase:
                on_phase("Copying files", copied, total_size)

        if on_phase:
            on_phase("Copying files", 0, total_size)

        _copy_files(files, iso_mount, usb_mount, copy_progress)

        if needs_split and wim_path:
            dest_dir = usb_mount / wim_path.relative_to(iso_mount).parent
            dest_dir.mkdir(parents=True, exist_ok=True)

            def split_progress(chunk_size: int) -> None:
                nonlocal copied
                copied += chunk_size
                if on_phase:
                    on_phase("Splitting install.wim", copied, total_size)

            if on_phase:
                on_phase("Splitting install.wim", copied, total_size)

            _split_wim(wim_path, dest_dir, split_progress)

        if system == "Linux":
            subprocess.run(["sync"], check=True)

    finally:
        if iso_mount:
            try:
                _unmount_iso(iso_mount)
            except Exception:
                pass
        if linux_usb_mount:
            try:
                subprocess.run(["sudo", "umount", str(linux_usb_mount)], capture_output=True)
                shutil.rmtree(linux_usb_mount, ignore_errors=True)
            except Exception:
                pass

        try:
            if system == "Darwin":
                subprocess.run(["diskutil", "eject", drive.device], capture_output=True)
            elif system == "Linux":
                subprocess.run(["sudo", "eject", drive.device], capture_output=True)
        except Exception:
            pass


# --- Dependency checks ---


def _wimlib_available() -> bool:
    return shutil.which("wimlib-imagex") is not None


def _iso_has_large_wim(iso_path: Path) -> bool:
    system = platform.system()
    mount_point: Path | None = None
    try:
        if system == "Darwin":
            mount_point = _mount_iso_macos(iso_path)
        elif system == "Linux":
            mount_point = _mount_iso_linux(iso_path)
        else:
            return False

        wim = _find_install_wim(mount_point)
        if wim is None:
            return False
        return wim.stat().st_size >= FAT32_MAX_FILE_SIZE
    finally:
        if mount_point:
            try:
                _unmount_iso(mount_point)
            except Exception:
                pass


def _find_install_wim(iso_mount: Path) -> Path | None:
    sources = iso_mount / "sources"
    if not sources.exists():
        for d in iso_mount.iterdir():
            if d.is_dir() and d.name.lower() == "sources":
                sources = d
                break
        else:
            return None

    for f in sources.iterdir():
        if f.name.lower() == "install.wim" and f.is_file():
            return f
    return None


# --- macOS platform ---


def _format_drive_macos(drive: UsbDrive) -> Path:
    result = subprocess.run(
        ["diskutil", "eraseDisk", "MS-DOS", "WINUSB", "MBR", drive.device],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise UsbError(
            f"Failed to format {drive.device}. "
            f"Is the drive write-protected?\n{result.stderr.strip()}"
        )

    mount_point = Path("/Volumes/WINUSB")
    if not mount_point.exists():
        raise UsbError(
            "USB drive did not mount after formatting. "
            "Check Disk Utility for errors."
        )
    return mount_point


def _mount_iso_macos(iso_path: Path) -> Path:
    result = subprocess.run(
        ["hdiutil", "attach", str(iso_path), "-readonly", "-nobrowse", "-plist"],
        capture_output=True,
    )
    if result.returncode != 0:
        stderr = result.stderr.decode("utf-8", errors="replace").strip()
        raise UsbError(f"Failed to mount ISO: {stderr}")

    plist = plistlib.loads(result.stdout)
    for entity in plist.get("system-entities", []):
        mp = entity.get("mount-point")
        if mp:
            return Path(mp)

    raise UsbError("Failed to determine ISO mount point from hdiutil output.")


def _unmount_iso_macos(mount_point: Path) -> None:
    subprocess.run(
        ["hdiutil", "detach", str(mount_point), "-force"],
        capture_output=True,
    )


# --- Linux platform ---


def _format_drive_linux(drive: UsbDrive) -> Path:
    for partition in drive.partitions:
        subprocess.run(["sudo", "umount", partition], capture_output=True)

    cmds = [
        ["sudo", "parted", "--script", drive.device, "mklabel", "msdos"],
        ["sudo", "parted", "--script", drive.device, "mkpart", "primary", "fat32", "1MiB", "100%"],
        ["sudo", "parted", "--script", drive.device, "set", "1", "boot", "on"],
    ]
    for cmd in cmds:
        result = subprocess.run(cmd, capture_output=True, text=True)
        if result.returncode != 0:
            raise UsbError(
                f"Failed to partition {drive.device}: {result.stderr.strip()}"
            )

    dev_name = Path(drive.device).name
    if dev_name[-1].isdigit():
        partition_dev = f"{drive.device}p1"
    else:
        partition_dev = f"{drive.device}1"

    result = subprocess.run(
        ["sudo", "mkfs.vfat", "-F", "32", "-n", "WINUSB", partition_dev],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        raise UsbError(f"Failed to format partition: {result.stderr.strip()}")

    mount_point = Path(tempfile.mkdtemp(prefix="winiso_usb_"))
    result = subprocess.run(
        ["sudo", "mount", "-o", f"uid={os.getuid()},gid={os.getgid()}", partition_dev, str(mount_point)],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        shutil.rmtree(mount_point, ignore_errors=True)
        raise UsbError(f"Failed to mount USB partition: {result.stderr.strip()}")

    return mount_point


def _mount_iso_linux(iso_path: Path) -> Path:
    mount_point = Path(tempfile.mkdtemp(prefix="winiso_iso_"))
    result = subprocess.run(
        ["sudo", "mount", "-o", "loop,ro", str(iso_path), str(mount_point)],
        capture_output=True, text=True,
    )
    if result.returncode != 0:
        shutil.rmtree(mount_point, ignore_errors=True)
        raise UsbError(f"Failed to mount ISO: {result.stderr.strip()}")
    return mount_point


def _unmount_iso_linux(mount_point: Path) -> None:
    subprocess.run(["sudo", "umount", str(mount_point)], capture_output=True)
    shutil.rmtree(mount_point, ignore_errors=True)


# --- Cross-platform file operations ---


def _unmount_iso(mount_point: Path) -> None:
    if platform.system() == "Darwin":
        _unmount_iso_macos(mount_point)
    else:
        _unmount_iso_linux(mount_point)


def _scan_files(
    iso_mount: Path,
    skip_wim: Path | None = None,
) -> tuple[int, list[tuple[Path, Path, int]]]:
    total_size = 0
    files: list[tuple[Path, Path, int]] = []

    for src in iso_mount.rglob("*"):
        if not src.is_file():
            continue
        if skip_wim and src == skip_wim:
            continue
        rel = src.relative_to(iso_mount)
        size = src.stat().st_size
        total_size += size
        files.append((src, rel, size))

    return total_size, files


def _copy_files(
    files: list[tuple[Path, Path, int]],
    iso_mount: Path,
    usb_mount: Path,
    on_chunk: Callable[[int], None] | None = None,
) -> None:
    buf_size = 1024 * 1024  # 1 MiB

    for src, rel, _size in files:
        dest = usb_mount / rel
        dest.parent.mkdir(parents=True, exist_ok=True)

        with open(src, "rb") as fin, open(dest, "wb") as fout:
            while True:
                chunk = fin.read(buf_size)
                if not chunk:
                    break
                fout.write(chunk)
                if on_chunk:
                    on_chunk(len(chunk))


def _split_wim(
    wim_path: Path,
    dest_dir: Path,
    on_chunk: Callable[[int], None] | None = None,
) -> None:
    dest_swm = dest_dir / "install.swm"
    wim_size = wim_path.stat().st_size

    proc = subprocess.Popen(
        ["wimlib-imagex", "split", str(wim_path), str(dest_swm), str(WIM_SPLIT_SIZE_MB)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    last_reported = 0
    for line in iter(proc.stdout.readline, b""):
        decoded = line.decode("utf-8", errors="replace").strip()
        if "Part" in decoded and "of" in decoded:
            try:
                parts = decoded.split()
                current = int(parts[parts.index("Part") + 1])
                total = int(parts[parts.index("of") + 1])
                estimated = int(wim_size * current / total)
                delta = estimated - last_reported
                if delta > 0 and on_chunk:
                    on_chunk(delta)
                    last_reported = estimated
            except (ValueError, IndexError):
                pass

    returncode = proc.wait()

    remaining = wim_size - last_reported
    if remaining > 0 and on_chunk:
        on_chunk(remaining)

    if returncode != 0:
        raise UsbError(f"wimlib-imagex split failed (exit code {returncode})")
