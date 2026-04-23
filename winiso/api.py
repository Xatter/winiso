from __future__ import annotations

import re
import time
import uuid
from dataclasses import dataclass

import httpx

from .models import DownloadLink, Language

USER_AGENT = "Mozilla/5.0 (X11; Linux x86_64; rv:100.0) Gecko/20100101 Firefox/100.0"
PROFILE = "606624d44113"
BASE_URL = "https://www.microsoft.com/software-download-connector/api"
INSTANCE_ID = "560dc9f3-1aa5-4a2f-b63c-9e18f8d0e175"

PRODUCTS = {
    "windows11": {
        "name": "Windows 11",
        "editions": [
            {"id": "3321", "name": "Windows 11 (x64)", "arch": "x64"},
            {"id": "3324", "name": "Windows 11 (ARM64)", "arch": "ARM64"},
        ],
        "segment": "windows11",
    },
    "windows10": {
        "name": "Windows 10",
        "editions": [
            {"id": "2618", "name": "Windows 10 (x64)", "arch": "x64"},
        ],
        "segment": "windows10ISO",
    },
}


@dataclass
class APIError(Exception):
    message: str
    details: str = ""


class MicrosoftDownloadAPI:
    """Protocol B: software-download-connector JSON API.

    Returns direct ISO download links. Requires anti-bot (Sentinel)
    verification via ov-df.microsoft.com before requesting download URLs.
    """

    def __init__(self) -> None:
        self.session_id = str(uuid.uuid4())
        self._client = httpx.Client(
            headers={"User-Agent": USER_AGENT},
            follow_redirects=True,
            timeout=30.0,
            http1=True,
            http2=False,
        )
        self._verified = False

    def _ensure_verified(self) -> None:
        if self._verified:
            return

        self._client.get(
            f"https://vlscppe.microsoft.com/tags?org_id=y6jn8c31&session_id={self.session_id}"
        ).raise_for_status()

        resp = self._client.get(
            "https://ov-df.microsoft.com/mdt.js",
            params={
                "instanceId": INSTANCE_ID,
                "PageId": "si",
                "session_id": self.session_id,
            },
        )
        resp.raise_for_status()
        mdt_js = resp.text

        url_match = re.search(r'url:"(https://ov-df\.microsoft\.com/[^"]+)"', mdt_js)
        rticks_match = re.search(r'rticks="\+(\d+)', mdt_js)

        if not url_match or not rticks_match:
            raise APIError(
                "Failed to parse anti-bot verification response",
                f"Could not extract verification tokens from mdt.js (length={len(mdt_js)})",
            )

        reply_url = f"{url_match.group(1)}&mdt={int(time.time() * 1000)}&rticks={rticks_match.group(1)}"
        self._client.get(reply_url).raise_for_status()
        self._verified = True

    def get_languages(self, product_edition_id: str) -> list[Language]:
        self._ensure_verified()
        resp = self._client.get(
            f"{BASE_URL}/getskuinformationbyproductedition",
            params={
                "profile": PROFILE,
                "productEditionId": product_edition_id,
                "SKU": "undefined",
                "friendlyFileName": "undefined",
                "Locale": "en-US",
                "sessionID": self.session_id,
            },
        )
        resp.raise_for_status()
        data = resp.json()

        if errors := data.get("Errors"):
            raise APIError("API returned errors", str(errors))

        languages = []
        for sku in data.get("Skus", []):
            filenames = sku.get("FriendlyFileNames", [])
            languages.append(
                Language(
                    id=sku.get("Language", ""),
                    name=sku.get("LocalizedLanguage", sku.get("Language", "")),
                    sku_id=str(sku.get("Id", "")),
                    friendly_filename=filenames[0] if filenames else None,
                )
            )
        return languages

    def get_download_links(self, sku_id: str, product_segment: str = "windows11") -> list[DownloadLink]:
        self._ensure_verified()
        referer = f"https://www.microsoft.com/software-download/{product_segment}"
        resp = self._client.get(
            f"{BASE_URL}/GetProductDownloadLinksBySku",
            params={
                "profile": PROFILE,
                "productEditionId": "undefined",
                "SKU": sku_id,
                "friendlyFileName": "undefined",
                "Locale": "en-US",
                "sessionID": self.session_id,
            },
            headers={"Referer": referer},
        )
        resp.raise_for_status()
        data = resp.json()

        if errors := data.get("Errors"):
            raise APIError("API returned errors", str(errors))

        links = []
        for option in data.get("ProductDownloadOptions", []):
            uri = option.get("Uri", "")
            if not uri:
                continue
            filename = uri.split("/")[-1].split("?")[0] if "/" in uri else "windows.iso"
            links.append(DownloadLink(url=uri, filename=filename))
        return links

    def close(self) -> None:
        self._client.close()

    def __enter__(self) -> MicrosoftDownloadAPI:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()
