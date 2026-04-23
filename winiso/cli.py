from __future__ import annotations

import argparse
import sys
from pathlib import Path

from rich.console import Console
from rich.table import Table

from .api import PRODUCTS, APIError, MicrosoftDownloadAPI
from .catalog import (
    fetch_catalog,
    get_download_link_from_catalog,
    get_languages_from_catalog,
)
from .download import check_existing, download_file
from .models import DownloadLink, Language

console = Console()

ARCH_ALIASES = {"x64": "x64", "x86_64": "x64", "amd64": "x64", "arm64": "ARM64", "aarch64": "ARM64"}


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(
        prog="winiso",
        description="Download Windows ISOs from Microsoft - cross-platform",
    )
    sub = parser.add_subparsers(dest="command")

    list_parser = sub.add_parser("list", help="List available products and languages")
    list_parser.add_argument("--version", choices=["10", "11"], help="Show languages for this version")
    list_parser.add_argument("--arch", default="x64", help="Architecture: x64 or ARM64 (default: x64)")
    list_parser.add_argument("--json", action="store_true", help="Output as JSON")

    dl_parser = sub.add_parser("download", help="Download a Windows ISO")
    dl_parser.add_argument("--version", choices=["10", "11"], help="Windows version")
    dl_parser.add_argument("--lang", help="Language (e.g. 'English' or 'en-us')")
    dl_parser.add_argument("--arch", default="x64", help="Architecture: x64 or ARM64 (default: x64)")
    dl_parser.add_argument("-o", "--output", type=Path, default=Path("."), help="Output directory")
    dl_parser.add_argument("--iso", action="store_true", help="Use ISO API (default: ESD from catalog)")

    burn_parser = sub.add_parser("burn", help="Write an ISO to a USB drive")
    burn_parser.add_argument("iso", nargs="?", type=Path, help="Path to ISO file")
    burn_parser.add_argument("--version", choices=["10", "11"], help="Windows version (downloads ISO if no file given)")
    burn_parser.add_argument("--lang", help="Language for auto-download")
    burn_parser.add_argument("--arch", default="x64", help="Architecture (default: x64)")
    burn_parser.add_argument("--drive", help="Target drive (e.g. /dev/disk2, /dev/sdb)")
    burn_parser.add_argument("-y", "--yes", action="store_true", help="Skip confirmation (DANGEROUS)")
    burn_parser.add_argument("--raw", action="store_true", help="Write ISO as raw image (dd) instead of creating bootable FAT32 USB")

    args = parser.parse_args(argv)

    if args.command is None:
        _interactive_flow()
    elif args.command == "list":
        _list_command(args)
    elif args.command == "download":
        _download_command(args)
    elif args.command == "burn":
        _burn_command(args)


def _normalize_arch(arch: str) -> str:
    return ARCH_ALIASES.get(arch.lower(), arch)


# --- Interactive flow ---


def _interactive_flow() -> None:
    from .burn import burn_iso, confirm_burn
    from .usb import UsbError, list_usb_drives

    console.print("[bold]winiso[/bold] - Windows ISO Downloader\n")

    # Step 1-3: Version, arch, language
    version_key = _pick_version()
    arch = _pick_arch()

    product = PRODUCTS[version_key]
    edition = next((e for e in product["editions"] if e["arch"] == arch), product["editions"][0])

    console.print(f"\nFetching available languages...")
    with MicrosoftDownloadAPI() as api:
        try:
            languages = api.get_languages(edition["id"])
        except APIError as e:
            console.print(f"[red]API error: {e.message}[/red]")
            sys.exit(1)

        if not languages:
            console.print(f"[red]No languages found for {arch}[/red]")
            sys.exit(1)

        language = _pick_language(languages)

        # Step 4: Detect USB drives
        console.print("\nLooking for USB drives...")
        try:
            drives = list_usb_drives()
        except UsbError as e:
            console.print(f"[red]{e}[/red]")
            sys.exit(1)

        if not drives:
            console.print("[yellow]No USB drives found.[/yellow]")
            console.print("[dim]Insert a USB drive and press Enter to retry, or type 'skip' to download only.[/dim]")
            while True:
                answer = console.input("[bold]>[/bold] ").strip().lower()
                if answer == "skip":
                    break
                try:
                    drives = list_usb_drives()
                except UsbError:
                    pass
                if drives:
                    break
                console.print("[dim]Still no drives found. Try again or type 'skip'.[/dim]")

        drive = _pick_drive(drives) if drives else None

        # Step 5: Confirm
        if drive:
            size_gb = drive.size / (1024 ** 3)
            console.print(f"\n[bold]Summary:[/bold]")
            console.print(f"  Windows:  {product['name']} ({arch})")
            console.print(f"  Language: {language.name}")
            console.print(f"  Drive:    {drive.name} ({size_gb:.1f} GB) — {drive.device}")
            console.print(f"\n  Will download ISO, then write to USB.\n")
        else:
            console.print(f"\n[bold]Summary:[/bold]")
            console.print(f"  Windows:  {product['name']} ({arch})")
            console.print(f"  Language: {language.name}")
            console.print(f"\n  Will download ISO to current directory.\n")

        # Step 6: Get download link
        console.print("Fetching download link...")
        try:
            links = api.get_download_links(language.sku_id, product["segment"])
        except APIError as e:
            console.print(f"[red]API error: {e.message}[/red]")
            console.print("[dim]Microsoft's anti-bot may have blocked the request. Try again later.[/dim]")
            sys.exit(1)

    if not links:
        console.print("[red]No download links returned.[/red]")
        sys.exit(1)

    link = links[0]
    filename = language.friendly_filename or link.filename
    output_path = Path.cwd() / filename

    # Step 7: Download (skip if already present)
    if not check_existing(output_path, sha1=link.sha1, console=console):
        console.print(f"\n[bold]Downloading {filename}[/bold]\n")
        download_file(link.url, output_path, console=console)

    # Step 8: Burn (if drive selected)
    if drive:
        console.print()
        if not confirm_burn(drive, output_path, console):
            console.print("Burn cancelled. ISO saved to: [bold]{output_path}[/bold]")
            return
        burn_iso(output_path, drive, console)
    else:
        console.print(f"\nISO saved to: [bold]{output_path}[/bold]")


