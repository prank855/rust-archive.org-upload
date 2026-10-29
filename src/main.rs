mod archive;
mod providers;
mod sniff;
mod util;

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::redirect;

use providers::Uploaded;

const MAX_UPLOAD_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DEADLINE_SECS: u64 = 3600;
const DEFAULT_DEADLINE_SECS: u64 = 60;
const DEFAULT_ARCHIVE_BUDGET_SECS: u64 = 8;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(10);
const URL_CHECK_TIMEOUT: Duration = Duration::from_secs(8);

struct Options {
    verbose: bool,
    deadline: Duration,
    archive_budget: Duration,
    skip_archive: bool,
    skip_permanent: bool,
    path: Option<String>,
    cached_url: Option<String>,
    help: bool,
}

fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(error) => exit_with_message(format!("error: {}\n", error), 1),
    };

    if options.help {
        exit_with_message(help_text(), 0);
    }

    let started = Instant::now();
    let deadline = match started.checked_add(options.deadline) {
        Some(deadline) => deadline,
        None => exit_with_message("error: deadline is too large\n".to_string(), 1),
    };
    let archive_deadline = if options.archive_budget.is_zero() {
        deadline
    } else {
        match started.checked_add(options.archive_budget) {
            Some(budget_deadline) => budget_deadline.min(deadline),
            None => deadline,
        }
    };

    let filepath = match options.path.clone() {
        Some(path) => Some(path),
        None => read_stdin_path(),
    };

    let client = match build_client(false) {
        Ok(client) => client,
        Err(error) => exit_with_message(
            format!("error: could not build HTTP client: {}\n", error),
            1,
        ),
    };
    let strict_client = match build_client(true) {
        Ok(client) => client,
        Err(error) => exit_with_message(
            format!("error: could not build HTTP client: {}\n", error),
            1,
        ),
    };

    let mut file_data: Option<Vec<u8>> = None;
    let mut file_error: Option<String> = None;

    if let Some(path) = &filepath {
        match read_file(path) {
            Ok(data) if data.is_empty() => file_error = Some(format!("file is empty: {}", path)),
            Ok(data) => file_data = Some(data),
            Err(error) => file_error = Some(error),
        }
    }

    let kind = file_data.as_deref().and_then(sniff::detect);

    if let Some(url) = &options.cached_url {
        if should_reuse_url(&strict_client, url, kind, Duration::from_secs(3)) {
            exit_with_message(url.clone(), 0);
        }

        if file_data.is_none() {
            exit_with_message(
                format!(
                    "error: cached URL is not usable and no file could be read{}\n",
                    file_error
                        .as_deref()
                        .map(|error| format!(": {}", error))
                        .unwrap_or_default()
                ),
                1,
            );
        }
    }

    let data = match file_data {
        Some(data) => data,
        None => exit_with_message(
            format!(
                "error: {}\n",
                file_error
                    .as_deref()
                    .unwrap_or("no file path or URL was provided")
            ),
            1,
        ),
    };

    let filepath = filepath.unwrap_or_default();
    let (mime, extension) = match kind {
        Some(kind) => (kind.mime, kind.ext),
        None => sniff::from_extension(Path::new(&filepath)),
    };
    let filename = format!("image.{}", extension);

    let input = providers::Input {
        data: &data,
        filename: &filename,
        mime,
    };

    if options.verbose {
        eprintln!(
            "{} x {} bytes, {}, budget {}s",
            filename,
            data.len(),
            mime,
            options.deadline.as_secs()
        );
    }

    let mut failures: Vec<String> = Vec::new();

    for provider in providers::CHAIN {
        if !provider.available() {
            if options.verbose {
                eprintln!(
                    "skip {}: IMGUR_CLIENT_ID not set at build time",
                    provider.name()
                );
            }
            continue;
        }

        if provider.is_permanent() && options.skip_permanent {
            if options.verbose {
                eprintln!("skip {}: --no-permanent was given", provider.name());
            }
            continue;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining < Duration::from_secs(2) {
            failures.push(format!("{}: time budget exhausted", provider.name()));
            break;
        }

        if options.verbose {
            eprintln!("uploading with {}", provider.name());
        }

        let uploaded = match provider.upload(&client, &input, remaining.min(UPLOAD_TIMEOUT)) {
            Ok(uploaded) => uploaded,
            Err(error) => {
                if options.verbose {
                    eprintln!("{} failed: {}", provider.name(), error);
                }
                failures.push(error);
                continue;
            }
        };

        if options.verbose {
            eprintln!("{} returned {}", uploaded.provider, uploaded.url);
        }

        if options.skip_archive {
            let check_timeout = deadline
                .saturating_duration_since(Instant::now())
                .min(URL_CHECK_TIMEOUT);

            if let Err(error) =
                util::verify_image(&strict_client, &uploaded.url, kind, check_timeout, 1)
                && options.verbose
            {
                eprintln!(
                    "warning: {} could not be verified: {}",
                    uploaded.provider,
                    error.message()
                );
            }

            exit_with_message(uploaded.url, 0);
        }

        let archived = if Instant::now() >= archive_deadline {
            Err(archive::ArchiveError {
                message: "archive time budget exhausted".to_string(),
                rate_limited: false,
            })
        } else {
            archive::archive(
                &client,
                &strict_client,
                &uploaded.url,
                kind,
                archive_deadline,
            )
        };

        match archived {
            Ok(link) => {
                if options.verbose {
                    eprintln!("archived {}", link);
                }
                cleanup(&client, &uploaded, options.verbose);
                exit_with_message(link, 0);
            }
            Err(error) => {
                if options.verbose {
                    eprintln!("warning: {}; returning the direct URL", error.message);
                }
                exit_with_message(uploaded.url, 0);
            }
        }
    }

    exit_with_message(
        format!("error: all options failed\n{}\n", failures.join("\n")),
        1,
    );
}

