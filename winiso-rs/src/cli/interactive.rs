use dialoguer::Select;

use crate::error::Result;
use crate::models::{Language, UsbDrive};

pub fn pick_version() -> &'static str {
    let options = &["Windows 11", "Windows 10"];
    let selection = Select::new()
        .with_prompt("Select Windows version")
        .items(options)
        .default(0)
        .interact()
        .unwrap_or(0);

    match selection {
        0 => "windows11",
        _ => "windows10",
    }
}

pub fn pick_arch() -> &'static str {
    let options = &["x64", "ARM64"];
    let selection = Select::new()
        .with_prompt("Select architecture")
        .items(options)
        .default(0)
        .interact()
        .unwrap_or(0);

    options[selection]
}

pub fn pick_language(languages: &[Language]) -> Result<usize> {
    let mut sorted: Vec<(usize, &Language)> = languages.iter().enumerate().collect();
    sorted.sort_by(|(_, a), (_, b)| {
        let a_rank = if a.id == "en-us" {
            0
        } else if a.id.starts_with("en-") {
            1
        } else {
            2
        };
        let b_rank = if b.id == "en-us" {
            0
        } else if b.id.starts_with("en-") {
            1
        } else {
            2
        };
        a_rank.cmp(&b_rank).then_with(|| a.name.cmp(&b.name))
    });

    let display: Vec<String> = sorted.iter().map(|(_, l)| l.name.clone()).collect();

    let selection = Select::new()
        .with_prompt(format!("Select language ({} available)", display.len()))
        .items(&display)
        .default(0)
        .interact()
        .unwrap_or(0);

    Ok(sorted[selection].0)
}

pub fn pick_drive(drives: &[UsbDrive]) -> Result<usize> {
    let display: Vec<String> = drives
        .iter()
        .map(|d| {
            let size_gb = d.size as f64 / 1e9;
            format!("{} ({:.1} GB) — {}", d.name, size_gb, d.device)
        })
        .collect();

    let selection = Select::new()
        .with_prompt("Select USB drive")
        .items(&display)
        .default(0)
        .interact()
        .unwrap_or(0);

    Ok(selection)
}

pub fn resolve_language<'a>(
    languages: &'a [Language],
    filter: Option<&str>,
) -> Option<&'a Language> {
    if let Some(f) = filter {
        let lf = f.to_lowercase();
        return languages
            .iter()
            .find(|l| l.name.to_lowercase().contains(&lf) || l.id.to_lowercase() == lf);
    }

    languages
        .iter()
        .find(|l| l.id == "en-us" || l.name.to_lowercase().contains("english"))
        .or(languages.first())
}
