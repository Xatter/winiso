pub mod interactive;
pub mod progress;

use std::path::{Path, PathBuf};
use std::process;

use clap::{Parser, Subcommand};

use crate::burn;
use crate::disk::PartitionScheme;
use crate::download;
use crate::microsoft::api::MicrosoftDownloadAPI;
use crate::microsoft::catalog;
use crate::microsoft::models as products;
use crate::usb;

const ARCH_ALIASES: &[(&str, &str)] = &[
    ("x64", "x64"),
    ("x86_64", "x64"),
    ("amd64", "x64"),
    ("arm64", "ARM64"),
    ("aarch64", "ARM64"),
];

fn normalize_arch(arch: &str) -> String {
    ARCH_ALIASES
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(arch))
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| arch.to_string())
}

#[derive(Parser)]
#[command(name = "winiso", about = "Download Windows ISOs from Microsoft")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// List available products and languages
    List {
        /// Windows version (10 or 11)
        #[arg(long)]
        version: Option<String>,
        /// Architecture: x64 or ARM64
        #[arg(long, default_value = "x64")]
        arch: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Download a Windows ISO
    Download {
        /// Windows version (10 or 11)
        #[arg(long)]
        version: Option<String>,
        /// Language (e.g. 'English' or 'en-us')
        #[arg(long)]
        lang: Option<String>,
        /// Architecture: x64 or ARM64
        #[arg(long, default_value = "x64")]
        arch: String,
        /// Output directory
        #[arg(short, long, default_value = ".")]
        output: PathBuf,
        /// Use ISO API (default: ESD from catalog)
        #[arg(long)]
        iso: bool,
    },
    /// Write an ISO to a USB drive
    Burn {
        /// Path to ISO file
        iso: Option<PathBuf>,
        /// Windows version (downloads ISO if no file given)
        #[arg(long)]
        version: Option<String>,
        /// Language for auto-download
        #[arg(long)]
        lang: Option<String>,
        /// Architecture
        #[arg(long, default_value = "x64")]
        arch: String,
        /// Target drive (e.g. /dev/disk2, /dev/sdb)
        #[arg(long)]
        drive: Option<String>,
        /// Use GPT instead of MBR
        #[arg(long)]
        gpt: bool,
        /// Write ISO as raw image instead of creating bootable FAT32 USB
        #[arg(long)]
        raw: bool,
        /// Skip confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Internal: privileged burn invoked via sudo (not user-facing)
    #[command(name = "_burn-device", hide = true)]
    BurnDevice {
        #[arg(long)]
        iso: PathBuf,
        #[arg(long)]
        device: String,
        #[arg(long)]
        device_name: String,
        #[arg(long)]
        device_size: u64,
        #[arg(long)]
        gpt: bool,
        #[arg(long)]
        raw: bool,
    },
}

pub fn run() {
    let cli = Cli::parse();

    let result = match cli.command {
        None => interactive_flow(),
        Some(Commands::List { version, arch, json }) => list_command(version, &arch, json),
        Some(Commands::Download {
            version,
            lang,
            arch,
            output,
            iso,
        }) => download_command(version, lang, &arch, &output, iso),
        Some(Commands::Burn {
            iso,
            version,
            lang,
            arch,
            drive,
            gpt,
            raw,
            yes,
        }) => burn_command(iso, version, lang, &arch, drive, gpt, raw, yes),
        Some(Commands::BurnDevice {
            iso,
            device,
            device_name,
            device_size,
            gpt,
            raw,
        }) => burn_device_command(iso, device, device_name, device_size, gpt, raw),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

fn interactive_flow() -> crate::error::Result<()> {
    eprintln!("winiso - Windows ISO Downloader\n");

    let version_key = interactive::pick_version();
    let arch = interactive::pick_arch();

    let product = products::get_product(version_key)
        .ok_or_else(|| crate::error::Error::Other(format!("Unknown version: {version_key}")))?;
    let edition = products::get_edition(product, arch);

    eprintln!("\nFetching available languages...");
    let mut api = MicrosoftDownloadAPI::new()?;
    let languages = api.get_languages(edition.id)?;

    if languages.is_empty() {
        return Err(crate::error::Error::Other(format!(
            "No languages available for {arch}. Microsoft may have changed their API."
        )));
    }

    let lang_idx = interactive::pick_language(&languages)?;
    let language = &languages[lang_idx];

    // Detect USB drives
    eprintln!("\nLooking for USB drives...");
    let drives = usb::list_usb_drives().unwrap_or_default();

    let drive_idx = if drives.is_empty() {
        eprintln!("No USB drives found. Will download ISO only.");
        None
    } else {
        Some(interactive::pick_drive(&drives)?)
    };

    // Summary
    if let Some(idx) = drive_idx {
        let d = &drives[idx];
        let size_gb = d.size as f64 / 1e9;
        eprintln!("\n  Windows:  {} ({})", product.name, arch);
        eprintln!("  Language: {}", language.name);
        eprintln!("  Drive:    {} ({:.1} GB) — {}", d.name, size_gb, d.device);
        eprintln!("  Will download ISO, then write to USB.\n");
    } else {
        eprintln!("\n  Windows:  {} ({})", product.name, arch);
        eprintln!("  Language: {}", language.name);
        eprintln!("  Will download ISO to current directory.\n");
    }

    // Download
    eprintln!("Fetching download link...");
    let links = api.get_download_links(&language.sku_id, product.segment)?;
    if links.is_empty() {
        return Err(crate::error::Error::Other(
            "No download links returned by Microsoft. Try again later.".into(),
        ));
    }

    let link = &links[0];
    let filename = language
        .friendly_filename
        .as_deref()
        .unwrap_or(&link.filename);
    let output_path = std::env::current_dir()?.join(filename);

    if !download::check_existing(&output_path, link.sha1.as_deref()) {
        eprintln!("\nDownloading {filename}\n");
        download::download_file(&link.url, &output_path, link.size)?;
    }

    // Burn if drive selected
    if let Some(idx) = drive_idx {
        let drive = &drives[idx];
        if !burn::confirm_burn(drive, &output_path) {
            eprintln!("Burn cancelled. ISO saved to: {}", output_path.display());
            return Ok(());
        }
        burn::sudo_burn(&output_path, drive, PartitionScheme::Mbr, false)?;
    } else {
        eprintln!("\nISO saved to: {}", output_path.display());
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn burn_command(
    iso: Option<PathBuf>,
    version: Option<String>,
    lang: Option<String>,
    arch: &str,
    drive: Option<String>,
    gpt: bool,
    raw: bool,
    yes: bool,
) -> crate::error::Result<()> {
    let arch = normalize_arch(arch);
    let scheme = if gpt {
        PartitionScheme::Gpt
    } else {
        PartitionScheme::Mbr
    };

    // Resolve ISO source
    let iso_path = match iso {
        Some(path) => {
            if !path.exists() {
                return Err(crate::error::Error::Other(format!(
                    "ISO file not found: {}",
                    path.display()
                )));
            }
            path
        }
        None => {
            let version = version.ok_or_else(|| {
                crate::error::Error::Other(
                    "Either provide an ISO file or use --version to download one".into(),
                )
            })?;
            let version_key = format!("windows{version}");
            download_iso_for_burn(&version_key, &arch, lang.as_deref())?
        }
    };

    // List and select drive
    let drives = usb::list_usb_drives()?;
    if drives.is_empty() {
        return Err(crate::error::Error::Other(
            "No removable USB drives found. Insert a USB drive and try again.".into(),
        ));
    }

    let selected_drive = if let Some(dev) = drive {
        drives
            .iter()
            .find(|d| d.device == dev)
            .ok_or_else(|| {
                let available: Vec<&str> = drives.iter().map(|d| d.device.as_str()).collect();
                crate::error::Error::Other(format!(
                    "Drive {dev} not found. Available: {}",
                    available.join(", ")
                ))
            })?
    } else {
        let idx = interactive::pick_drive(&drives)?;
        &drives[idx]
    };

    // Confirm
    if !yes && !burn::confirm_burn(selected_drive, &iso_path) {
        eprintln!("Cancelled.");
        process::exit(0);
    }

    // Burn (escalates to sudo if needed)
    burn::sudo_burn(&iso_path, selected_drive, scheme, raw)?;

    Ok(())
}

fn burn_device_command(
    iso: PathBuf,
    device: String,
    device_name: String,
    device_size: u64,
    gpt: bool,
    raw: bool,
) -> crate::error::Result<()> {
    let drive = crate::models::UsbDrive {
        device,
        name: device_name,
        size: device_size,
        partitions: Vec::new(),
    };
    let scheme = if gpt {
        PartitionScheme::Gpt
    } else {
        PartitionScheme::Mbr
    };

    if raw {
        burn::write_raw_iso(&iso, &drive)
    } else {
        burn::create_bootable_usb(&iso, &drive, scheme)
    }
}

fn download_iso_for_burn(
    version_key: &str,
    arch: &str,
    lang_filter: Option<&str>,
) -> crate::error::Result<PathBuf> {
    let product = products::get_product(version_key)
        .ok_or_else(|| crate::error::Error::Other(format!("Unknown version: {version_key}")))?;
    let edition = products::get_edition(product, arch);

    let mut api = MicrosoftDownloadAPI::new()?;

    eprintln!("Fetching languages for {}...", edition.name);
    let languages = api.get_languages(edition.id)?;

    let language = interactive::resolve_language(&languages, lang_filter)
        .ok_or_else(|| crate::error::Error::Other("Language not found".into()))?
        .clone();

    eprintln!("Fetching download link for {}...", language.name);
    let links = api.get_download_links(&language.sku_id, product.segment)?;

    if links.is_empty() {
        return Err(crate::error::Error::Other(
            "No download links returned.".into(),
        ));
    }

    let link = &links[0];
    let filename = language
        .friendly_filename
        .as_deref()
        .unwrap_or(&link.filename);
    let output_path = std::env::current_dir()?.join(filename);

    if !download::check_existing(&output_path, link.sha1.as_deref()) {
        eprintln!("\nDownloading {filename}...\n");
        download::download_file(&link.url, &output_path, link.size)?;
    }

    Ok(output_path)
}

fn list_command(
    version: Option<String>,
    arch: &str,
    json: bool,
) -> crate::error::Result<()> {
    let arch = normalize_arch(arch);

    if version.is_none() {
        eprintln!("Available Windows Versions:\n");
        for product in products::PRODUCTS {
            let archs: Vec<&str> = product.editions.iter().map(|e| e.arch).collect();
            eprintln!(
                "  {} — {}",
                product.key.replace("windows", ""),
                archs.join(", ")
            );
        }
        eprintln!("\nUse: winiso list --version 11 to see available languages.");
        return Ok(());
    }

    let version_key = format!("windows{}", version.unwrap());

    if !json {
        eprintln!("Fetching catalog for {version_key} ({arch})...");
    }

    let entries = catalog::fetch_catalog(&version_key)?;
    let languages = catalog::get_languages_from_catalog(&entries, &arch);

    if json {
        let data: Vec<serde_json::Value> = languages
            .iter()
            .map(|l| {
                serde_json::json!({
                    "name": l.name,
                    "code": l.id,
                    "filename": l.friendly_filename,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&data).unwrap());
        return Ok(());
    }

    eprintln!("\nLanguages for {version_key} ({arch}):\n");
    for (i, lang) in languages.iter().enumerate() {
        let fname = lang.friendly_filename.as_deref().unwrap_or("");
        eprintln!(
            "  {:>3}) {} ({}){}", i + 1, lang.name, lang.id,
            if fname.is_empty() { String::new() } else { format!(" — {fname}") }
        );
    }

    Ok(())
}

fn download_command(
    version: Option<String>,
    lang: Option<String>,
    arch: &str,
    output: &Path,
    iso: bool,
) -> crate::error::Result<()> {
    let version = version.ok_or_else(|| {
        crate::error::Error::Other(
            "--version is required for non-interactive download.\n\
             Usage: winiso download --version 11 --lang en-us"
                .into(),
        )
    })?;

    let version_key = format!("windows{version}");
    let arch = normalize_arch(arch);

    std::fs::create_dir_all(output)?;

    if iso {
        download_iso(&version_key, &arch, lang.as_deref(), output)
    } else {
        download_esd(&version_key, &arch, lang.as_deref(), output)
    }
}

fn download_esd(
    version_key: &str,
    arch: &str,
    lang_filter: Option<&str>,
    output_dir: &Path,
) -> crate::error::Result<()> {
    eprintln!("Fetching catalog...");
    let entries = catalog::fetch_catalog(version_key)?;
    let languages = catalog::get_languages_from_catalog(&entries, arch);

    let language = interactive::resolve_language(&languages, lang_filter).ok_or_else(|| {
        let available: Vec<String> = languages
            .iter()
            .take(10)
            .map(|l| format!("{} ({})", l.name, l.id))
            .collect();
        crate::error::Error::Other(format!(
            "Language '{}' not found. Available: {}...",
            lang_filter.unwrap_or(""),
            available.join(", ")
        ))
    })?;

    let link = catalog::get_download_link_from_catalog(&entries, &language.id, arch)
        .ok_or_else(|| crate::error::Error::Other("No download link found.".into()))?;

    let output_path = output_dir.join(&link.filename);
    if download::check_existing(&output_path, link.sha1.as_deref()) {
        return Ok(());
    }

    eprintln!("\n{}", link.filename);
    if let Some(size) = link.size {
        eprintln!("Size: {:.1} GB", size as f64 / 1024.0 / 1024.0 / 1024.0);
    }

    download::download_file(&link.url, &output_path, link.size)?;
    Ok(())
}

fn download_iso(
    version_key: &str,
    arch: &str,
    lang_filter: Option<&str>,
    output_dir: &Path,
) -> crate::error::Result<()> {
    let product = products::get_product(version_key)
        .ok_or_else(|| crate::error::Error::Other(format!("Unknown version: {version_key}")))?;
    let edition = products::get_edition(product, arch);

    let mut api = MicrosoftDownloadAPI::new()?;

    eprintln!("Fetching languages for {}...", edition.name);
    let languages = api.get_languages(edition.id)?;

    let language = interactive::resolve_language(&languages, lang_filter)
        .ok_or_else(|| {
            let available: Vec<String> = languages
                .iter()
                .take(10)
                .map(|l| format!("{} ({})", l.name, l.id))
                .collect();
            crate::error::Error::Other(format!(
                "Language '{}' not found. Available: {}...",
                lang_filter.unwrap_or(""),
                available.join(", ")
            ))
        })?
        .clone();

    eprintln!("Fetching download link for {}...", language.name);
    let links = api.get_download_links(&language.sku_id, product.segment)?;

    if links.is_empty() {
        return Err(crate::error::Error::Other(
            "No download links returned.".into(),
        ));
    }

    let link = &links[0];
    let filename = language
        .friendly_filename
        .as_deref()
        .unwrap_or(&link.filename);
    let output_path = output_dir.join(filename);

    if download::check_existing(&output_path, link.sha1.as_deref()) {
        return Ok(());
    }

    eprintln!("\nDownloading {filename}...\n");
    download::download_file(&link.url, &output_path, link.size)?;
    Ok(())
}
