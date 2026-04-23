# winiso

A single-binary tool for downloading Windows ISOs from Microsoft and creating bootable USB drives. No external dependencies required — everything is handled natively in Rust.

## Features

- **Download Windows 10/11 ISOs** directly from Microsoft's servers
- **Create bootable USB drives** with FAT32 + MBR (UEFI + legacy BIOS compatible)
- **Automatic WIM splitting** for install.wim files over 4 GB (FAT32 limit)
- **Resumable downloads** with SHA-1 verification
- **Interactive wizard** or scriptable CLI
- **Cross-platform**: macOS and Linux

## Usage

### Interactive mode

```
winiso
```

Walks you through selecting a Windows version, language, and USB drive.

### Download only

```
# List available languages
winiso list --version 11

# Download via ESD catalog (default, smaller files)
winiso download --version 11 --lang en-us

# Download official ISO
winiso download --version 11 --lang English --iso
```

### Create bootable USB

```
# From a local ISO
winiso burn /path/to/windows.iso

# Download and burn in one step
winiso burn --version 11 --lang English

# Specify drive and skip confirmation
winiso burn windows.iso --drive /dev/disk2 --yes

# Use GPT partition table (UEFI only)
winiso burn windows.iso --gpt

# Raw write (dd-style, no FAT32)
winiso burn windows.iso --raw
```

## Building

```
cargo build --release
```

The release binary is ~2 MB with all optimizations enabled.

### Build targets

| Platform | Target |
|----------|--------|
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| Linux ARM64 | `aarch64-unknown-linux-musl` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |

## How it works

1. **ISO reading**: Custom UDF + ISO 9660/Joliet parser reads Windows ISOs directly — no mounting required
2. **Disk operations**: MBR partition tables via `mbrman`, FAT32 formatting and file writing via `fatfs` — no `mkfs.vfat` or `parted` needed
3. **WIM splitting**: Parses the WIM binary format and redistributes compressed resources across multiple .swm files under 4 GB each
4. **Privilege escalation**: Runs as a normal user, only invoking `sudo` when raw device access is needed for writing

## License

See [LICENSE](../LICENSE).
