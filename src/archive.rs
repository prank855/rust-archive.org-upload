use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::RETRY_AFTER;
use serde::Deserialize;

use crate::sniff::ImageType;
use crate::util::{MAX_RESPONSE_BYTES, parse_web_timestamp, read_text, snippet, verify_image};

const SAVE_PREFIX: &str = "https://web.archive.org/save/";
const AVAILABILITY_URL: &str = "https://archive.org/wayback/available";

const SAVE_TIMEOUT: Duration = Duration::from_secs(25);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(750), Duration::from_millis(2000)];

#[derive(Debug)]
pub struct ArchiveError {
    pub message: String,
    pub rate_limited: bool,
}

impl ArchiveError {
    fn new(message: String) -> Self {
        Self {
            message,
            rate_limited: false,
        }
    }

    fn rate_limited(message: String) -> Self {
        Self {
            message,
            rate_limited: true,
        }
    }
}

#[derive(Deserialize)]
struct Availability {
    archived_snapshots: Snapshots,
}

#[derive(Deserialize)]
struct Snapshots {
    closest: Option<Snapshot>,
}

#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    available: bool,
    #[serde(default)]
    status: String,
    #[serde(default)]
    timestamp: String,
}

pub fn archive(
    client: &Client,
    save_client: &Client,
    page_url: &str,
    kind: Option<&ImageType>,
    deadline: Instant,
) -> Result<String, ArchiveError> {
    let mut last_error = String::from("no snapshot was created");

    for attempt in 0..3 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining < Duration::from_secs(3) {
            break;
        }

        if attempt > 0 {
            let pause = RETRY_DELAYS[attempt - 1].min(remaining);
            if pause.is_zero() {
                break;
            }
            std::thread::sleep(pause);
        }

        let timestamp = match submit(save_client, page_url, remaining.min(SAVE_TIMEOUT)) {
            Ok(Some(timestamp)) => timestamp,
            Ok(None) => match wait_for_snapshot(client, page_url, deadline) {
                Ok(timestamp) => timestamp,
                Err(error) => {
                    last_error = error;
                    continue;
                }
            },
            Err(error) if error.rate_limited => return Err(error),
            Err(error) => {
                last_error = error.message;
                continue;
            }
        };

        let replay = format!("https://web.archive.org/web/{timestamp}id_/{page_url}");
        let remaining = deadline.saturating_duration_since(Instant::now());

        match verify_image(client, &replay, kind, remaining.min(VERIFY_TIMEOUT), 3) {
            Ok(()) => return Ok(replay),
            Err(error) => {
                last_error = format!("archived copy could not be verified: {}", error.message());
            }
        }
    }

    Err(ArchiveError::new(format!(
        "could not archive {}: {}",
        page_url, last_error
    )))
}

fn submit(
    client: &Client,
    page_url: &str,
    timeout: Duration,
) -> Result<Option<String>, ArchiveError> {
    let response = client
        .get(format!("{}{}", SAVE_PREFIX, page_url))
        .timeout(timeout)
        .send()
        .map_err(|error| ArchiveError::new(format!("save request failed: {}", error)))?;

    let status = response.status();

    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(|value| format!(" (retry after {}s)", value))
            .unwrap_or_default();
        let body = read_text(response, MAX_RESPONSE_BYTES).unwrap_or_default();
        return Err(ArchiveError::rate_limited(format!(
            "Save Page Now rate limited{}: {}",
            retry_after,
            snippet(&body)
        )));
    }

    if status.is_redirection() {
        for header in ["location", "content-location"] {
            if let Some(timestamp) = response
                .headers()
                .get(header)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_web_timestamp)
            {
                return Ok(Some(timestamp));
            }
        }
        return Ok(None);
    }

    if status.is_success() {
        let body = read_text(response, MAX_RESPONSE_BYTES).unwrap_or_default();
        return Ok(timestamp_in_body(&body));
    }

    let body = read_text(response, MAX_RESPONSE_BYTES).unwrap_or_default();
    Err(ArchiveError::new(format!(
        "save returned HTTP {}: {}",
        status.as_u16(),
        snippet(&body)
    )))
}

fn timestamp_in_body(body: &str) -> Option<String> {
    body.match_indices("/web/")
        .filter_map(|(index, _)| parse_web_timestamp(&body[index..]))
        .next()
}

fn wait_for_snapshot(client: &Client, page_url: &str, deadline: Instant) -> Result<String, String> {
    let mut last_error = String::from("no snapshot appeared");

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining < Duration::from_secs(2) {
            return Err(last_error);
        }

        let timeout = remaining.min(Duration::from_secs(5));
        let response = client
            .get(AVAILABILITY_URL)
            .query(&[("url", page_url)])
            .timeout(timeout)
            .send();

        if let Ok(response) = response
            && response.status().is_success()
            && let Ok(body) = read_text(response, MAX_RESPONSE_BYTES)
            && let Ok(availability) = serde_json::from_str::<Availability>(&body)
            && let Some(snapshot) = availability.archived_snapshots.closest
        {
            if snapshot.available && snapshot.status == "200" {
                return Ok(snapshot.timestamp);
            }
            last_error = format!("snapshot status was {:?}", snapshot.status);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= Duration::from_secs(1) {
            return Err(last_error);
        }
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::USER_AGENT;
    use reqwest::redirect;

    #[test]
    fn extracts_timestamp_from_save_page() {
        let body = r#"<a href="/web/20261029040720/https://example.com/x">go</a>"#;
        assert_eq!(timestamp_in_body(body), Some("20261029040720".to_string()));
        assert_eq!(timestamp_in_body("<html>no capture</html>"), None);
    }

    #[test]
    #[ignore = "hits the Wayback Machine"]
    fn archives_a_public_image() {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        let save_client = Client::builder()
            .user_agent(USER_AGENT)
            .redirect(redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(90);
        let url = "https://upload.wikimedia.org/wikipedia/commons/4/47/PNG_transparency_demonstration_1.png";

        let archived = archive(&client, &save_client, url, None, deadline).unwrap();
        assert!(archived.starts_with("https://web.archive.org/web/"));
        assert!(archived.contains("id_/"));
    }
}
