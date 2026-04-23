from __future__ import annotations

import hashlib
import signal
import sys
from pathlib import Path

import httpx
from rich.console import Console
from rich.progress import (
    BarColumn,
    DownloadColumn,
    Progress,
    TextColumn,
    TimeRemainingColumn,
    TransferSpeedColumn,
)

USER_AGENT = "Mozilla/5.0 (X11; Linux x86_64; rv:100.0) Gecko/20100101 Firefox/100.0"
CHUNK_SIZE = 1024 * 256  # 256 KB


def check_existing(
    output_path: Path,
    *,
    sha1: str | None = None,
    console: Console | None = None,
) -> bool:
    """Return True if file already exists and is valid (skip download)."""
    console = console or Console()
    if not output_path.exists():
        return False
    if sha1:
        console.print(f"Found existing [bold]{output_path.name}[/bold], verifying integrity...")
        if _verify_sha1(output_path, sha1, console):
            console.print(f"[green]SHA-1 verified — skipping download.[/green]")
            return True
        partial_path = output_path.with_suffix(output_path.suffix + ".part")
        output_path.rename(partial_path)
        console.print("[yellow]Hash mismatch — resuming download.[/yellow]")
        return False
    console.print(f"[green]{output_path.name} already exists — skipping download.[/green]")
    return True


def _verify_sha1(path: Path, expected: str, console: Console) -> bool:
    sha1 = hashlib.sha1()
    size = path.stat().st_size
    with open(path, "rb") as f, _make_progress(console) as progress:
        task = progress.add_task("Verifying", total=size)
        while True:
            chunk = f.read(CHUNK_SIZE)
            if not chunk:
                break
            sha1.update(chunk)
            progress.update(task, advance=len(chunk))
    return sha1.hexdigest().lower() == expected.lower()


def download_file(
    url: str,
    output_path: Path,
    *,
    expected_size: int | None = None,
    console: Console | None = None,
) -> Path:
    console = console or Console()
    partial_path = output_path.with_suffix(output_path.suffix + ".part")
    existing_size = partial_path.stat().st_size if partial_path.exists() else 0

    headers = {"User-Agent": USER_AGENT}
    if existing_size > 0:
        headers["Range"] = f"bytes={existing_size}-"
        console.print(f"Resuming download from {_fmt_size(existing_size)}...")

    cancelled = False

    def handle_interrupt(sig: int, frame: object) -> None:
        nonlocal cancelled
        cancelled = True

    prev_handler = signal.signal(signal.SIGINT, handle_interrupt)

    try:
        with httpx.stream("GET", url, headers=headers, follow_redirects=True, timeout=60.0) as resp:
            if resp.status_code == 416:
                if partial_path.exists() and (expected_size is None or partial_path.stat().st_size >= expected_size):
                    partial_path.rename(output_path)
                    console.print("[green]Download already complete.[/green]")
                    return output_path
                partial_path.unlink(missing_ok=True)
                existing_size = 0
                return download_file(url, output_path, expected_size=expected_size, console=console)

            resp.raise_for_status()

            total = None
            if "content-length" in resp.headers:
                total = int(resp.headers["content-length"]) + existing_size
            elif expected_size:
                total = expected_size

            mode = "ab" if existing_size > 0 and resp.status_code == 206 else "wb"
            if mode == "wb":
                existing_size = 0

            with open(partial_path, mode) as f, _make_progress(console) as progress:
                task = progress.add_task("Downloading", total=total, completed=existing_size)
                for chunk in resp.iter_bytes(chunk_size=CHUNK_SIZE):
                    if cancelled:
                        console.print("\n[yellow]Download paused. Run again to resume.[/yellow]")
                        sys.exit(130)
                    f.write(chunk)
                    progress.update(task, advance=len(chunk))

        partial_path.rename(output_path)
        console.print(f"[green]Saved to {output_path}[/green]")
        return output_path

    finally:
        signal.signal(signal.SIGINT, prev_handler)


def _make_progress(console: Console) -> Progress:
    return Progress(
        TextColumn("[bold blue]{task.description}"),
        BarColumn(bar_width=40),
        DownloadColumn(),
        TransferSpeedColumn(),
        TimeRemainingColumn(),
        console=console,
        transient=False,
    )


def _fmt_size(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024:
            return f"{n:.1f} {unit}"
        n /= 1024  # type: ignore[assignment]
    return f"{n:.1f} TB"