def _pick_version() -> str:
    console.print("Select Windows version:\n")
    options = [("windows11", "Windows 11"), ("windows10", "Windows 10")]
    for i, (_, name) in enumerate(options, 1):
        console.print(f"  [bold cyan]{i}[/bold cyan]) {name}")
    console.print()
    while True:
        choice = console.input("[bold]Choice[/bold] [1]: ").strip()
        if choice in ("", "1"):
            return options[0][0]
        if choice == "2":
            return options[1][0]
        console.print("[red]Please enter 1 or 2[/red]")


def _pick_arch() -> str:
    console.print("\nSelect architecture:\n")
    options = ["x64", "ARM64"]
    for i, arch in enumerate(options, 1):
        console.print(f"  [bold cyan]{i}[/bold cyan]) {arch}")
    console.print()
    while True:
        choice = console.input("[bold]Choice[/bold] [1]: ").strip()
        if choice in ("", "1"):
            return options[0]
        if choice == "2":
            return options[1]
        console.print("[red]Please enter 1 or 2[/red]")


def _pick_language(languages: list[Language]) -> Language:
    english_first = sorted(
        languages,
        key=lambda l: (
            0 if l.id == "en-us" else
            1 if "en-" in l.id else 2,
            l.name,
        ),
    )
    console.print(f"\nSelect language ({len(english_first)} available):\n")
    for i, lang in enumerate(english_first, 1):
        console.print(f"  [bold cyan]{i:>3}[/bold cyan]) {lang.name}")
    console.print()
    while True:
        choice = console.input("[bold]Choice[/bold] [1]: ").strip()
        if choice == "":
            return english_first[0]
        try:
            idx = int(choice) - 1
            if 0 <= idx < len(english_first):
                return english_first[idx]
        except ValueError:
            pass
        console.print(f"[red]Please enter a number between 1 and {len(english_first)}[/red]")


def _pick_output_dir() -> Path:
    default = Path.cwd()
    path_str = console.input(f"\n[bold]Save to directory[/bold] [{default}]: ").strip()
    if not path_str:
        return default
    p = Path(path_str).expanduser().resolve()
    p.mkdir(parents=True, exist_ok=True)
    return p


def _print_download_info(link: DownloadLink) -> None:
    console.print(f"\n[bold]{link.filename}[/bold]")
    if link.size:
        gb = link.size / 1024 / 1024 / 1024
        console.print(f"Size: {gb:.1f} GB")
    if link.filename.endswith(".esd"):
        console.print("[dim]Format: ESD (Windows installation image)[/dim]")
        console.print("[dim]To convert to ISO, use wimlib: wimlib-imagex export ...[/dim]")
    console.print()


# --- List command ---


def _list_command(args: argparse.Namespace) -> None:
    arch = _normalize_arch(args.arch)

    if args.version is None:
        table = Table(title="Available Windows Versions")
        table.add_column("Version", style="bold")
        table.add_column("Architectures")
        for key, prod in PRODUCTS.items():
            archs = ", ".join(e["arch"] for e in prod["editions"])
            table.add_row(key.replace("windows", ""), archs)
        console.print(table)
        console.print("\nUse [bold]winiso list --version 11[/bold] to see available languages.")
        return

    version_key = f"windows{args.version}"
    if not args.json:
        console.print(f"Fetching catalog for Windows {args.version} ({arch})...")

    try:
        entries = fetch_catalog(version_key)
    except Exception as e:
        console.print(f"[red]Error: {e}[/red]", stderr=True)
        sys.exit(1)

    languages = get_languages_from_catalog(entries, arch)

    if args.json:
        import json
        data = [{"name": l.name, "code": l.id, "filename": l.friendly_filename} for l in languages]
        print(json.dumps(data, indent=2))
        return

    table = Table(title=f"Languages for Windows {args.version} ({arch})")
    table.add_column("#", style="dim")
    table.add_column("Language", style="bold")
    table.add_column("Code", style="dim")
    table.add_column("Filename")
    for i, lang in enumerate(languages, 1):
        table.add_row(str(i), lang.name, lang.id, lang.friendly_filename or "")
    console.print(table)


