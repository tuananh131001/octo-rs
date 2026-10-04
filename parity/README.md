# Octo HTTP parity harness

A black-box harness for the C# → Rust rewrite (`docs/rust-migration/PLAN.md`, Phase 0). It
boots one Octo build against a fixture Navidrome and stubs for every upstream, replays a
hand-written request corpus, records every response, and diffs two recordings after
normalising the values that change from run to run.

The C# baseline (`octo-csharp:csharp-final`, release `2026.10.03.2`) is recorded in
[`recordings/csharp/`](recordings/csharp/). A Rust build passes when its recording diffs clean
against it, or when every remaining diff is covered by [`allowlist.json`](allowlist.json),
which must point at a row in [`docs/rust-migration/known-diffs.md`](../docs/rust-migration/known-diffs.md).

Everything is Python 3.10+ standard library plus Docker Compose. No `pip install`.

## Running it

```sh
# Record a build: recreate the stack from scratch, replay the corpus, tear it down again.
python3 parity/parity.py run --image octo-csharp:csharp-final --out /tmp/parity/csharp-again

# Compare two recordings. Exit 0 = no unexplained diff, 1 = diffs (report on stdout).
python3 parity/parity.py diff parity/recordings/csharp /tmp/parity/csharp-again
python3 parity/parity.py diff --mode bytes parity/recordings/csharp /tmp/parity/csharp-again

# The Rust build, once an image exists. A different project and ports let it run beside a C# stack.
python3 parity/parity.py run --image octo-rust:dev --project parity-rust --port 18580 --nd-port 18553 \
    --out /tmp/parity/rust
python3 parity/parity.py diff parity/recordings/csharp /tmp/parity/rust --report /tmp/parity/rust.txt
```

The steps can also run one at a time, for example to poke at a live stack:

```sh
python3 parity/parity.py up                  # fresh stack; Octo on 127.0.0.1:18480, Navidrome on :18453
python3 parity/parity.py record --target http://127.0.0.1:18480 --out /tmp/parity/x [--only '04-media/*'] [-v]
python3 parity/parity.py show /tmp/parity/x '09-admin/settings*'      # print recorded entries
docker compose -p parity logs stubs | grep UNMATCHED                  # upstream calls with no stub
python3 parity/parity.py down                # containers, networks and volumes (never images)
```

Options: `--image` (also `OCTO_IMAGE`), `--port` (`PARITY_PORT`, default 18480), `--nd-port`
(`PARITY_ND_PORT`, 18453), `--project` (`PARITY_PROJECT`, `parity`), `--settle` (seconds to wait
after Octo first answers, default 10), `--keep` on `run` to leave the stack up.

A run takes about 90 seconds. The stack is capped at roughly 1.4 GB of RAM (`mem_limit` on each
service) and needs two small images besides Octo: `deluan/navidrome:0.64.2` and
`python:3.13-alpine`. Nothing is built; nothing is pruned.

**Re-recording the baseline** (only when the corpus, the stubs or the fixtures change):

```sh
rm -rf parity/recordings/csharp
python3 parity/parity.py run --out parity/recordings/csharp
python3 parity/parity.py run --out /tmp/parity/check
python3 parity/parity.py diff parity/recordings/csharp /tmp/parity/check                # must be 0
python3 parity/parity.py diff --mode bytes parity/recordings/csharp /tmp/parity/check   # must be 0
```

## What runs

```
                 host 127.0.0.1:18480 / :18453
                              │
                         ┌─────────┐     network "edge" (published ports only)
                         │ gateway │
                         └────┬────┘     network "sandbox" (internal: no route out)
        ┌─────────────────────┼──────────────────────┐
   ┌────┴────┐          ┌─────┴─────┐          ┌─────┴─────┐
   │  octo   │ ───────▶ │ navidrome │          │   stubs   │  slskd :5030, Lidarr :8686,
   │ (image  │          │  0.64.2   │          │ (python)  │  yt-dlp shim :8090,
   │ under   │ ─────────────────────────────▶  │           │  HTTPS :443 for every
   │ test)   │                                 │           │  hard-coded upstream host
   └─────────┘                                 └───────────┘
```

