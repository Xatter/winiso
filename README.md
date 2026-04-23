# winiso

Cross-platform command-line tool to download Windows installation media directly from Microsoft. No Windows required.

Microsoft's [Media Creation Tool](https://www.microsoft.com/software-download/windows11) only runs on Windows — if you're on macOS or Linux and need a Windows ISO, you're stuck. **winiso** fixes that.

## Install

```bash
git clone https://github.com/Xatter/winiso.git
cd winiso
python3 -m venv .venv && source .venv/bin/activate
pip install -e .
```

Then install the system dependencies listed under [Requirements](#requirements).

## Usage

### Interactive wizard

```bash
winiso
```

Walks you through the entire process: select Windows version, architecture, language, pick your USB drive, then downloads the ISO and writes it to the drive. If no USB drive is detected, it downloads the ISO only.

### List available downloads

```bash
winiso list                             # Show available versions
winiso list --version 11                # Languages for Windows 11 x64
winiso list --version 11 --arch ARM64   # Languages for ARM64
winiso list --version 10                # Windows 10
winiso list --version 11 --json         # Machine-readable output
```

### Download

```bash
winiso download --version 11 --lang en-us                  # English x64 (default)
winiso download --version 11 --lang Japanese --arch ARM64  # Japanese ARM64
winiso download --version 10 --lang de-de -o ~/Downloads   # German Win10, custom dir
```

Downloads are resumable — if interrupted, just run the same command again.

### Create bootable USB

```bash
winiso burn Win11_24H2_English_x64.iso              # Write ISO to USB (interactive drive selection)
winiso burn --version 11 --lang en-us                # Download ISO and burn in one step
winiso burn Win11.iso --drive /dev/disk2             # Specify drive directly
winiso burn Win11.iso --raw                          # Raw dd write (advanced, may not boot)
```

Formats the USB as MBR + FAT32 and copies the Windows installation files, producing a drive that boots on both UEFI and legacy BIOS systems. If the ISO contains an `install.wim` larger than 4 GB (common with Windows 10/11), it automatically splits it using [wimlib](https://wimlib.net/) so it fits on FAT32.

Lists only physical removable drives, requires you to type the device name to confirm before writing. Supports macOS and Linux.

## How it works

winiso uses the same `products.cab` catalog that Microsoft's own Media Creation Tool uses internally. It:

1. Fetches the signed product catalog from Microsoft's CDN (`go.microsoft.com/fwlink`)
2. Parses `products.xml` to find available editions, languages, and architectures
3. Downloads ESD files directly from Microsoft's servers

The catalog approach is the same mechanism used by the official MCT binary — we verified this by reverse-engineering `MediaCreationTool.exe` (see [analysis notes](docs/binary-analysis.md) if interested).

### ESD vs ISO

The default download format is **ESD** (Electronic Software Download) — the same format Microsoft's servers provide to the Media Creation Tool. ESDs are compressed Windows installation images.

To convert an ESD to a bootable ISO, use [wimlib](https://wimlib.net/):

```bash
brew install wimlib   # macOS
# Then: wimlib-imagex export source.esd 1 dest.wim
```

For direct ISO downloads (bypasses ESD), use the `--iso` flag — this uses a different Microsoft API that may be less reliable due to anti-bot protection.

## Supported platforms

- **macOS** (Intel and Apple Silicon)
- **Linux** (any distro with Python 3.10+)
- **Windows** (works too, but you probably have the real MCT)

## Requirements

- Python 3.10+
- `cabextract` (for parsing Microsoft's product catalog)
- `wimlib` (optional — only needed if `install.wim` in the ISO exceeds 4 GB)

```bash
# macOS
brew install cabextract wimlib

# Ubuntu/Debian
sudo apt install cabextract wimlib-tools
```

## Prior art

This project builds on protocol research from:
- [Fido](https://github.com/pbatard/Fido) by Pete Batard (PowerShell, Windows-only)
- [Mido](https://github.com/ElliotKillick/Mido) by Elliot Killick (POSIX shell)
- [UUP dump](https://uupdump.net/) (web service)

winiso aims to be a proper cross-platform CLI tool with good UX, resume support, and error handling — filling a gap in the ecosystem where only shell scripts existed.

## License

MIT
