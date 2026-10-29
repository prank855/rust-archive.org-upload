use std::time::Duration;

use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::blocking::multipart::{Form, Part};
use serde::Deserialize;

use crate::util::{MAX_RESPONSE_BYTES, read_text, snippet};

pub const USER_AGENT: &str = concat!(
    "discord-rpc-archive-uploader/",
    env!("CARGO_PKG_VERSION"),
    " (foobar2000 rich presence album art uploader)"
);

const LITTERBOX_URL: &str = "https://litterbox.catbox.moe/resources/internals/api.php";
const OXO_URL: &str = "https://0x0.st";
const UGUU_URL: &str = "https://uguu.se/upload?output=text";
const CATBOX_URL: &str = "https://catbox.moe/user/api.php";
const IMGUR_URL: &str = "https://api.imgur.com/3/image";
const TMPFILES_URL: &str = "https://tmpfiles.org/api/v1/upload";

const LITTERBOX_HOSTS: &[&str] = &["litter.catbox.moe"];
const OXO_HOSTS: &[&str] = &["0x0.st"];
const UGUU_HOSTS: &[&str] = &["uguu.se"];
const CATBOX_HOSTS: &[&str] = &["files.catbox.moe"];
const IMGUR_HOSTS: &[&str] = &["i.imgur.com"];
const TMPFILES_HOSTS: &[&str] = &["tmpfiles.org"];

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Input<'a> {
    pub data: &'a [u8],
    pub filename: &'a str,
    pub mime: &'a str,
}

pub struct Uploaded {
    pub url: String,
    pub provider: &'static str,
    pub cleanup: Option<Cleanup>,
}

pub struct Cleanup {
    url: String,
    token: String,
}

impl Cleanup {
    pub fn run(&self, client: &Client) -> Result<(), String> {
        let form = Form::new()
            .text("token", self.token.clone())
            .text("delete", "");

        let response = client
            .post(&self.url)
            .timeout(CLEANUP_TIMEOUT)
            .multipart(form)
            .send()
            .map_err(|error| error.to_string())?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "delete returned HTTP {}",
                response.status().as_u16()
            ))
        }
    }
}

#[derive(Clone, Copy)]
pub enum Provider {
    Litterbox,
    Oxo,
    Uguu,
    Catbox,
    Imgur,
    Tmpfiles,
}

pub const CHAIN: [Provider; 6] = [
    Provider::Imgur,
    Provider::Litterbox,
    Provider::Uguu,
    Provider::Oxo,
    Provider::Tmpfiles,
    Provider::Catbox,
];

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::Litterbox => "litterbox",
            Provider::Oxo => "0x0.st",
            Provider::Uguu => "uguu",
            Provider::Catbox => "catbox",
            Provider::Imgur => "imgur",
            Provider::Tmpfiles => "tmpfiles",
        }
    }

    pub fn available(self) -> bool {
        match self {
            Provider::Imgur => imgur_client_id().is_some(),
            _ => true,
        }
    }

    pub fn is_permanent(self) -> bool {
        matches!(self, Provider::Catbox)
    }

    pub fn is_discord_safe(self) -> bool {
        matches!(self, Provider::Imgur | Provider::Litterbox | Provider::Uguu)
    }

    pub fn upload(
        self,
        client: &Client,
        input: &Input<'_>,
        timeout: Duration,
    ) -> Result<Uploaded, String> {
        match self {
            Provider::Litterbox => litterbox(client, input, timeout),
            Provider::Oxo => oxo(client, input, timeout),
            Provider::Uguu => uguu(client, input, timeout),
            Provider::Catbox => catbox(client, input, timeout),
            Provider::Imgur => imgur(client, input, timeout),
            Provider::Tmpfiles => tmpfiles(client, input, timeout),
        }
    }
}

pub fn imgur_client_id() -> Option<&'static str> {
    option_env!("IMGUR_CLIENT_ID").filter(|client_id| !client_id.is_empty())
}

fn part(input: &Input<'_>) -> Result<Part, String> {
    Part::bytes(input.data.to_vec())
        .file_name(input.filename.to_string())
        .mime_str(input.mime)
        .map_err(|error| format!("could not build multipart body: {}", error))
}

fn host_allowed(host: &str, allowed: &[&str]) -> bool {
    let host = host.to_ascii_lowercase();

    allowed.iter().any(|candidate| {
        let candidate = candidate.to_ascii_lowercase();
        host == candidate || host.ends_with(&format!(".{}", candidate))
    })
}