| Service | What it is |
|---|---|
| `octo` | The image under test (`OCTO_IMAGE`). Configured only through env vars in [`docker-compose.yml`](docker-compose.yml); `/app/config` is a fresh volume each run. |
| `navidrome` | `deluan/navidrome:0.64.2` over the fixture library (read-only). External agents, insights and the auth rate limiter are off. |
| `stubs` | [`stubs/stub_server.py`](stubs/stub_server.py), one process answering slskd, Lidarr, the yt-dlp shim and every HTTPS upstream from the JSON mappings in [`stubs/mappings/`](stubs/mappings/). |
| `gateway` | [`stubs/gateway.py`](stubs/gateway.py), a TCP forwarder. Docker cannot publish ports from an internal-only network, so this is the one container on both networks. |

### Fixtures

- **Music library** ([`fixtures/music/`](fixtures/music/), 260 KB, checked in): 10 tracks of 2-4 s
  sine tones by 3 artists on 3 albums, one container per album: FLAC (with `cover.jpg` and a
  sidecar `.lrc`), MP3 (ID3v2.4, `folder.jpg`) and M4A/AAC (no cover). Names include
  `Café del Mar`, `Bjørn Ålund`, `Ágætis Prófun`, `Þögn`, `宇多田テスト`, `桜の歌` and `Ñandú (Remix)`.
  [`fixtures/make-library.sh`](fixtures/make-library.sh) regenerates it with ffmpeg using
  bit-exact flags and fixed mtimes, and writes `fixtures/music.sha256`. The output is
  byte-identical between runs of one ffmpeg build, but encoder output differs between builds,
  and Navidrome reports sizes and bit rates, so the files are checked in. Regenerating means
  re-recording the baseline. Git does not keep mtimes, and Navidrome relays them as
  `Last-Modified` (`download`, `HEAD stream`), so `parity.py up` sets every file and folder
  back to the baseline's instant (`2020-01-01T20:04:05Z`) before it starts the stack.
- **Navidrome users:** `admin` / `parity-admin` (created by `ND_DEVAUTOCREATEADMINPASSWORD`)
  and the non-admin `listener` / `listener-pass` (created by `parity.py up` through the
  native API).
- **Octo config:** Navidrome URL and admin login, slskd/Lidarr/shim URLs pointing at the stubs,
  a Last.fm API key (so search discovery runs) with radio, stations and mixes off, lyrics
  fetching on with every source, update checks off, library actions off. See the comments in
  `docker-compose.yml`.

### Hard-coded HTTPS upstreams

Octo calls these with fixed `https://` URLs (from `grep -rn 'https://' octo/`): `api.deezer.com`
(plus the `*.dzcdn.net` image CDN), `itunes.apple.com` (and `is1-ssl.mzstatic.com`),
`musicbrainz.org`, `coverartarchive.org`, `api.acoustid.org`, `ws.audioscrobbler.com` and
`www.last.fm`, `lrclib.net`, `lyrics.kugou.com`, `mobileservice.kugou.com`, `music.163.com`,
`api.lyrics.ovh`, `api.github.com`, `api.listenbrainz.org` and `ntfy.sh`.

**Approach: intercept, don't switch off.** The `stubs` container carries a docker network alias
for each of those host names, so inside the sandbox they resolve to it. It serves TLS on 443
with one certificate whose SANs list every host, signed by a test CA
([`certs/`](certs/), regenerated by `certs/make-certs.sh`, keys checked in on purpose). The Octo
container gets that CA as its only root store through `SSL_CERT_FILE` / `SSL_CERT_DIR`
(OpenSSL, which .NET on Linux uses, honours both), so no derived image is needed. The stub
routes by the `Host` header. Because the network is `internal`, any host without an alias
fails to resolve instead of reaching the internet, which keeps every run deterministic.

**For the Rust build:** this works as long as its HTTP client trusts the platform store as
configured by `SSL_CERT_FILE` (reqwest with `rustls-native-certs` or `rustls-platform-verifier`
on Linux, or `native-tls`). A client built with compiled-in `webpki-roots` would reject the
stubs; in that case add the parity CA another way rather than turning features off.
Checked on 2026-10-04 (task 2-D) with the workspace reqwest 0.13 (`rustls`, platform verifier):
a request to `https://api.deezer.com` resolved to an `openssl s_server` presenting
`certs/stub.crt` succeeds with `SSL_CERT_FILE=certs/ca.crt`, and equally with only
`SSL_CERT_DIR` pointing at a directory holding `ca.crt`; without either it fails with
`InvalidCertificate(UnknownIssuer)`.

Keep the host list in three places in sync: `certs/make-certs.sh` (SANs), the `stubs` aliases in
`docker-compose.yml`, and the mappings.

### Stub mappings

