use reqwest::blocking::Client;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::models::{DownloadLink, Language};

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:100.0) Gecko/20100101 Firefox/100.0";
const PROFILE: &str = "606624d44113";
const BASE_URL: &str = "https://www.microsoft.com/software-download-connector/api";
const INSTANCE_ID: &str = "560dc9f3-1aa5-4a2f-b63c-9e18f8d0e175";

pub struct MicrosoftDownloadAPI {
    session_id: String,
    client: Client,
    verified: bool,
}

impl MicrosoftDownloadAPI {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(10))
            .cookie_store(true)
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            client,
            verified: false,
        })
    }

    fn ensure_verified(&mut self) -> Result<()> {
        if self.verified {
            return Ok(());
        }

        self.client
            .get(format!(
                "https://vlscppe.microsoft.com/tags?org_id=y6jn8c31&session_id={}",
                self.session_id
            ))
            .send()?
            .error_for_status()?;

        let resp = self.client
            .get("https://ov-df.microsoft.com/mdt.js")
            .query(&[
                ("instanceId", INSTANCE_ID),
                ("PageId", "si"),
                ("session_id", self.session_id.as_str()),
            ])
            .send()?
            .error_for_status()?;

        let mdt_js = resp.text()?;

        let parse_err = || Error::Api {
            message: "Failed to parse anti-bot verification response".into(),
            details: format!("Could not parse mdt.js (length={})", mdt_js.len()),
        };

        // Extract URL: find url:"https://ov-df.microsoft.com/..." and grab until closing "
        let url_prefix = "url:\"https://ov-df.microsoft.com/";
        let url_start = mdt_js.find(url_prefix).ok_or_else(parse_err)? + 5; // skip url:"
        let url_end = mdt_js[url_start..].find('"').ok_or_else(parse_err)? + url_start;
        let base_url = &mdt_js[url_start..url_end];

        // Extract rticks: find rticks="+DIGITS
        let rticks_prefix = "rticks=\"+";
        let rticks_start = mdt_js.find(rticks_prefix).ok_or_else(parse_err)? + rticks_prefix.len();
        let rticks_end = mdt_js[rticks_start..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| i + rticks_start)
            .unwrap_or(mdt_js.len());
        let rticks = &mdt_js[rticks_start..rticks_end];

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();

        let reply_url = format!("{base_url}&mdt={now_ms}&rticks={rticks}");

        self.client.get(&reply_url).send()?.error_for_status()?;
        self.verified = true;
        Ok(())
    }

    pub fn get_languages(&mut self, product_edition_id: &str) -> Result<Vec<Language>> {
        self.ensure_verified()?;

        let resp = self.client
            .get(format!("{BASE_URL}/getskuinformationbyproductedition"))
            .query(&[
                ("profile", PROFILE),
                ("productEditionId", product_edition_id),
                ("SKU", "undefined"),
                ("friendlyFileName", "undefined"),
                ("Locale", "en-US"),
                ("sessionID", self.session_id.as_str()),
            ])
            .send()?
            .error_for_status()?;

        let data: serde_json::Value = resp.json()?;

        if let Some(errors) = data.get("Errors").filter(|e| {
            !e.is_null() && e.as_array().is_some_and(|a| !a.is_empty())
        }) {
            return Err(Error::Api {
                message: "API returned errors".into(),
                details: errors.to_string(),
            });
        }

        let languages = data["Skus"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|sku| {
                let filenames = sku["FriendlyFileNames"].as_array();
                Language {
                    id: sku["Language"].as_str().unwrap_or("").to_string(),
                    name: sku["LocalizedLanguage"]
                        .as_str()
                        .or_else(|| sku["Language"].as_str())
                        .unwrap_or("")
                        .to_string(),
                    sku_id: sku["Id"].to_string().trim_matches('"').to_string(),
                    friendly_filename: filenames
                        .and_then(|f: &Vec<serde_json::Value>| f.first())
                        .and_then(|v: &serde_json::Value| v.as_str())
                        .map(String::from),
                }
            })
            .collect();

        Ok(languages)
    }

    pub fn get_download_links(
        &mut self,
        sku_id: &str,
        product_segment: &str,
    ) -> Result<Vec<DownloadLink>> {
        self.ensure_verified()?;

        let referer = format!("https://www.microsoft.com/software-download/{product_segment}");

        let resp = self.client
            .get(format!("{BASE_URL}/GetProductDownloadLinksBySku"))
            .query(&[
                ("profile", PROFILE),
                ("productEditionId", "undefined"),
                ("SKU", sku_id),
                ("friendlyFileName", "undefined"),
                ("Locale", "en-US"),
                ("sessionID", self.session_id.as_str()),
            ])
            .header("Referer", &referer)
            .send()?
            .error_for_status()?;

        let data: serde_json::Value = resp.json()?;

        if let Some(errors) = data.get("Errors").filter(|e| {
            !e.is_null() && e.as_array().is_some_and(|a| !a.is_empty())
        }) {
            return Err(Error::Api {
                message: "API returned errors".into(),
                details: errors.to_string(),
            });
        }

        let links = data["ProductDownloadOptions"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|option| {
                let uri = option["Uri"].as_str().unwrap_or("");
                if uri.is_empty() {
                    return None;
                }
                let filename = uri
                    .rsplit('/')
                    .next()
                    .unwrap_or("windows.iso")
                    .split('?')
                    .next()
                    .unwrap_or("windows.iso")
                    .to_string();
                Some(DownloadLink {
                    url: uri.to_string(),
                    filename,
                    size: None,
                    sha1: None,
                })
            })
            .collect();

        Ok(links)
    }
}
