# Rust Archive Uploader

Uploads an image from a local path, has the Internet Archive capture it, and
prints the permanent `archive.org` URL. Built for the Discord Rich Presence
plugin for foobar2000, which pipes an album art path in and uses the returned
URL as the presence image.

## How it works

1. Reads a file path from a positional argument, or one line from stdin.
2. Uploads the file to the first host that accepts it, in this order:
   imgur (optional), litterbox (72 hours), uguu (3 hours), 0x0.st (1 hour),
   tmpfiles (1 hour), catbox. No flags are needed for this default behavior.
3. Checks that the returned URL is a plain HTTPS URL on the expected host.
4. Downscales the image to 160x160 by default (center crop, Lanczos3, lossy
   WebP q90), then submits the URL to the Wayback Machine within the archive
   budget.
5. If a snapshot is produced, checks that the archived copy serves the image
   and prints `https://web.archive.org/web/<timestamp>id_/<url>`. Otherwise it
   prints the direct upload URL instead.

The uploader is designed for foo_discord_rich, which kills the process after
10 seconds, so archiving gets an 8 second budget by default and the direct
upload URL is used as a fallback. Discord's media proxy fetches that URL, so
catbox is tried last: `files.catbox.moe` is not resolvable from some datacenter
resolvers, which makes the image fail to load in Rich Presence.

If an upload or archive step fails, the next host is tried. The whole run is
bounded by a single deadline. The intermediate upload expires on its own
(litterbox, uguu, tmpfiles) or is deleted after the capture (0x0.st).

## Build

Requires Rust 1.88 or newer (edition 2024).

```sh
cargo build --release --locked
```

Imgur is an optional fallback: set `IMGUR_CLIENT_ID` at build time to enable it.

```sh
IMGUR_CLIENT_ID=your_client_id cargo build --release
```

## Usage

Two invocation styles are supported: a path argument, or the path on stdin.

```sh
archive-uploader.exe "C:\Music\Album\cover.jpg"
# -> https://web.archive.org/web/20261029025557id_/https://litter.catbox.moe/ab12cd.jpg

echo "C:\Music\Album\cover.jpg" | archive-uploader.exe
```

A previously uploaded URL can be passed as a second positional argument. It is
reused when it still serves the image; otherwise the file is uploaded again:

```sh
archive-uploader.exe "C:\Music\Album\cover.jpg" "https://web.archive.org/web/..."
```

The path may be quoted; surrounding quotes are stripped. Only the first stdin
line is read. A third numeric argument (`{blur_percent}`) is accepted and
ignored.

The URL is printed to stdout without a trailing newline. Errors are printed to
stdout as well with exit code 1, matching the original tool's contract. Nothing
is written to stderr on success unless `--verbose` is used.

## foo_discord_rich (wsnrq fork)

This fork substitutes `{filepath}`, `{url}` and `{blur_percent}` into the upload
command and passes them as arguments instead of writing to stdin. Configure:

```
"C:\path\to\archive-uploader.exe" {filepath} {url}
```

- When the cached URL is still reachable it is printed back unchanged, so no
  re-upload happens.
- A dead URL, an HTML response, or a `files.catbox.moe` URL causes a fresh
  upload (catbox is unreachable from Discord's media proxy).
- `{blur_percent}` is ignored; use the fork's bundled uploader if you need
  blurred artwork.
- The fork merges stderr into stdout, so never pass `--verbose` to the command:
  any diagnostic text corrupts the URL it reads.
- The fork's default process timeout is 10 seconds; the uploader aims to answer
  within 8 and otherwise prints the direct URL.

## Parallel upload (--race)

`--race` uploads the image to imgur (when configured), litterbox, uguu, 0x0.st,
tmpfiles and catbox concurrently, then archives the first successful result.
Catbox stays enabled unless `--no-permanent` is passed; its returned URLs are
checked for a live image before they can win, because anonymous catbox uploads
sometimes 404.

This avoids waiting on a slow or dead first host, but it sends the same file to
every raced host on every upload. Those extra copies are not cleaned up
(litterbox/uguu/tmpfiles expire; imgur and catbox copies persist), so use it
only if the latency matters. Remaining uploads are terminated when the process
exits after printing the result.

If the archive step fails, the direct fallback can be a catbox URL, which
Discord's media proxy cannot resolve. The archive URL is the reliable result;
use `--no-permanent` to keep catbox out of the race entirely.

```
"C:\path\to\archive-uploader.exe" --race --deadline 120 --archive-budget 90 {filepath} {url}
```

## Options

| Option | Description |
| --- | --- |
| `-v`, `--verbose` | Print progress and diagnostics to stderr |
| `--deadline SECS` | Total time budget in seconds (default 60, max 3600) |
| `--archive-budget SECS` | Seconds to spend on archiving before printing the direct URL (default 8, 0 = wait for `--deadline`) |
| `--race` | Upload to all available hosts concurrently and use the first successful result |
| `--resize PX` | Downscale artwork to PX (square, Lanczos3; default 160, 0 keeps the original) |
| `--format FMT` | Resized output format: `webp` (lossy q90, default), `jpeg`, or `png` |
| `--skip-archive` | Print the direct host URL instead of archiving it |
| `--no-permanent` | Skip catbox, whose anonymous uploads cannot be deleted |
| `-h`, `--help` | Show help |

## Notes

- Wayback captures are permanent and public. Uploading an image publishes it,
  and a capture cannot be removed by this tool.
- Catbox is tried last and anonymous catbox uploads are permanent and cannot
  be deleted. Pass `--no-permanent` or set
  `ARCHIVE_UPLOADER_NO_PERMANENT=1` to skip it entirely.
- Imgur uploads also persist after the run; the tool does not keep a delete
  token for them.
- The host URLs are only needed until the capture completes; the returned URL
  points at archive.org, so a later host expiry does not break it.
- If the uploaded URL is unreachable from the local machine but the host still
  returned a valid URL, the Wayback Machine is asked to fetch it anyway; the
  archived copy is then verified byte-for-byte by content type. A host that
  answers with a 404 or a non-image body is rejected immediately.
- Save Page Now requests are retried up to three times with backoff on
  transient errors. HTTP 429 stops the retries.
- If the archive is slow or unavailable, the tool exits 0 with the direct
  upload URL instead of failing. That URL is temporary for the default hosts
  (imgur, when configured, is the exception).
- Supported inputs are PNG, JPEG, GIF, WebP, BMP, TIFF and SVG. Other files are
  uploaded as `application/octet-stream` and accepted if the host serves them
  back unchanged.
- Files larger than 128 MiB are rejected.
- Discord requests the activity image at `size=160`, so the default `--resize
  160` sends a pre-scaled, center-cropped image instead of letting the client
  reduce a 3000 px source in one step. Pass `--resize 0` to upload the original
  and keep the full-resolution archive capture. Resized output is lossy WebP
  q90 by default; `--format jpeg` or `--format png` changes it. Discord's external
  RPC URLs support WebP (including animated WebP), AVIF, GIF, PNG and JPEG.
  GIF, animated WebP and SVG sources are uploaded unchanged.
- Save Page Now rate limiting (HTTP 429 or 503) stops the run instead of
  retrying every remaining host.
