from __future__ import annotations

import sys
from pathlib import Path

from rich.console import Console
from rich.panel import Panel
from rich.progress import BarColumn, DownloadColumn, Progress, TransferSpeedColumn

from .models import UsbDrive
from .usb import UsbError, write_iso_to_drive


def resolve_iso_source(
    iso_path: Path | None,
    version: str | None,
    lang: str | None,
    arch: str,
    output_dir: Path,
    console: Console,
) -> Path:
    if iso_path is not None:
        iso_path = iso_path.expanduser().resolve()
        if not iso_path.exists():
            console.print(f"[red]File not found: {iso_path}[/red]")
            sys.exit(1)
        if iso_path.suffix.lower() == ".esd":
            console.print("[red]ESD files cannot be written directly to USB.[/red]")
            console.print("Download an ISO instead:")
            console.print("  [bold]winiso download --version 11 --iso[/bold]")
            console.print("Or convert with wimlib:")
            console.print("  [bold]wimlib-imagex export source.esd 1 dest.wim[/bold]")
            sys.exit(1)
        return iso_path

    if version:
        console.print(f"Downloading ISO for {version} ({arch})...\n")
        return _download_iso_for_burn(version, arch, lang, output_dir, console)

    console.print("[red]Provide an ISO file or --version to auto-download.[/red]")
    console.print("Usage: [bold]winiso burn image.iso[/bold]")
    console.print("       [bold]winiso burn --version 11 --lang en-us[/bold]")
    sys.exit(1)


def _download_iso_for_burn(
    version_key: str, arch: str, lang: str | None, output_dir: Path, console: Console,
) -> Path:
    from .api import PRODUCTS, APIError, MicrosoftDownloadAPI
    from .download import check_existing, download_file

    product = PRODUCTS.get(version_key)
    if not product:
        console.print(f"[red]Unknown version: {version_key}[/red]")
        sys.exit(1)

    edition = next((e for e in product["editions"] if e["arch"] == arch), product["editions"][0])

    with MicrosoftDownloadAPI() as api:
        console.print(f"Fetching languages for [bold]{edition['name']}[/bold]...")
        try:
            languages = api.get_languages(edition["id"])
        except APIError as e:
            console.print(f"[red]API error: {e.message}[/red]")
            console.print("[dim]Download an ISO manually: winiso download --version 11 --iso[/dim]")
            sys.exit(1)

        if lang:
            lf = lang.lower()
            language = next(
                (l for l in languages if lf in l.name.lower() or lf == l.id.lower()),
                None,
            )
            if not language:
                console.print(f"[red]Language '{lang}' not found.[/red]")
                sys.exit(1)
        else:
            language = next(
                (l for l in languages if l.id == "en-us" or "english" in l.name.lower()),
                languages[0],
            )

        console.print(f"Fetching download link for [bold]{language.name}[/bold]...")
        try:
            links = api.get_download_links(language.sku_id, product["segment"])
        except APIError as e:
            console.print(f"[red]API error: {e.message}[/red]")
            console.print("[dim]Microsoft's anti-bot may have blocked the request.[/dim]")
            console.print("[dim]Download an ISO manually: winiso download --version 11 --iso[/dim]")
            sys.exit(1)

    if not links:
        console.print("[red]No download links returned.[/red]")
        sys.exit(1)

    link = links[0]
    filename = language.friendly_filename or link.filename
    output_path = output_dir / filename
    if not check_existing(output_path, sha1=link.sha1, console=console):
        console.print(f"\nDownloading [bold]{filename}[/bold]...\n")
        download_file(link.url, output_path, console=console)
    return output_path