`stubs/mappings/*.json` are tried in file-name order, then in list order; the first match wins.
Each entry names a `service` (the Host header for HTTPS hosts, else `slskd`, `lidarr` or
`shim`), an optional `method`, a `path` or `pathRegex` (matched against the raw,
percent-encoded path), optional `query` regexes, and a `body` (JSON), `bodyText` or `bodyFile`
(served with Range support). Unmatched calls get `404 {"error":"no stub"}` and an `UNMATCHED`
line in `docker compose -p parity logs stubs`.

The data set ([`40-discovery.json`](stubs/mappings/40-discovery.json)): Last.fm `track.search`
returns three songs by the discovery-only artist **Zephyr Echo** (`Glass Harbor`, `Neon Tide`,
`Paper Moons`) for queries containing "zephyr", and `Café del Mar` (local) plus `Borealis`
(external) for "aurora". Deezer knows artist 1001, album 2001 and tracks 3001-3003. LRCLIB has
synced lyrics for `Polar Night`, lyrics.ovh has `Glass Harbor`. Everything else gets an empty
or not-found answer in the upstream's own shape ([`90-https-fallbacks.json`](stubs/mappings/90-https-fallbacks.json)).
The shim answers every search with video `parityVid01` and streams `stubs/bodies/tone.mp3`.

## The corpus

[`corpus/*.json`](corpus/) runs in file order, request by request, against one fresh stack.

```jsonc
{
  "group": "subsonic-browsing",
  "description": "...",
  "defaults": {"auth": "p"},              // merged into every request
  "requests": [
    {"name": "song-local-json",           // unique per file; the id is "<file>/<name>"
     "method": "GET",                     // default GET
     "path": "/rest/getSong",
     "auth": "p",                         // p | t (token+salt) | enc | bad | listener | apikey | u-only | none
     "client": "parity",                  // the c= parameter
     "f": "json",                         // appended after the auth params; null drops a default
     "query": [["id", "{{song_flac1}}"]], // pairs keep order and repeats; or "rawQuery": "..."
     "headers": {"Range": "bytes=0-9"},
     "body": {"json": {...}},             // or {"form": [[k, v]]} or {"raw": "...", "contentType": "..."}
     "capture": {"var": "json:a.b[0].c"}, // also header:Name, cookie:name, regex:(...)
     "unordered": true,                   // upstream order undefined: sort arrays / children
     "ignoreKeys": ["positionMs"],        // drop these members/attributes before comparing
     "delayBeforeMs": 1500,
     "timeout": 60}
  ]
}
```

