#[derive(Debug, Clone)]
pub struct Language {
    pub id: String,
    pub name: String,
    pub sku_id: String,
    pub friendly_filename: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DownloadLink {
    pub url: String,
    pub filename: String,
    pub size: Option<u64>,
    pub sha1: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UsbDrive {
    pub device: String,
    pub name: String,
    pub size: u64,
    pub partitions: Vec<String>,
}
