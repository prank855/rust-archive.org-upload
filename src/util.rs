use std::io::Read;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, RANGE};

use crate::sniff::{self, ImageType, WINDOW};

const VERIFY_RETRY_DELAY: Duration = Duration::from_millis(900);

pub const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
pub enum VerifyError {
    Unreachable(String),
    Mismatch(String),
}

impl VerifyError {
    pub fn message(&self) -> &str {
        match self {
            VerifyError::Unreachable(message) | VerifyError::Mismatch(message) => message,
        }
    }
}

pub fn read_text(response: reqwest::blocking::Response, limit: u64) -> Result<String, String> {
    let mut buffer = Vec::new();

    response
        .take(limit + 1)
        .read_to_end(&mut buffer)
        .map_err(|error| error.to_string())?;

    if buffer.len() as u64 > limit {
        return Err(format!("response body larger than {} KiB", limit / 1024));
    }

    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

pub fn snippet(body: &str) -> String {
    const LIMIT: usize = 160;

    let one_line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = one_line.chars().take(LIMIT).collect();
    if one_line.chars().count() > LIMIT {
        out.push_str("...");
    }
    out
}

pub fn parse_web_timestamp(location: &str) -> Option<String> {
    let rest = location.split("/web/").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();

    if digits.len() >= 14 {
        Some(digits[..14].to_string())
    } else {
        None
    }
}

pub fn fetch_prefix(
    client: &Client,
    url: &str,
    timeout: Duration,
) -> Result<(Vec<u8>, Option<String>), VerifyError> {
    let response = client
        .get(url)
        .header(RANGE, format!("bytes=0-{}", WINDOW - 1))
        .timeout(timeout)
        .send()
        .map_err(|error| VerifyError::Unreachable(error.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(VerifyError::Mismatch(format!("HTTP {}", status.as_u16())));
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut prefix = Vec::with_capacity(WINDOW);
    response
        .take(WINDOW as u64)
        .read_to_end(&mut prefix)
        .map_err(|error| VerifyError::Unreachable(error.to_string()))?;

    Ok((prefix, content_type))
}

pub fn verify_image(
    client: &Client,
    url: &str,
    kind: Option<&ImageType>,
    timeout: Duration,
    attempts: u32,
) -> Result<(), VerifyError> {
    let deadline = Instant::now() + timeout;
    let mut mismatch = None;
    let mut unreachable = String::from("no response");

    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining < VERIFY_RETRY_DELAY {
                break;
            }
            std::thread::sleep(VERIFY_RETRY_DELAY);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match check_image(client, url, kind, remaining.min(Duration::from_secs(6))) {
            Ok(()) => return Ok(()),
            Err(VerifyError::Mismatch(message)) => {
                mismatch = Some(message);
                break;
            }
            Err(VerifyError::Unreachable(message)) => unreachable = message,
        }
    }

    Err(match mismatch {
        Some(message) => VerifyError::Mismatch(message),
        None => VerifyError::Unreachable(unreachable),
    })
}

fn check_image(
    client: &Client,
    url: &str,
    kind: Option<&ImageType>,
    timeout: Duration,
) -> Result<(), VerifyError> {
    let (prefix, content_type) = fetch_prefix(client, url, timeout)?;
    let content_type = content_type.as_deref().unwrap_or("unknown");

    match kind {
        Some(kind) if kind.matches(&prefix) => Ok(()),
        Some(kind) => Err(VerifyError::Mismatch(format!(
            "served bytes are not {} (content-type: {})",
            kind.mime, content_type
        ))),
        None if !sniff::looks_like_html(&prefix) => Ok(()),
        None => Err(VerifyError::Mismatch(format!(
            "served an HTML page instead of the file (content-type: {})",
            content_type
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_wayback_timestamps() {
        assert_eq!(
            parse_web_timestamp(
                "https://web.archive.org/web/20261029025557/https://i.imgur.com/a.jpg"
            ),
            Some("20261029025557".to_string())
        );
        assert_eq!(
            parse_web_timestamp("/web/20261029025557id_/https://example.com/a.png"),
            Some("20261029025557".to_string())
        );
        assert_eq!(parse_web_timestamp("/web/2026/https://example.com/"), None);
        assert_eq!(parse_web_timestamp("https://example.com/"), None);
    }

    #[test]
    fn snippets_are_single_line_and_bounded() {
        assert_eq!(snippet("a\n b\tc"), "a b c");
        let long = "x".repeat(500);
        let short = snippet(&long);
        assert!(short.len() < long.len());
        assert!(short.ends_with("..."));
    }
}
