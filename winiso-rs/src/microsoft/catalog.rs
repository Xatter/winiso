use std::io::{Cursor, Read};

use crate::error::{Error, Result};
use crate::models::{DownloadLink, Language};

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:100.0) Gecko/20100101 Firefox/100.0";

const CATALOG_URLS: &[(&str, &str)] = &[
    ("windows11", "https://go.microsoft.com/fwlink/?LinkId=2156292"),
    ("windows10", "https://go.microsoft.com/fwlink/?LinkId=841361"),
];

#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub filename: String,
    pub language_code: String,
    pub language: String,
    pub architecture: String,
    pub size: u64,
    pub sha1: String,
    pub file_path: String,
}

pub fn fetch_catalog(version: &str) -> Result<Vec<CatalogEntry>> {
    let url = CATALOG_URLS
        .iter()
        .find(|(k, _)| *k == version)
        .map(|(_, v)| *v)
        .ok_or_else(|| Error::Other(format!("Unknown version: {version}")))?;

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let cab_data = client.get(url).send()?.error_for_status()?.bytes()?;

    let xml_content = extract_xml_from_cab(&cab_data)?;
    parse_products_xml(&xml_content)
}

fn extract_xml_from_cab(cab_data: &[u8]) -> Result<String> {
    let cursor = Cursor::new(cab_data);
    let mut cabinet = cab::Cabinet::new(cursor)
        .map_err(|e| Error::Cab(format!("Failed to open CAB: {e}")))?;

    let mut file_names = Vec::new();
    for folder in cabinet.folder_entries() {
        for file in folder.file_entries() {
            file_names.push(file.name().to_string());
        }
    }

    let xml_name = file_names
        .iter()
        .find(|name: &&String| name.to_lowercase().contains("products") && name.to_lowercase().ends_with(".xml"))
        .or_else(|| file_names.iter().find(|name: &&String| name.to_lowercase().ends_with(".xml")))
        .ok_or_else(|| Error::Cab("No XML file found in CAB archive".into()))?
        .clone();

    let mut reader = cabinet.read_file(&xml_name)
        .map_err(|e| Error::Cab(format!("Failed to read {xml_name}: {e}")))?;

    let mut xml_content = String::new();
    reader.read_to_string(&mut xml_content)
        .map_err(|e| Error::Cab(format!("Failed to decode XML: {e}")))?;

    Ok(xml_content)
}

fn parse_products_xml(xml_content: &str) -> Result<Vec<CatalogEntry>> {
    let doc = roxmltree::Document::parse(xml_content)
        .map_err(|e| Error::Xml(format!("Failed to parse products XML: {e}")))?;

    let mut entries = Vec::new();

    for node in doc.descendants() {
        if node.has_tag_name("File") {
            let filename = child_text(&node, "FileName");
            if filename.is_empty() {
                continue;
            }
            entries.push(CatalogEntry {
                filename,
                language_code: child_text(&node, "LanguageCode"),
                language: child_text(&node, "Language"),
                architecture: child_text(&node, "Architecture"),
                size: child_text(&node, "Size").parse().unwrap_or(0),
                sha1: child_text(&node, "Sha1"),
                file_path: child_text(&node, "FilePath"),
            });
        }
    }

    Ok(entries)
}

fn child_text(node: &roxmltree::Node, tag: &str) -> String {
    node.children()
        .find(|c| c.has_tag_name(tag))
        .and_then(|c| c.text())
        .unwrap_or("")
        .trim()
        .to_string()
}

pub fn get_languages_from_catalog(entries: &[CatalogEntry], arch: &str) -> Vec<Language> {
    let mut seen = std::collections::BTreeMap::new();

    for entry in entries {
        if !entry.architecture.eq_ignore_ascii_case(arch) {
            continue;
        }
        seen.entry(entry.language_code.clone()).or_insert_with(|| {
            Language {
                id: entry.language_code.clone(),
                name: entry.language.clone(),
                sku_id: entry.language_code.clone(),
                friendly_filename: Some(entry.filename.clone()),
            }
        });
    }

    let mut languages: Vec<Language> = seen.into_values().collect();
    languages.sort_by(|a, b| a.name.cmp(&b.name));
    languages
}

pub fn get_download_link_from_catalog(
    entries: &[CatalogEntry],
    language_code: &str,
    arch: &str,
) -> Option<DownloadLink> {
    entries.iter().find(|e| {
        e.language_code == language_code && e.architecture.eq_ignore_ascii_case(arch)
    }).map(|entry| DownloadLink {
        url: entry.file_path.clone(),
        filename: entry.filename.clone(),
        size: Some(entry.size),
        sha1: Some(entry.sha1.clone()),
    })
}
