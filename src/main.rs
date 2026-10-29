mod archive;
mod providers;
mod sniff;
mod util;

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::redirect;

use providers::Uploaded;

const MAX_UPLOAD_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DEADLINE_SECS: u64 = 3600;
const DEFAULT_DEADLINE_SECS: u64 = 60;
const DEFAULT_ARCHIVE_BUDGET_SECS: u64 = 8;
const DEFAULT_RESIZE_PX: u32 = 160;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(10);
const URL_CHECK_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Webp,
    Jpeg,
    Png,
}

struct Options {
    verbose: bool,
    deadline: Duration,
    archive_budget: Duration,
    skip_archive: bool,
    skip_permanent: bool,
    race: bool,
    resize: u32,
    format: OutputFormat,
    archive_only: bool,
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

    let kind: Option<&'static sniff::ImageType> = file_data.as_deref().and_then(sniff::detect);
    let resize_target = (options.resize > 0).then_some(options.resize);

    if let Some(url) = &options.cached_url {
        if should_reuse_url(
            &strict_client,
            url,
            kind,
            resize_target,
            options.archive_only,
            Duration::from_secs(3),
        ) {
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

    let mut data = match file_data {
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

    let mut kind = kind;
    let filepath = filepath.unwrap_or_default();
    let (mut mime, mut extension) = match kind {
        Some(kind) => (kind.mime, kind.ext),
        None => sniff::from_extension(Path::new(&filepath)),
    };

    if let Some(target) = resize_target
        && let Some((resized, resized_mime, resized_extension)) =
            resize_for_discord(&data, target, options.format)
    {
        data = resized;
        kind = sniff::detect(&data);
        mime = resized_mime;
        extension = resized_extension;

        if options.verbose {
            eprintln!("resized to {}x{} ({} bytes)", target, target, data.len());
        }
    }

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

    if options.race {
        let candidates: Vec<providers::Provider> = providers::CHAIN
            .iter()
            .copied()
            .filter(|provider| provider.available())
            .filter(|provider| !(provider.is_permanent() && options.skip_permanent))
            .collect();

        if candidates.is_empty() {
            failures.push("race: no providers available".to_string());
        } else {
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .min(UPLOAD_TIMEOUT);

            match race_uploads(
                &client,
                &strict_client,
                &candidates,
                &input,
                kind,
                timeout,
                deadline,
            ) {
                Ok(winner) => {
                    if options.verbose {
                        eprintln!("race winner: {} {}", winner.provider, winner.url);
                    }
                    archive_or_print(
                        &client,
                        &strict_client,
                        winner,
                        kind,
                        &options,
                        archive_deadline,
                        deadline,
                    );
                }
                Err(errors) => {
                    failures.extend(errors);
                    if options.verbose {
                        eprintln!("race failed; falling back to the permanent host");
                    }
                }
            }
        }
    }

    let chain: Vec<providers::Provider> = if options.race {
        Vec::new()
    } else {
        providers::CHAIN.to_vec()
    };

    for provider in chain {
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

        archive_or_print(
            &client,
            &strict_client,
            uploaded,
            kind,
            &options,
            archive_deadline,
            deadline,
        );
    }

    exit_with_message(
        format!("error: all options failed\n{}\n", failures.join("\n")),
        1,
    );
}

fn archive_or_print(
    client: &Client,
    strict_client: &Client,
    uploaded: Uploaded,
    kind: Option<&sniff::ImageType>,
    options: &Options,
    archive_deadline: Instant,
    deadline: Instant,
) -> ! {
    if options.skip_archive {
        let check_timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(URL_CHECK_TIMEOUT);

        if let Err(error) = util::verify_image(strict_client, &uploaded.url, kind, check_timeout, 1)
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
        archive::archive(client, strict_client, &uploaded.url, kind, archive_deadline)
    };

    match archived {
        Ok(link) => {
            if options.verbose {
                eprintln!("archived {}", link);
            }
            cleanup(client, &uploaded, options.verbose);
            exit_with_message(link, 0);
        }
        Err(error) => {
            if options.archive_only {
                let message = if options.verbose {
                    format!(
                        "error: no archive.org URL was produced: {}\n",
                        error.message
                    )
                } else {
                    "error: no archive.org URL was produced\n".to_string()
                };
                exit_with_message(message, 1);
            }

            if options.verbose {
                eprintln!("warning: {}; returning the direct URL", error.message);
            }
            exit_with_message(uploaded.url, 0);
        }
    }
}

fn race_uploads(
    client: &Client,
    strict_client: &Client,
    candidates: &[providers::Provider],
    input: &providers::Input<'_>,
    kind: Option<&'static sniff::ImageType>,
    timeout: Duration,
    deadline: Instant,
) -> Result<Uploaded, Vec<String>> {
    let (sender, receiver) = mpsc::channel();
    let data = Arc::new(input.data.to_vec());
    let filename = Arc::new(input.filename.to_string());
    let mime = Arc::new(input.mime.to_string());

    for provider in candidates.iter().copied() {
        let sender = sender.clone();
        let client = client.clone();
        let strict_client = strict_client.clone();
        let data = Arc::clone(&data);
        let filename = Arc::clone(&filename);
        let mime = Arc::clone(&mime);

        std::thread::spawn(move || {
            let input = providers::Input {
                data: data.as_slice(),
                filename: filename.as_str(),
                mime: mime.as_str(),
            };

            let result = match provider.upload(&client, &input, timeout) {
                Ok(uploaded) if provider.is_permanent() => match util::verify_image(
                    &strict_client,
                    &uploaded.url,
                    kind,
                    timeout.min(Duration::from_secs(2)),
                    1,
                ) {
                    Ok(()) | Err(util::VerifyError::Unreachable(_)) => Ok(uploaded),
                    Err(error) => Err(format!(
                        "{}: unusable file URL: {}",
                        provider.name(),
                        error.message()
                    )),
                },
                result => result,
            };

            let _ = sender.send(result);
        });
    }

    drop(sender);

    let mut failures = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match receiver.recv_timeout(remaining) {
            Ok(Ok(uploaded)) => return Ok(uploaded),
            Ok(Err(error)) => failures.push(error),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }

    Err(failures)
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
        race: false,
        resize: DEFAULT_RESIZE_PX,
        format: OutputFormat::Webp,
        archive_only: false,
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
            "--race" => options.race = true,
            "--archive-only" => options.archive_only = true,
            "--resize" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--resize needs a value in pixels".to_string())?;
                options.resize = parse_resize(&value)?;
            }
            "--format" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--format needs webp, jpeg or png".to_string())?;
                options.format = parse_format(&value)?;
            }
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
                } else if let Some(value) = arg.strip_prefix("--resize=") {
                    options.resize = parse_resize(value)?;
                } else if let Some(value) = arg.strip_prefix("--format=") {
                    options.format = parse_format(value)?;
                } else if arg.starts_with('-') && arg != "-" {
                    return Err(format!("unknown argument: {}", arg));
                } else {
                    positional.push(arg);
                }
            }
        }
    }

    if options.archive_only && options.skip_archive {
        return Err("--archive-only cannot be combined with --skip-archive".to_string());
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

fn parse_resize(value: &str) -> Result<u32, String> {
    let pixels: u32 = value
        .parse()
        .map_err(|_| format!("invalid resize: {}", value))?;

    if pixels != 0 && !(32..=1024).contains(&pixels) {
        return Err("resize must be 0 or between 32 and 1024 pixels".to_string());
    }

    Ok(pixels)
}

fn parse_format(value: &str) -> Result<OutputFormat, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "webp" => Ok(OutputFormat::Webp),
        "jpeg" | "jpg" => Ok(OutputFormat::Jpeg),
        "png" => Ok(OutputFormat::Png),
        _ => Err(format!(
            "unsupported format: {} (use webp, jpeg or png)",
            value
        )),
    }
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
    target: Option<u32>,
    archive_only: bool,
    timeout: Duration,
) -> bool {
    if is_known_unreachable_for_discord(url) {
        return false;
    }

    if archive_only && !is_archive_url(url) {
        return false;
    }

    match util::fetch_bytes(client, url, 32 * 1024, timeout) {
        Ok((prefix, _)) => {
            if sniff::looks_like_html(&prefix) {
                return false;
            }

            if target.is_none()
                && let Some(kind) = kind
                && !kind.matches(&prefix)
            {
                return false;
            }

            if let Some(target) = target
                && let Some((width, height)) = image_dimensions(&prefix)
                && width.max(height) > target
            {
                return false;
            }

            true
        }
        Err(util::VerifyError::Mismatch(_)) => false,
        Err(util::VerifyError::Unreachable(_)) => true,
    }
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn resize_for_discord(
    data: &[u8],
    target: u32,
    format: OutputFormat,
) -> Option<(Vec<u8>, &'static str, &'static str)> {
    if data.starts_with(b"GIF8")
        || (data.starts_with(b"RIFF") && data.len() >= 12 && &data[8..12] == b"WEBP")
    {
        return None;
    }

    let reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;

    if width == 0 || height == 0 || width.max(height) <= target {
        return None;
    }
    if u64::from(width) * u64::from(height) > 80_000_000 {
        return None;
    }

    let source = image::load_from_memory(data).ok()?;
    let side = width.min(height);
    let cropped = source.crop_imm((width - side) / 2, (height - side) / 2, side, side);
    let resized = cropped.resize_exact(target, target, image::imageops::FilterType::Lanczos3);

    match format {
        OutputFormat::Webp => encode_webp(&resized).or_else(|| encode_fallback(&resized)),
        OutputFormat::Jpeg => encode_jpeg(&resized),
        OutputFormat::Png => encode_png(&resized),
    }
}

fn encode_webp(image: &image::DynamicImage) -> Option<(Vec<u8>, &'static str, &'static str)> {
    let encoded = if image.color().has_alpha() {
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        webp::Encoder::from_rgba(rgba.as_raw(), width, height).encode(90.0)
    } else {
        let rgb = image.to_rgb8();
        let (width, height) = rgb.dimensions();
        webp::Encoder::from_rgb(rgb.as_raw(), width, height).encode(90.0)
    };

    Some((encoded.to_vec(), "image/webp", "webp"))
}

fn encode_jpeg(image: &image::DynamicImage) -> Option<(Vec<u8>, &'static str, &'static str)> {
    let mut buffer = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, 90);
    encoder.encode_image(&image.to_rgb8()).ok()?;
    Some((buffer, "image/jpeg", "jpg"))
}