def confirm_burn(drive: UsbDrive, iso_path: Path, console: Console) -> bool:
    size_gb = drive.size / (1024 ** 3)
    iso_size_gb = iso_path.stat().st_size / (1024 ** 3)

    warning = (
        f"[bold red]ALL DATA ON THIS DRIVE WILL BE ERASED[/bold red]\n\n"
        f"  Drive:    [bold]{drive.name}[/bold]\n"
        f"  Device:   {drive.device}\n"
        f"  Size:     {size_gb:.1f} GB\n\n"
        f"  ISO:      {iso_path.name}\n"
        f"  ISO Size: {iso_size_gb:.1f} GB"
    )
    console.print()
    console.print(Panel(warning, title="[bold red]WARNING[/bold red]", border_style="red"))

    short_name = drive.device.split("/")[-1]
    console.print(f"\nTo confirm, type [bold]{short_name}[/bold] (or 'cancel' to abort)")
    answer = console.input(f"[bold]Confirm[/bold] [{short_name}]: ").strip()

    return answer == short_name


def burn_iso(iso_path: Path, drive: UsbDrive, console: Console, *, raw: bool = False) -> None:
    if raw:
        _burn_raw(iso_path, drive, console)
        return

    from .bootusb import create_bootable_usb, preflight_check

    try:
        preflight_check(iso_path, drive)
    except UsbError as e:
        console.print(f"\n[red]{e}[/red]")
        sys.exit(1)

    console.print("\n[dim]Administrator privileges (sudo) are required.[/dim]")
    console.print("[dim]Do not remove the USB drive during this process.[/dim]\n")

    progress = Progress(
        "[progress.description]{task.description}",
        BarColumn(),
        DownloadColumn(),
        TransferSpeedColumn(),
        console=console,
    )

    with progress:
        current_task = None
        current_phase: str | None = None

        def on_phase(phase: str, completed: int, total: int) -> None:
            nonlocal current_task, current_phase
            if phase != current_phase:
                if current_task is not None:
                    progress.update(current_task, completed=progress.tasks[current_task].total)
                current_task = progress.add_task(phase, total=total)
                current_phase = phase
            progress.update(current_task, completed=completed, total=total)

        try:
            create_bootable_usb(iso_path, drive, on_phase=on_phase)
        except UsbError as e:
            console.print(f"\n[red]Failed: {e}[/red]")
            sys.exit(1)
        except KeyboardInterrupt:
            console.print("\n[yellow]Interrupted. Cleaning up...[/yellow]")
            sys.exit(1)

    console.print(f"\n[bold green]Done![/bold green] Bootable USB created from {iso_path.name}")
    console.print("[dim]You can now safely remove the USB drive.[/dim]")


def _burn_raw(iso_path: Path, drive: UsbDrive, console: Console) -> None:
    iso_size = iso_path.stat().st_size
    if iso_size > drive.size:
        iso_gb = iso_size / (1024 ** 3)
        drive_gb = drive.size / (1024 ** 3)
        console.print(f"[red]ISO ({iso_gb:.1f} GB) is larger than drive ({drive_gb:.1f} GB).[/red]")
        sys.exit(1)

    console.print("\n[dim]Administrator privileges (sudo) are required to write to the drive.[/dim]")
    console.print("[dim]Do not remove the USB drive during writing.[/dim]\n")

    progress = Progress(
        "[progress.description]{task.description}",
        BarColumn(),
        DownloadColumn(),
        TransferSpeedColumn(),
        console=console,
    )

    with progress:
        task = progress.add_task("Writing (raw)", total=iso_size)

        def on_progress(written: int, total: int) -> None:
            progress.update(task, completed=written)

        try:
            write_iso_to_drive(iso_path, drive, on_progress=on_progress)
        except UsbError as e:
            console.print(f"\n[red]Write failed: {e}[/red]")
            if "exit code" in str(e):
                console.print("[dim]Make sure you have sudo access and the drive is not in use.[/dim]")
            sys.exit(1)
        except KeyboardInterrupt:
            console.print("\n[yellow]Write interrupted. The USB drive may be in an unusable state.[/yellow]")
            sys.exit(1)

    console.print(f"\n[bold green]Done![/bold green] {iso_path.name} written to {drive.device}")
    console.print("[dim]You can now safely remove the USB drive.[/dim]")