`{{var}}` substitutes a captured value or a built-in (`admin_user`, `admin_pass`,
`listener_user`, `listener_pass`, `salt`, `token`, `enc_pass`, `api_key`, `host`). A capture
written as a string is **volatile**: the diff replaces its value with `{{var}}` everywhere in
that recording (Navidrome's random song, user, playlist and session ids). A capture written as
`{"from": "...", "normalize": false}` is only reused, and must itself match between builds
(name-derived album/artist ids, Octo's sha256-derived external ids, static-file ETags).

| File | Requests | Covers (endpoints.md section) |
|---|---:|---|
| `00-setup` | 6 | Captures: album/artist/song ids, Navidrome JWT via `POST /auth/login` through the relay, external ids from a discovery search |
| `01-system` | 43 | 3.1 and 2.2-2.4: `ping` in xml/json, `.view`/plain, auth `u/p`, `t/s`, `enc:`, wrong password, non-admin, `apiKey`, `u` only, none; `jsonp`, `f=JSON`, `F=json`, case and trailing slash, POST form/JSON bodies (body overrides query), PUT/HEAD/OPTIONS falling to the catch-all; `getOpenSubsonicExtensions` merge; `jukeboxControl`; system relays |
| `02-browsing` | 65 | 3.2 and 3.9 relays: `getSong`/`getArtist`/`getAlbum` local (merge with the catalog), external, legacy `ext-`, `pl-`, missing and unknown ids, jsonp; album/artist info v1/v2; random and similar songs (radio off); `getArtists`, `getIndexes`, `getMusicDirectory`, album lists, genres, starred, now playing, top songs, play queue, bookmarks, shares, repeated ids |
| `03-search` | 25 | 3.3: `search3`/`search2` local-only, mixed, discovery-only, CJK and diacritics, quoted, type-ahead, songs/albums only, album offset, later song page, empty query, Symfonium sync walk, folder, wrong password, jsonp, POST form |
| `04-media` | 42 | 3.4: `stream` local (FLAC/MP3/M4A, Range, open-ended Range), external through the shim (with Range), auth failures, missing/unknown id; HEAD and `download` through the catch-all; `getCoverArt` local, sized, no art, missing id, `octo-radio`, registry ids with and without `c=Octo`, legacy ids, `pl-`, station id; `getTranscodeDecision`; `radio/stream` token routing |
| `05-playlists` | 19 | 3.5 read side: `getPlaylists`/`getInternetRadioStations` merges, jsonp, wrong password, Octo-id read-only refusals |
| `06-lyrics` | 24 | 3.7: `getLyricsBySongId` (sidecar, LRCLIB, none, external, enhanced), `getLyrics` (relay, live fill-in), `getLyricsCandidates`, `setLyricsChoice` |
| `07-octo-extensions` | 15 | 3.8: `getAcquisitions`, `getLibraryActions`, `getUpgrades`, `libraryAction` |
| `08-catchall-native` | 34 | 3.9 and 2.2: `GET /`, Octo-owned 404s, external-id safety net and element naming, unknown endpoints, DELETE on a Subsonic route, POST bodies through the faithful relay, native `/api` with JWT (lists, Octo's external song/album/artist answers, discovery appends, radio playlist ids) |
| `09-admin` | 97 | Section 4 and 2.9: `/admin` redirect, every safe admin GET, the guard (OPTIONS, preflight, writes without `X-Octo-Admin`, CORS stripping), every browse-session endpoint without and with a session (cookie, header, stale cookie hiding a header), browse auth failures and success, validation 400s, side-effect-free POSTs |
| `10-static-cors` | 25 | Sections 5 and 7: static files plain, HEAD, `If-None-Match`, Range (also unsatisfiable), Brotli/gzip, upper-case path, `/Assets`; CORS on simple requests, preflights, errors and admin 403s; `X-Forwarded-Proto` |
| `90-mutations` | 58 | Run last: star/unstar/rating/scrobble/now playing/reportPlayback, a playlist create-update-delete round trip, a settings patch and re-read, the side-effect admin POSTs (caches, notifications, update check, Last.fm connect flow, job cancel/resume/undo, lyrics choices), sign-out, and a heart on an external song |
| **Total** | **453** | |

## Recordings

`record` writes one `<corpus-file>.json` per corpus file plus `meta.json` (target, image label,
captured variables). Each entry keeps the request (method, target, headers, body), the status,
**all response headers in wire order** (repeats included), and the body: text bodies up to
64 KB verbatim, larger ones and binary bodies (audio, images, compressed static files) as
size + SHA-256. Raw values are stored; normalisation happens only at diff time.

## Normalisation

Applied to both sides before comparing:

| What | How |
|---|---|
| `Date`, `Connection`, `Keep-Alive` headers | dropped |
| `Content-Length`, `Transfer-Encoding` | dropped in structural mode; compared in bytes mode unless the body length moved with normalised values |
| The target's `host:port` | `{{host}}` (it appears in Octo's messages and stream URLs) |
| Volatile captures (song/user/playlist ids, JWT, session cookie, salts) | `{{var}}`, raw and percent-encoded |
| ProblemDetails `traceId` | `00-TRACE-00` |
| JWTs anywhere | `JWT` |
| ISO timestamps dated 2025 or later (made by the run; fixture dates are 2018-2021) | `TS(<sep><.f if it had a fraction><zone form>)`, keeping the shape but not the digit count (Go trims trailing zeros, so the count varies) |
| HTTP dates dated 2025 or later | `HTTP-DATE` |
| `Last-Modified` on static files (build time) | `BUILD-TIME` ([`normalize.json`](normalize.json)) |
| `roles` arrays / `<roles>` runs (Navidrome builds them from Go maps) | sorted |
| Requests marked `unordered` | arrays (JSON) and child elements (XML) sorted, recursively |
| `ignoreKeys` (per request, or global in `normalize.json`) | members/attributes removed |

More rules go in `normalize.json` (`rules`: `requests` glob, `where` = `body` or
`header:<lower-case name>`, `regex`, `replace`).

**Structural mode** (default) parses JSON and XML and compares values: key and attribute order,
whitespace between elements and number spelling (`1` vs `1.0`) are not compared; headers are
compared as a sorted multiset. **Bytes mode** compares the normalised body text exactly
(escaping, key order, XML indentation and declaration, attribute order) and the headers in
wire order, so it catches what a client parsing strictly would see. Requests marked
`unordered` are compared structurally in both modes. Binary bodies are compared by hash in
both.

## Allowlist

[`allowlist.json`](allowlist.json) explains diffs that are deliberate. Each entry must quote
text that appears in `docs/rust-migration/known-diffs.md`, or `diff` refuses to run:

```json
[
  {"id": "server-header",
   "requests": ["*"],
   "aspects": ["header:server"],
   "knownDiff": "Server header",
   "reason": "axum sends no Server header; Kestrel sent `Server: Kestrel`."}
]
```

`requests` and `aspects` are globs. Aspects are `status`, `header:<lower-case name>`,
`header-order`, `body` and `transport`. A request whose every differing aspect is allowed
counts as explained; the report lists how often each entry was used.

## Facts the baseline settles

From `recordings/csharp/` (these close several **(verify)** items in `endpoints.md`):

- `GET /api/admin/lyrics/library` answers `400` with `{"subsonic-response":{...,"error":{"code":10,"message":"Operation not valid"}}}` on every call: the `busy`/`Busy` name collision is real (endpoints.md 4.9, 6.6).
- `getSimilarSongs*` with radio off relays to `{Url}//rest/...`; Navidrome answers the double slash with its web UI (`200 text/html`), which Octo passes through (3.2, 6.5).
- `radio/stream/<48 chars>` with an unknown token is a bare `404` (GET and HEAD); a shorter token is relayed to Navidrome (3.4).
- `GET /` is the `400` ProblemDetails for `endpoint`; Octo-owned paths (`/favicon.ico`, `/administrator`, `/assets/...`, `/api/admin/<unknown>`) are ProblemDetails `404`s; `HEAD /api/admin/settings` is a `404` (2.2).
- `POST /api/admin/browse/auth` with an empty JSON body is a `400` ValidationProblemDetails; a bad bool in `covers/upgrade/thumb?found=` likewise (2.8).
- The local `stream` relay keeps Navidrome's `206` and `Content-Range` (6.4).
- `X-Octo-Browse-Token` alone does not sign `browse/session` in (cookie only), and a stale `octo_browse` cookie hides a valid header (2.9).

## Not covered

The harness is only as good as its corpus. Known gaps, roughly by risk:

- **Acquisition pipeline.** Hearting an external song is the last request, so the background
  Soulseek search, download, tagging, verification (MusicBrainz, AcoustID, Cover Art Archive,
  fpcalc), file placement and the resulting acquisition rows are not observed. slskd always
  returns zero search responses. Library actions, quarantine, the upgrade queue and the review
  sweep run only to their "off" or refusal answers.
- **Last.fm radio, stations, mixes and `radio/stream` playback** (radio and generated playlists
  are off, because their background builds run on timers and their timing would leak into
  `getPlaylists`/`getInternetRadioStations`). Station and mix covers, ICY metadata, Last.fm and
  ListenBrainz scrobbling, Now Playing submission.
- **Settings edge cases:** hot reload of `settings.json`, a corrupt file (`409`), `PUT
  raw-config` success, secret placeholders beyond the admin password, env-var parsing quirks
  (`config.md`). One settings patch is covered.
- **Process-level:** `POST /api/admin/restart`, the update helper handshake (`POST
  /api/admin/update` success), first-run LAN discovery and `GET /api/admin/discover-servers`
  (it scans the network; the sandbox's addresses change per run).
- **Feature switches left at one value:** `WaitForLosslessOnPlay`, `DownloadOnPlay`,
  `StorageMode=Cache`, `EnableExternalPlaylists` (so `pl-` ids only hit their not-found paths),
  notification sinks (none configured), Genre normalisation.
- **apiKey auth:** Navidrome 0.64.2 does not implement it, so only its failure paths and
  `tokenInfo`'s 404 are recorded.
- **Images:** binary bodies are compared by SHA-256, so the Rust cover renderer (badged
  covers, the placeholder) will differ from ImageSharp's bytes even when it looks the same.
  PLAN.md's SSIM ≥ 0.98 tolerance is not implemented here; allowlist those diffs against a
  known-diffs row until a perceptual comparator is added.
- **Timing:** `tags/preview` `stageSeconds` and Now Playing `positionMs`/`minutesAgo` are ignored;
  fractional-second digit counts of run-time timestamps are not compared; bodies over 64 KB are
  compared by hash only (the admin UI files, `/Assets` images).
- **Transport:** keep-alive behaviour, chunk boundaries, HTTP/2, request bodies sent as
  `multipart/form-data`, very large uploads, client disconnects.
