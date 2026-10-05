# Parity status (6-C, final)

The full parity corpus ([`parity/README.md`](../../parity/README.md)) against the finished Rust
image, taken on 2026-10-05 at `rust-rewrite` `f651ed5` (every port merged: waves 1-6B). The image
was built from that commit with `Dockerfile.rust` as `octo-rust:final`, and compared with the C#
baseline recording in [`parity/recordings/csharp/`](../../parity/recordings/csharp/)
(`octo-csharp:csharp-final`, release `2026.10.03.2`).

```sh
docker build -t octo-rust:final .   # Dockerfile.rust at f651ed5; the root Dockerfile since the cutover
python3 parity/parity.py run --image octo-rust:final --project parity-rust --port 18580 --nd-port 18553 \
    --out <scratch>/parity/rust1
python3 parity/parity.py diff               parity/recordings/csharp <scratch>/parity/rust1
python3 parity/parity.py diff --mode bytes  parity/recordings/csharp <scratch>/parity/rust1
```

The recordings are not checked in; the baseline is the only recording the harness keeps.

## Summary

- **453 of 453 requests compared, 0 unexplained diffs**, in structural mode and in bytes mode.
  `diff` exits 0 in both modes. No request was skipped and there were no transport errors, so
  every capture (external ids from discovery `search3`, the browse-session token, Navidrome's
  ids) resolved on the Rust side too.
- **430 requests are identical apart from `Server` and framing** (`Server: Kestrel`, and
  `Transfer-Encoding: chunked` vs `Content-Length` in bytes mode): status, every other header in
  wire order, and the body byte for byte after normalisation.
- **23 requests differ in an allowlisted, reviewed way** (table below): 7 cover JPEGs re-encoded
  by the `image` crate, and 16 static-file answers (`Accept-Ranges` once instead of twice; the
  Brotli/gzip variants of 3 of them come from a different compressor).
- **No flakes.** Four full runs (one of them while `cargo test --workspace` loaded every core)
  gave the same result, and the four Rust recordings are byte-identical to each other
  (`diff --mode bytes` with no allowlist: 453 identical).

## Per corpus group

The same table for each of the four runs.

| Group | Requests | Identical apart from `Server`/framing (structural) | (bytes) | Other allowlisted diffs | Unexplained (structural / bytes) |
|---|---:|---:|---:|---|---:|
| `00-setup` | 6 | 6 | 6 | 0 | 0 / 0 |
| `01-system` | 43 | 43 | 43 | 0 | 0 / 0 |
| `02-browsing` | 65 | 65 | 65 | 0 | 0 / 0 |
| `03-search` | 25 | 25 | 25 | 0 | 0 / 0 |
| `04-media` | 42 | 35 | 35 | `cover-jpeg-bytes` 7 | 0 / 0 |
| `05-playlists` | 19 | 19 | 19 | 0 | 0 / 0 |
| `06-lyrics` | 24 | 24 | 24 | 0 | 0 / 0 |
| `07-octo-extensions` | 15 | 15 | 15 | 0 | 0 / 0 |
| `08-catchall-native` | 34 | 34 | 34 | 0 | 0 / 0 |
| `09-admin` | 97 | 97 | 97 | 0 | 0 / 0 |
| `10-static-cors` | 25 | 9 | 9 | `static-accept-ranges-once` 16 (3 of them also `static-compressed-variants`) | 0 / 0 |
| `90-mutations` | 58 | 58 | 58 | 0 | 0 / 0 |
| **Total** | **453** | **430** | **430** | **23** | **0 / 0** |

## Allowlisted diffs

Every entry in [`parity/allowlist.json`](../../parity/allowlist.json) quotes a reviewed row of
[`known-diffs.md`](known-diffs.md). Uses in the final run:

| Allowlist id | Aspects used (structural / bytes) | known-diffs row | Notes |
|---|---:|---|---|
| `server-header` | 453 / 453 | HTTP | axum sends no `Server` header. |
| `body-framing` | 0 / 422 | HTTP | Rust sends `Content-Length` where Kestrel streamed chunked; every length change that matters shows as a body diff, and there are none. |
| `static-accept-ranges-once` | 16 / 16 | Static files | One `Accept-Ranges: bytes` instead of two. |
| `static-compressed-variants` | 6 / 6 | Static files | `index-html-br`, `index-html-gzip`, `admin-js-br`: different compressor bytes; the decompressed content and the weak ETag are the same. |
| `cover-jpeg-bytes` | 7 / 7 | List covers: JPEG bytes | Same status, type and size in pixels. Checked perceptually in this run (ffmpeg `ssim`, RGB): the 600×600 placeholder (`cover-octo-radio`, `cover-legacy-ext-*`, and the not-diffing `cover-external-playlist`) **0.9926**, the 300×300 badged outside covers (`cover-external-song`/`-album`/`-artist`) **0.9937**, all above PLAN's 0.98. `cover-external-song-octo-client` is byte-identical. Rust's JPEGs are larger (17.6 KB vs 11.8 KB, 4.0 KB vs 2.6 KB) because the `image` encoder always writes 4:4:4. |

## Beyond the corpus

The upgrade and downgrade runs (C# and Rust taking turns on one `/app/config` and `/music`), the
fixture-state check, the settings round trip and the load test are in
[`cutover-report.md`](cutover-report.md). Two findings from the earlier status are closed:

- **First-run auto-detect adopting Octo itself:** fixed (known-diffs "First-run discovery").
  A lone `octo-rust:final` container with no `SUBSONIC_URL` now logs `Server discovery: found 0
  Subsonic server(s).`
- **Startup compression:** the variants are now compressed in the background (known-diffs
  "Static files"); cold start is 0.50-0.55 s.

## Harness note: the stubs and Nagle

The stub server (`parity/stubs/stub_server.py`) writes headers and body in separate TCP writes
with Nagle on, so a keep-alive caller waits for a 40 ms delayed ACK on some calls. It does not
change any recorded body (the corpus is unaffected), but it dominates `search3` timings: with
the stock stubs a sequential `search3` takes 86 ms (both builds), and with
`disable_nagle_algorithm = True` it takes 10-11 ms. The load test in `cutover-report.md` uses a
patched copy of the stubs for that reason; the checked-in stubs were left alone so the baseline
recording stays valid.