fn validate_url(provider: &str, url: &str, hosts: &[&str]) -> Result<String, String> {
    let parsed = Url::parse(url)
        .map_err(|error| format!("{}: invalid URL {:?}: {}", provider, url, error))?;

    if parsed.scheme() != "https" {
        return Err(format!("{}: unsupported URL scheme: {}", provider, url));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!("{}: URL contains credentials: {}", provider, url));
    }
    if parsed.port().is_some() {
        return Err(format!(
            "{}: URL uses a non-default port: {}",
            provider, url
        ));
    }

    let host = parsed.host_str().unwrap_or_default();
    if !host_allowed(host, hosts) {
        return Err(format!(
            "{}: unexpected host {:?} (expected one of: {})",
            provider,
            host,
            hosts.join(", ")
        ));
    }

    Ok(url.to_string())
}

fn text_url(
    provider: &str,
    hosts: &[&str],
    response: reqwest::blocking::Response,
) -> Result<String, String> {
    let status = response.status();
    let body = read_text(response, MAX_RESPONSE_BYTES)
        .map_err(|error| format!("{}: could not read response: {}", provider, error))?;

    if !status.is_success() {
        return Err(format!(
            "{}: HTTP {}: {}",
            provider,
            status.as_u16(),
            snippet(&body)
        ));
    }

    validate_url(provider, body.trim(), hosts)
        .map_err(|error| format!("{}: unexpected response: {}", provider, error))
}

fn litterbox(client: &Client, input: &Input<'_>, timeout: Duration) -> Result<Uploaded, String> {
    let form = Form::new()
        .text("reqtype", "fileupload")
        .text("time", "72h")
        .part("fileToUpload", part(input)?);

    let response = client
        .post(LITTERBOX_URL)
        .timeout(timeout)
        .multipart(form)
        .send()
        .map_err(|error| format!("litterbox: {}", error))?;

    Ok(Uploaded {
        url: text_url("litterbox", LITTERBOX_HOSTS, response)?,
        provider: "litterbox",
        cleanup: None,
    })
}

