from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class Product:
    id: str
    name: str
    description: str = ""


@dataclass
class Language:
    id: str
    name: str
    sku_id: str
    friendly_filename: str | None = None


@dataclass
class DownloadLink:
    url: str
    filename: str
    size: int | None = None
    sha256: str | None = None
