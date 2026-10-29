# Rust Archive Uploader

Uploads an image from a local path, has the Internet Archive capture it, and
prints the permanent `archive.org` URL. Built for the Discord Rich Presence
plugin for foobar2000, which passes an album art path in and uses the returned
URL as the presence image.

## How it works

1. Reads a file path from a positional argument, or one line from stdin.
2. If a previously uploaded URL is supplied, reuses it when it still serves an
   image at or below the resize target (when resizing); otherwise it processes
   the file again.
3. Downscales the image: center-crops to a square and applies a Lanczos3 resize
   to 160x160 by default, then encodes it as lossy WebP q90 (JPEG or PNG with
   `--format`).
4. Uploads the file to the first host that accepts it, in this order: imgur
   (optional), litterbox (72 h), uguu (3 h), 0x0.st (1 h), tmpfiles (1 h),
   catbox. With `--race`, all available hosts are attempted concurrently.
5. Submits the URL to the Wayback Machine within the archive budget.
6. If a snapshot is produced, verifies that the archived copy is an image and
   prints `https://web.archive.org/web/<timestamp>id_/<url>`. Otherwise it
   prints the direct upload URL, or exits 1 when `--archive-only` is set.

The uploader is designed for foo_discord_rich, whose default process timeout is
10 seconds. Archiving therefore gets an 8-second budget and falls back to the
direct URL; raise both values when configuring the fork (see below).

An upload failure moves to the next host (or another racing host wins). An
archive failure does not fail the run: the direct URL is printed and the exit
code is 0. Every run is bounded by one deadline. Intermediate uploads expire on
their own (litterbox, uguu, tmpfiles) or are deleted after the capture (0x0.st).

## Build

Requires Rust 1.88 or newer (edition 2024) and a C toolchain, because libwebp is
linked statically: Visual Studio Build Tools with the C++ workload on Windows,
`cc` elsewhere.

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
# -> https://web.archive.org/web/20261029025557id_/https://litter.catbox.moe/ab12cd.webp

echo "C:\Music\Album\cover.jpg" | archive-uploader.exe
```

A previously uploaded URL can be passed as a second positional argument. It is
reused when it still serves an image at or below the resize target; otherwise
the file is uploaded again:

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

The fork substitutes `{filepath}`, `{url}` and `{blur_percent}` and passes them
as arguments instead of writing to stdin. A working custom command, with the
fork's process timeout raised to 130 seconds in the Uploader tab:

```
"C:\path\to\archive-uploader.exe" --archive-only --race --deadline 120 --archive-budget 90 {filepath} {url}
```

- A cached URL is reused only if it still serves an image and, when resizing,
  its dimensions are at or below the target. Dead URLs, HTML responses, and
  `files.catbox.moe` URLs are replaced.
- `{blur_percent}` is ignored; use the fork's bundled uploader if you need
  blurred artwork.
- With `--archive-only`, cached non-archive URLs are rejected and an archive
  failure returns no URL, so nothing new is cached. Remove `{url}` from the
  command if you also want failures to leave the album unresolved instead of
  letting the fork reuse a previously cached URL.
- The fork merges stderr into stdout, so never pass `--verbose` to the command:
  any diagnostic text corrupts the URL it reads.
- Inspect cached URLs with the fork's title-format fields `%drp_artwork_url%`,
  `%drp_artwork_mode%` and `%drp_blurred_artwork_url%`, or open
  `%APPDATA%\foobar2000-v2\foo_discord_rich\image_hashes.json`.

## Parallel upload (--race)

`--race` uploads the image to imgur (when configured), litterbox, uguu, 0x0.st,
tmpfiles and catbox concurrently, then archives the first successful result.
Catbox stays enabled unless `--no-permanent` is passed; its returned URLs are
checked for a live image before they can win, because anonymous catbox uploads
sometimes 404.

This avoids waiting on a slow or dead first host, but it sends the same file to
every raced host on every upload. Those extra copies are not cleaned up
(litterbox/uguu/tmpfiles expire; imgur and catbox copies persist), so use it
only if latency matters. Remaining uploads are terminated when the process exits
after printing the result.

If the archive step fails, the direct fallback can be a catbox URL, which
Discord's media proxy cannot resolve. The archive URL is the reliable result;
use `--no-permanent` to keep catbox out of the race entirely.

## Options

| Option | Description |
| --- | --- |
| `-v`, `--verbose` | Print progress and diagnostics to stderr (do not use with the fork) |
| `--deadline SECS` | Total process budget in seconds (default 60, max 3600) |
| `--archive-budget SECS` | Archive deadline in seconds from process start (default 8, 0 = use `--deadline`) |
| `--race` | Upload to all available hosts concurrently and use the first successful result |
| `--resize PX` | Center-crop and downscale to `PX x PX` (default 160, 0 keeps the original) |
| `--format FMT` | Resized output: `webp` (lossy q90, default), `jpeg`, or `png` (lossless) |
| `--archive-only` | Only print `web.archive.org` URLs; exit 1 instead of printing a direct upload URL |
| `--skip-archive` | Print the direct host URL instead of archiving it |
| `--no-permanent` | Skip catbox, whose anonymous uploads cannot be deleted |
| `-h`, `--help` | Show help |

## Notes

- Wayback captures are permanent and public. Uploading an image publishes it,
  and a capture cannot be removed by this tool.
- Catbox is last in the sequential chain and participates in a race unless
  `--no-permanent` or `ARCHIVE_UPLOADER_NO_PERMANENT=1` is set. Anonymous catbox
  uploads are permanent and cannot be deleted. `files.catbox.moe` also cannot be
  resolved from some datacenter resolvers, including Discord's media proxy, so a
  catbox URL is only a fallback when archiving fails.
- Imgur uploads also persist after the run; the tool does not keep a delete
  token for them.
- The direct URL is only needed until the capture completes. The returned URL
  points at archive.org, so a later host expiry does not break it.
- There is no local pre-verification on the archive path; the archived replay is
  the authority and is checked by its image type. `--skip-archive` verifies the
  direct URL instead.
- Save Page Now requests are retried up to three times with backoff on transient
  errors, including HTTP 503. HTTP 429 stops the retries.
- If the archive is slow or unavailable, the tool exits 0 with the direct upload
  URL instead of failing. That URL is temporary for litterbox, uguu and
  tmpfiles; imgur and catbox are permanent. With `--archive-only`, the tool
  instead exits 1 without printing a URL.
- Resized output is lossy WebP q90 by default. `--format png` preserves every
  pixel; `--format jpeg` uses JPEG q90. Discord's external RPC URLs support
  WebP (including animated WebP), AVIF, GIF, PNG and JPEG.
- JPEG, PNG, BMP and TIFF sources are resized when larger than the target. GIF,
  WebP and SVG sources are uploaded unchanged so animations and vectors are not
  flattened; unknown files are uploaded as `application/octet-stream`.
- Files larger than 128 MiB are rejected.
- Discord requests the activity image at `size=160`, so the default
  `--resize 160` sends a pre-scaled, center-cropped image instead of letting the
  client reduce a 3000 px source in one step. Pass `--resize 0` to upload the
  original and keep the full-resolution archive capture.