fn cleanup(client: &Client, uploaded: &Uploaded, verbose: bool) {
    if let Some(cleanup) = &uploaded.cleanup
        && let Err(error) = cleanup.run(client)
        && verbose
    {
        eprintln!("could not remove the intermediate upload: {}", error);
    }
}

fn build_client(no_redirects: bool) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder()
        .user_agent(providers::USER_AGENT)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30));

    if no_redirects {
        builder = builder.redirect(redirect::Policy::none());
    }

    builder.build()
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        verbose: false,
        deadline: Duration::from_secs(DEFAULT_DEADLINE_SECS),
        archive_budget: Duration::from_secs(DEFAULT_ARCHIVE_BUDGET_SECS),
        skip_archive: false,
        skip_permanent: skip_permanent_from_env(),
        path: None,
        cached_url: None,
        help: false,
    };

    let mut positional: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => options.help = true,
            "-v" | "--verbose" => options.verbose = true,
            "--skip-archive" => options.skip_archive = true,
            "--no-permanent" => options.skip_permanent = true,
            "--deadline" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--deadline needs a value in seconds".to_string())?;
                options.deadline = parse_deadline(&value)?;
            }
            "--archive-budget" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--archive-budget needs a value in seconds".to_string())?;
                options.archive_budget = parse_archive_budget(&value)?;
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--deadline=") {
                    options.deadline = parse_deadline(value)?;
                } else if let Some(value) = arg.strip_prefix("--archive-budget=") {
                    options.archive_budget = parse_archive_budget(value)?;
                } else if arg.starts_with('-') && arg != "-" {
                    return Err(format!("unknown argument: {}", arg));
                } else {
                    positional.push(arg);
                }
            }
        }
    }

    let (path, cached_url) = classify_positionals(&positional)?;
    options.path = path;
    options.cached_url = cached_url;

    Ok(options)
}

fn classify_positionals(args: &[String]) -> Result<(Option<String>, Option<String>), String> {
    let mut path = None;
    let mut url = None;

    for arg in args {
        if arg.starts_with("http://") || arg.starts_with("https://") {
            if url.is_some() {
                return Err(format!("unexpected extra URL argument: {}", arg));
            }
            url = Some(arg.clone());
        } else if arg.chars().all(|character| character.is_ascii_digit()) {
            // {blur_percent} from foo_discord_rich; nothing to do with it.
        } else if path.is_none() {
            path = Some(arg.clone());
        } else {
            return Err(format!("unexpected extra argument: {}", arg));
        }
    }

    Ok((path, url))
}

fn parse_deadline(value: &str) -> Result<Duration, String> {
    let seconds: u64 = value
        .parse()
        .map_err(|_| format!("invalid deadline: {}", value))?;

    if seconds == 0 || seconds > MAX_DEADLINE_SECS {
        return Err(format!(
            "deadline must be between 1 and {} seconds",
            MAX_DEADLINE_SECS
        ));
    }

    Ok(Duration::from_secs(seconds))
}

fn parse_archive_budget(value: &str) -> Result<Duration, String> {
    let seconds: u64 = value
        .parse()
        .map_err(|_| format!("invalid archive budget: {}", value))?;

    if seconds > MAX_DEADLINE_SECS {
        return Err(format!(
            "archive budget must be between 0 and {} seconds",
            MAX_DEADLINE_SECS
        ));
    }

    Ok(Duration::from_secs(seconds))
}

fn skip_permanent_from_env() -> bool {
    std::env::var("ARCHIVE_UPLOADER_NO_PERMANENT")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
}