fn encode_png(image: &image::DynamicImage) -> Option<(Vec<u8>, &'static str, &'static str)> {
    let mut buffer = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .ok()?;
    Some((buffer, "image/png", "png"))
}

fn encode_fallback(image: &image::DynamicImage) -> Option<(Vec<u8>, &'static str, &'static str)> {
    if image.color().has_alpha() {
        encode_png(image)
    } else {
        encode_jpeg(image)
    }
}

fn is_archive_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "web.archive.org")
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
        "      --race           Upload to all available hosts at once and use the\n",
        "                       first successful result\n",
        "      --resize PX      Downscale artwork to PX (square, Lanczos3; 0 = keep\n",
        "                       the original; default 160)\n",
        "      --format FMT     Resized output format: webp, jpeg or png (default\n",
        "                       webp, lossy q90)\n",
        "      --archive-only   Only print web.archive.org URLs; exit 1 instead\n",
        "                       of printing a direct upload URL\n",
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
    fn archive_urls_are_recognized() {
        assert!(is_archive_url(
            "https://web.archive.org/web/20261029053236id_/https://example.com/a.webp"
        ));
        assert!(!is_archive_url("https://litter.catbox.moe/a.webp"));
        assert!(!is_archive_url("https://files.catbox.moe/a.webp"));
        assert!(!is_archive_url("not a url"));
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

    fn sample_png(width: u32, height: u32) -> Vec<u8> {
        let source = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([x as u8, y as u8, (x + y) as u8])
        });
        let mut data = Vec::new();
        image::DynamicImage::ImageRgb8(source)
            .write_to(
                &mut std::io::Cursor::new(&mut data),
                image::ImageFormat::Png,
            )
            .unwrap();
        data
    }

    #[test]
    fn downscales_to_square_webp_by_default() {
        let data = sample_png(400, 200);

        let (resized, mime, extension) =
            resize_for_discord(&data, 160, OutputFormat::Webp).unwrap();

        assert_eq!(mime, "image/webp");
        assert_eq!(extension, "webp");
        assert!(resized.starts_with(b"RIFF"));
        assert_eq!(image_dimensions(&resized), Some((160, 160)));
    }

    #[test]
    fn can_encode_jpeg() {
        let data = sample_png(400, 400);

        let (resized, mime, extension) =
            resize_for_discord(&data, 160, OutputFormat::Jpeg).unwrap();

        assert_eq!(mime, "image/jpeg");
        assert_eq!(extension, "jpg");
        assert_eq!(image_dimensions(&resized), Some((160, 160)));
    }

    #[test]
    fn keeps_images_that_are_already_small() {
        let source = image::RgbImage::from_fn(100, 100, |_, _| image::Rgb([1, 2, 3]));
        let mut data = Vec::new();
        image::DynamicImage::ImageRgb8(source)
            .write_to(
                &mut std::io::Cursor::new(&mut data),
                image::ImageFormat::Png,
            )
            .unwrap();

        assert!(resize_for_discord(&data, 160, OutputFormat::Webp).is_none());
    }

    #[test]
    fn skips_animated_formats() {
        assert!(resize_for_discord(b"GIF89a\0\0", 160, OutputFormat::Webp).is_none());
    }
}
