"""Protocol A: Download Windows ISOs via the products.cab catalog.

This is the same mechanism the actual Media Creation Tool uses.
No anti-bot (Sentinel) protection -- just download the catalog and parse it.
"""

from __future__ import annotations

import subprocess
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path

import httpx

from .models import DownloadLink, Language

USER_AGENT = "Mozilla/5.0 (X11; Linux x86_64; rv:100.0) Gecko/20100101 Firefox/100.0"

CATALOG_URLS = {
    "windows11": "https://go.microsoft.com/fwlink/?LinkId=2156292",
    "windows10": "https://go.microsoft.com/fwlink/?LinkId=841361",
}


def fetch_catalog(version: str = "windows11") -> list[CatalogEntry]:
    url = CATALOG_URLS.get(version)
    if not url:
        raise ValueError(f"Unknown version: {version}")

    with httpx.Client(headers={"User-Agent": USER_AGENT}, follow_redirects=True, timeout=60.0) as client:
        resp = client.get(url)
        resp.raise_for_status()
        cab_data = resp.content

    with tempfile.TemporaryDirectory() as tmpdir:
        cab_path = Path(tmpdir) / "products.cab"
        cab_path.write_bytes(cab_data)

        xml_path = _extract_cab(cab_path, tmpdir)
        if not xml_path:
            raise RuntimeError("Failed to extract products.xml from catalog")

        return _parse_products_xml(xml_path.read_text(encoding="utf-8"))


def _extract_cab(cab_path: Path, output_dir: str) -> Path | None:
    # Try cabextract (Linux/macOS via homebrew)
    try:
        subprocess.run(
            ["cabextract", "-d", output_dir, str(cab_path)],
            capture_output=True, check=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError):
        # Try Python's built-in cabinet support (Windows) or 7z
        try:
            subprocess.run(
                ["7z", "x", f"-o{output_dir}", str(cab_path)],
                capture_output=True, check=True,
            )
        except (FileNotFoundError, subprocess.CalledProcessError):
            return None

    for p in Path(output_dir).rglob("*.xml"):
        if "products" in p.name.lower():
            return p
    # Fallback: return any XML file
    xmls = list(Path(output_dir).rglob("*.xml"))
    return xmls[0] if xmls else None


class CatalogEntry:
    def __init__(
        self,
        filename: str,
        language_code: str,
        language: str,
        edition: str,
        architecture: str,
        size: int,
        sha1: str,
        file_path: str,
    ) -> None:
        self.filename = filename
        self.language_code = language_code
        self.language = language
        self.edition = edition
        self.architecture = architecture
        self.size = size
        self.sha1 = sha1
        self.file_path = file_path

    @property
    def is_esd(self) -> bool:
        return self.filename.lower().endswith(".esd")


def _parse_products_xml(xml_content: str) -> list[CatalogEntry]:
    root = ET.fromstring(xml_content)

    entries = []
    for file_elem in root.iter("File"):
        filename = _text(file_elem, "FileName", "")
        if not filename:
            continue
        entries.append(
            CatalogEntry(
                filename=filename,
                language_code=_text(file_elem, "LanguageCode", ""),
                language=_text(file_elem, "Language", ""),
                edition=_text(file_elem, "Edition", ""),
                architecture=_text(file_elem, "Architecture", ""),
                size=int(_text(file_elem, "Size", "0")),
                sha1=_text(file_elem, "Sha1", ""),
                file_path=_text(file_elem, "FilePath", ""),
            )
        )
    return entries


def _text(elem: ET.Element, tag: str, default: str) -> str:
    child = elem.find(tag)
    return child.text.strip() if child is not None and child.text else default


def get_languages_from_catalog(entries: list[CatalogEntry], arch: str = "x64") -> list[Language]:
    seen = {}
    for entry in entries:
        if entry.architecture.lower() != arch.lower():
            continue
        if entry.language_code not in seen:
            seen[entry.language_code] = Language(
                id=entry.language_code,
                name=entry.language,
                sku_id=entry.language_code,
                friendly_filename=entry.filename,
            )
    return sorted(seen.values(), key=lambda l: l.name)


def get_download_link_from_catalog(
    entries: list[CatalogEntry], language_code: str, arch: str = "x64"
) -> DownloadLink | None:
    for entry in entries:
        if entry.language_code == language_code and entry.architecture.lower() == arch.lower():
            return DownloadLink(
                url=entry.file_path,
                filename=entry.filename,
                size=entry.size,
                sha256=None,
            )
    return None