# --- Download command ---


def _download_command(args: argparse.Namespace) -> None:
    if not args.version:
        console.print("[red]--version is required for non-interactive download[/red]")
        console.print("Usage: [bold]winiso download --version 11 --lang en-us[/bold]")
        sys.exit(1)

    version_key = f"windows{args.version}"
    arch = _normalize_arch(args.arch)
    output_dir = args.output.expanduser().resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    if args.iso:
        _download_iso(version_key, arch, args.lang, output_dir)
    else:
        _download_esd(version_key, arch, args.lang, output_dir)


def _download_esd(version_key: str, arch: str, lang_filter: str | None, output_dir: Path) -> None:
    console.print(f"Fetching catalog...")
    entries = fetch_catalog(version_key)
    languages = get_languages_from_catalog(entries, arch)

    language = _resolve_language(languages, lang_filter)
    link = get_download_link_from_catalog(entries, language.id, arch)
    if not link:
        console.print("[red]No download link found.[/red]")
        sys.exit(1)

    output_path = output_dir / link.filename
    if check_existing(output_path, sha1=link.sha1, console=console):
        return
    _print_download_info(link)
    download_file(link.url, output_path, expected_size=link.size, console=console)


def _download_iso(version_key: str, arch: str, lang_filter: str | None, output_dir: Path) -> None:
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
            console.print("[dim]Try without --iso flag to use the catalog instead.[/dim]")
            sys.exit(1)

        language = _resolve_language(languages, lang_filter)

        console.print(f"Fetching download link for [bold]{language.name}[/bold]...")
        try:
            links = api.get_download_links(language.sku_id, product["segment"])
        except APIError as e:
            console.print(f"[red]API error: {e.message}[/red]")
            console.print("[dim]Microsoft's anti-bot may have blocked the request. Try without --iso.[/dim]")
            sys.exit(1)

    if not links:
        console.print("[red]No download links returned.[/red]")
        sys.exit(1)

    link = links[0]
    filename = language.friendly_filename or link.filename
    output_path = output_dir / filename
    if check_existing(output_path, sha1=link.sha1, console=console):
        return
    console.print(f"\nDownloading [bold]{filename}[/bold]...\n")
    download_file(link.url, output_path, console=console)


def _resolve_language(languages: list[Language], lang_filter: str | None) -> Language:
    if lang_filter:
        lf = lang_filter.lower()
        match = next(
            (l for l in languages if lf in l.name.lower() or lf == l.id.lower()),
            None,
        )
        if not match:
            console.print(f"[red]Language '{lang_filter}' not found.[/red]")
            console.print("Available: " + ", ".join(f"{l.name} ({l.id})" for l in languages[:10]) + "...")
            sys.exit(1)
        return match

    return next(
        (l for l in languages if l.id == "en-us" or "english" in l.name.lower()),
        languages[0],
    )


# --- Burn command ---


def _burn_command(args: argparse.Namespace) -> None:
    from .burn import burn_iso, confirm_burn, resolve_iso_source
    from .models import UsbDrive
    from .usb import UsbError, list_usb_drives

    arch = _normalize_arch(args.arch)
    output_dir = Path.cwd()

    iso_path = resolve_iso_source(
        iso_path=args.iso,
        version=f"windows{args.version}" if args.version else None,
        lang=args.lang,
        arch=arch,
        output_dir=output_dir,
        console=console,
    )

    try:
        drives = list_usb_drives()
    except UsbError as e:
        console.print(f"[red]{e}[/red]")
        sys.exit(1)

    if not drives:
        console.print("[red]No removable USB drives found.[/red]")
        console.print("[dim]Insert a USB drive and try again.[/dim]")
        sys.exit(1)

    if args.drive:
        drive = next((d for d in drives if d.device == args.drive), None)
        if not drive:
            console.print(f"[red]Drive {args.drive} not found or not removable.[/red]")
            console.print("Available drives: " + ", ".join(d.device for d in drives))
            sys.exit(1)
    else:
        drive = _pick_drive(drives)

    if not args.yes:
        if not confirm_burn(drive, iso_path, console):
            console.print("Cancelled.")
            sys.exit(0)

    burn_iso(iso_path, drive, console, raw=args.raw)


def _pick_drive(drives: list) -> object:
    console.print("\nAvailable USB drives:\n")
    table = Table()
    table.add_column("#", style="bold cyan")
    table.add_column("Device", style="dim")
    table.add_column("Name")
    table.add_column("Size")
    for i, d in enumerate(drives, 1):
        size_gb = d.size / (1024 ** 3)
        table.add_row(str(i), d.device, d.name, f"{size_gb:.1f} GB")
    console.print(table)
    console.print()
    while True:
        choice = console.input("[bold]Select drive[/bold]: ").strip()
        try:
            idx = int(choice) - 1
            if 0 <= idx < len(drives):
                return drives[idx]
        except ValueError:
            pass
        console.print(f"[red]Please enter a number between 1 and {len(drives)}[/red]")
