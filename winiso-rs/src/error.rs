use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("API error: {message}")]
    Api { message: String, details: String },

    #[error("{}", friendly_http_error(.0))]
    Http(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("XML parse error: {0}")]
    Xml(String),

    #[error("CAB extraction error: {0}")]
    Cab(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn friendly_http_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "Network timeout — check your internet connection or try again later".into()
    } else if e.is_connect() {
        "Could not connect to Microsoft servers — check your internet connection".into()
    } else if let Some(status) = e.status() {
        format!("HTTP {status} from Microsoft servers — try again later")
    } else {
        format!("Network error: {e}")
    }
}