fn read_stdin_path() -> Option<String> {
    let mut buffer = String::new();

    match io::stdin().read_line(&mut buffer) {
        Ok(0) => None,
        Ok(_) => {
            let path = buffer.trim().trim_matches('"').trim().to_string();
            if path.is_empty() { None } else { Some(path) }
        }
        Err(_) => None,
    }
}

fn read_file(path: &str) -> Result<Vec<u8>, String> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Err(format!("not a file: {}", path)),
        Ok(metadata) if metadata.len() > MAX_UPLOAD_BYTES => {
            return Err(format!(
                "file is larger than {} MiB: {}",
                MAX_UPLOAD_BYTES / (1024 * 1024),
                path
            ));
        }
        Ok(_) => {}
        Err(error) => return Err(format!("could not read file: {}", error)),
    }

    std::fs::read(path).map_err(|error| format!("could not read file: {}", error))
}

fn should_reuse_url(
    client: &Client,
    url: &str,
    kind: Option<&sniff::ImageType>,
    timeout: Duration,
) -> bool {
    if is_known_unreachable_for_discord(url) {
        return false;
    }

    match util::verify_image(client, url, kind, timeout, 1) {
        Ok(()) => true,
        Err(util::VerifyError::Mismatch(_)) => false,
        Err(util::VerifyError::Unreachable(_)) => true,
    }
}

fn is_known_unreachable_for_discord(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return true;
    };

    parsed
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("files.catbox.moe"))
}

fn help_text() -> String {
    concat!(
        "Usage: archive-uploader [OPTIONS] [FILE] [URL] [BLUR_PERCENT]\n",
        "\n",
        "Uploads an image, archives it with the Wayback Machine, and prints the\n",
        "archive.org URL to stdout.\n",
        "\n",
        "  FILE           Image path. When omitted, one line is read from stdin.\n",
        "  URL            A previously uploaded URL (foo_discord_rich {url}). It is\n",
        "                 reused when it still serves the image; otherwise FILE is\n",
        "                 uploaded again.\n",
        "  BLUR_PERCENT   Accepted and ignored for {blur_percent} compatibility.\n",
        "\n",
        "Options:\n",
        "  -v, --verbose        Print progress to stderr (do not use with\n",
        "                       foo_discord_rich; it merges stderr into stdout)\n",
        "      --deadline SECS  Total time budget in seconds (default 60, max 3600)\n",
        "      --archive-budget SECS\n",
        "                       Give up on archiving after N seconds and print the\n",
        "                       direct upload URL (default 8, 0 = wait for --deadline)\n",
        "      --skip-archive   Print the direct host URL instead of archiving it\n",
        "      --no-permanent   Skip hosts that keep permanent copies (catbox);\n",
        "                       also enabled by ARCHIVE_UPLOADER_NO_PERMANENT=1\n",
        "  -h, --help           Show this help\n",
        "\n",
        "Hosts are tried in this order: imgur, litterbox, uguu, 0x0.st, tmpfiles,\n",
        "catbox. Imgur is only used when IMGUR_CLIENT_ID was set at build time.\n",
        "Catbox is last because files.catbox.moe is not resolvable from some\n",
        "datacenter resolvers, including Discord's media proxy.\n",
    )
    .to_string()
}

fn exit_with_message(message: String, exit_code: i32) -> ! {
    io::stdout().write_all(message.as_bytes()).unwrap();
    io::stdout().flush().unwrap();
    std::process::exit(exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn classifies_filepath_url_and_blur_percent() {
        assert_eq!(
            classify_positionals(&args(&["C:\\Music\\cover.jpg"])).unwrap(),
            (Some("C:\\Music\\cover.jpg".to_string()), None)
        );

        assert_eq!(
            classify_positionals(&args(&[
                "C:\\Music\\cover.jpg",
                "https://example.com/cover.jpg",
                "50",
            ]))
            .unwrap(),
            (
                Some("C:\\Music\\cover.jpg".to_string()),
                Some("https://example.com/cover.jpg".to_string())
            )
        );

        assert_eq!(
            classify_positionals(&args(&["https://example.com/cover.jpg"])).unwrap(),
            (None, Some("https://example.com/cover.jpg".to_string()))
        );

        assert!(classify_positionals(&args(&["a.jpg", "b.jpg"])).is_err());
        assert!(
            classify_positionals(&args(&["a.jpg", "https://x/1.jpg", "https://x/2.jpg"])).is_err()
        );
    }

    #[test]
    fn catbox_urls_are_never_reused() {
        assert!(is_known_unreachable_for_discord(
            "https://files.catbox.moe/abc.jpg"
        ));
        assert!(is_known_unreachable_for_discord("not a url"));
        assert!(!is_known_unreachable_for_discord(
            "https://litter.catbox.moe/abc.jpg"
        ));
        assert!(!is_known_unreachable_for_discord(
            "https://web.archive.org/web/20261029043717id_/https://example.com/a.jpg"
        ));
    }
}