fn oxo(client: &Client, input: &Input<'_>, timeout: Duration) -> Result<Uploaded, String> {
    let form = Form::new().text("expires", "1").part("file", part(input)?);

    let response = client
        .post(OXO_URL)
        .timeout(timeout)
        .multipart(form)
        .send()
        .map_err(|error| format!("0x0.st: {}", error))?;

    let token = response
        .headers()
        .get("x-token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let url = text_url("0x0.st", OXO_HOSTS, response)?;
    let cleanup = token.map(|token| Cleanup {
        url: url.clone(),
        token,
    });

    Ok(Uploaded {
        url,
        provider: "0x0.st",
        cleanup,
    })
}

fn uguu(client: &Client, input: &Input<'_>, timeout: Duration) -> Result<Uploaded, String> {
    let form = Form::new().part("files[]", part(input)?);

    let response = client
        .post(UGUU_URL)
        .timeout(timeout)
        .multipart(form)
        .send()
        .map_err(|error| format!("uguu: {}", error))?;

    Ok(Uploaded {
        url: text_url("uguu", UGUU_HOSTS, response)?,
        provider: "uguu",
        cleanup: None,
    })
}

fn catbox(client: &Client, input: &Input<'_>, timeout: Duration) -> Result<Uploaded, String> {
    let form = Form::new()
        .text("reqtype", "fileupload")
        .part("fileToUpload", part(input)?);

    let response = client
        .post(CATBOX_URL)
        .timeout(timeout)
        .multipart(form)
        .send()
        .map_err(|error| format!("catbox: {}", error))?;

    Ok(Uploaded {
        url: text_url("catbox", CATBOX_HOSTS, response)?,
        provider: "catbox",
        cleanup: None,
    })
}

#[derive(Deserialize)]
struct ImgurResponse {
    data: ImgurData,
}

#[derive(Deserialize)]
struct ImgurData {
    link: String,
}

fn imgur(client: &Client, input: &Input<'_>, timeout: Duration) -> Result<Uploaded, String> {
    let client_id = imgur_client_id()
        .ok_or_else(|| "imgur: IMGUR_CLIENT_ID was not set at build time".to_string())?;

    let response = client
        .post(IMGUR_URL)
        .header("Authorization", format!("Client-ID {}", client_id))
        .timeout(timeout)
        .body(input.data.to_vec())
        .send()
        .map_err(|error| format!("imgur: {}", error))?;

    let status = response.status();
    let body = read_text(response, MAX_RESPONSE_BYTES)
        .map_err(|error| format!("imgur: could not read response: {}", error))?;

    if !status.is_success() {
        return Err(format!(
            "imgur: HTTP {}: {}",
            status.as_u16(),
            snippet(&body)
        ));
    }

    let parsed: ImgurResponse = serde_json::from_str(&body)
        .map_err(|error| format!("imgur: bad JSON ({}): {}", error, snippet(&body)))?;
    let link = validate_url("imgur", &parsed.data.link, IMGUR_HOSTS)?;

    Ok(Uploaded {
        url: link,
        provider: "imgur",
        cleanup: None,
    })
}

#[derive(Deserialize)]
struct TmpfilesResponse {
    data: TmpfilesData,
}

#[derive(Deserialize)]
struct TmpfilesData {
    url: String,
}

fn tmpfiles(client: &Client, input: &Input<'_>, timeout: Duration) -> Result<Uploaded, String> {
    let form = Form::new().part("file", part(input)?);

    let response = client
        .post(TMPFILES_URL)
        .timeout(timeout)
        .multipart(form)
        .send()
        .map_err(|error| format!("tmpfiles: {}", error))?;

    let status = response.status();
    let body = read_text(response, MAX_RESPONSE_BYTES)
        .map_err(|error| format!("tmpfiles: could not read response: {}", error))?;

    if !status.is_success() {
        return Err(format!(
            "tmpfiles: HTTP {}: {}",
            status.as_u16(),
            snippet(&body)
        ));
    }

    let parsed: TmpfilesResponse = serde_json::from_str(&body)
        .map_err(|error| format!("tmpfiles: bad JSON ({}): {}", error, snippet(&body)))?;
    let url = validate_url(
        "tmpfiles",
        &tmpfiles_direct(&parsed.data.url),
        TMPFILES_HOSTS,
    )?;

    Ok(Uploaded {
        url,
        provider: "tmpfiles",
        cleanup: None,
    })
}

pub fn tmpfiles_direct(url: &str) -> String {
    for prefix in ["https://tmpfiles.org/", "http://tmpfiles.org/"] {
        if let Some(rest) = url.strip_prefix(prefix)
            && !rest.starts_with("dl/")
        {
            return format!("https://tmpfiles.org/dl/{}", rest);
        }
    }

    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmpfiles_urls_point_at_the_file() {
        assert_eq!(
            tmpfiles_direct("https://tmpfiles.org/12345/cover.jpg"),
            "https://tmpfiles.org/dl/12345/cover.jpg"
        );
        assert_eq!(
            tmpfiles_direct("https://tmpfiles.org/dl/12345/cover.jpg"),
            "https://tmpfiles.org/dl/12345/cover.jpg"
        );
        assert_eq!(
            tmpfiles_direct("https://example.com/cover.jpg"),
            "https://example.com/cover.jpg"
        );
        assert_eq!(
            tmpfiles_direct("http://tmpfiles.org/12345/cover.jpg"),
            "https://tmpfiles.org/dl/12345/cover.jpg"
        );
    }

    #[test]
    fn only_accepts_expected_https_hosts() {
        assert!(
            validate_url(
                "litterbox",
                "https://litter.catbox.moe/ab.png",
                LITTERBOX_HOSTS
            )
            .is_ok()
        );
        for url in [
            "http://litter.catbox.moe/ab.png",
            "https://litter.catbox.moe:8443/ab.png",
            "https://user:pass@litter.catbox.moe/ab.png",
            "https://127.0.0.1/ab.png",
            "https://litter.catbox.moe.evil.example/ab.png",
            "https://example.com/ab.png",
        ] {
            assert!(
                validate_url("litterbox", url, LITTERBOX_HOSTS).is_err(),
                "accepted {}",
                url
            );
        }

        assert!(validate_url("uguu", "https://d.uguu.se/ab.png", UGUU_HOSTS).is_ok());
        assert!(validate_url("uguu", "https://h.uguu.se/ab.png", UGUU_HOSTS).is_ok());
        assert!(validate_url("uguu", "https://uguu.se.evil.example/a.png", UGUU_HOSTS).is_err());
    }

    #[test]
    fn discord_safe_providers() {
        assert!(Provider::Imgur.is_discord_safe());
        assert!(Provider::Litterbox.is_discord_safe());
        assert!(Provider::Uguu.is_discord_safe());
        assert!(!Provider::Oxo.is_discord_safe());
        assert!(!Provider::Tmpfiles.is_discord_safe());
        assert!(!Provider::Catbox.is_discord_safe());
    }

    #[test]
    fn chain_is_ordered_and_has_unique_names() {
        let names: Vec<&str> = CHAIN.iter().map(|provider| provider.name()).collect();
        assert_eq!(
            names,
            ["imgur", "litterbox", "uguu", "0x0.st", "tmpfiles", "catbox"]
        );

        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
    }
}
