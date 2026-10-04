# Parity status (prep for 6-C)

The first run of the parity corpus ([`parity/README.md`](../../parity/README.md)) against the Rust
image, taken on 2026-10-04 at `rust-rewrite` `0192536` (4-A merged). The controllers (6-A, 6-B)
are not ported yet, so only the catch-all relay, static files, the `/admin` redirect, the admin
guard, CORS and the error mapping are live HTTP routes. Every other Subsonic or admin route
reaches the catch-all and is relayed to Navidrome as is.

```sh
docker build -f Dockerfile.rust -t octo-rust:dev .
python3 parity/parity.py run --image octo-rust:dev --project parity-rust --port 18580 --nd-port 18553 \
    --out <scratch>/parity/rust
python3 parity/parity.py diff               parity/recordings/csharp <scratch>/parity/rust
python3 parity/parity.py diff --mode bytes  parity/recordings/csharp <scratch>/parity/rust
```

The recording itself is not checked in, since it only describes a half-ported build.

## Summary

- **Ported areas: 96 of 96 requests match** in structural mode and in bytes mode. The only
  differences are four that `known-diffs.md` already lists, and those are now in
  [`parity/allowlist.json`](../../parity/allowlist.json) (see below). No unexplained diff
  remains in the catch-all relay, static files, CORS, the admin guard, error envelopes or the
  404/400 problem documents.
- **Not yet ported: 357 requests.** C# answered these from a controller action, or from steps
  2-10 of `GenericEndpoint`, which are marked `TODO(6-A)` in `catch_all.rs`. Of these, 1 matches
  anyway, 86 were skipped because the variables they need are captured from responses only
  controllers give (the external ids from discovery `search3`, the browse-session token), and
  270 differ, as expected.
- Official `diff` output with the allowlist: 453 compared, 97 explained (the 96 ported requests
  plus the one that matches anyway), 356 unexplained. The 356 are all not-yet-ported requests.
  Both modes give the same counts.

A request counts as **ported** when, in C#, no controller action answered it: the request
reached the catch-all's step 1 or step 11, a static file, or `AdminRoot`, or middleware answered
it before routing (the guard's 403 and its OPTIONS answer, the CORS preflight). That gives 66
catch-all requests (56 relays, 10 problem documents), 17 static files, 8 guard answers, 3 CORS
preflights and 2 `/admin` redirects. The route table
came from the `[Route]`/`[Http*]` attributes in `octo/Controllers/*.cs` (150 templates). The
catch-all's steps 2-10 count as not ported: native radio playlists, the external-id safety net,
native external song, album and artist answers, and the search appends.

## Per corpus group

| Group | Requests | Ported area | Matched | Differing | Not yet ported | of which skipped (no capture) | Kinds of not-ported diff |
|---|---:|---:|---:|---:|---:|---:|---|
| `00-setup` | 6 | 2 | 2 | 0 | 4 | 0 | body 4 (`getAlbum` merge fields, discovery `search3`) |
| `01-system` | 43 | 13 | 13 | 0 | 30 | 0 | `Vary` only 21, body 6, status 3 |
| `02-browsing` | 65 | 19 | 19 | 0 | 46 | 11 | `Vary` only 16, body 19 |
| `03-search` | 25 | 0 | 0 | 0 | 25 | 0 | body 19, `Vary` only 6 |
| `04-media` | 42 | 4 | 4 | 0 | 38 | 12 | headers only 14, status 8, body 4 |
| `05-playlists` | 19 | 0 | 0 | 0 | 19 | 0 | headers only 14, body 5 |
| `06-lyrics` | 24 | 0 | 0 | 0 | 24 | 2 | headers only 12, status 8, body 2 |
| `07-octo-extensions` | 15 | 0 | 0 | 0 | 15 | 0 | status 15 (Navidrome's 404/`failed` for Octo-only endpoints) |
| `08-catchall-native` | 34 | 21 | 21 | 0 | 13 | 7 | status 2, body 2, headers 1; 1 matches anyway (`native-album-by-name-discovery`) |
| `09-admin` | 97 | 10 | 10 | 0 | 87 | 32 | status 55 (relayed to Navidrome instead of answered) |
| `10-static-cors` | 25 | 23 | 23 | 0 | 2 | 0 | `Vary` only 2 (`ping`, `getInternetRadioStations` with `Origin`) |
| `90-mutations` | 58 | 4 | 4 | 0 | 54 | 22 | headers only 17, status 14, body 1 |
| **Total** | **453** | **96** | **96** | **0** | **357** | **86** | |

"Headers only" in the not-ported column is almost always `Vary: Origin` (97 of 103). C#'s
controller actions answered with their own headers. In Rust these requests go through the
faithful relay, which passes Navidrome's `Vary: Origin` on. The others are the
`Cache-Control`/`ETag`/`Last-Modified` of a relayed `getCoverArt`, and `X-Nd-Authorization` on
a relayed native call. All of these go away once the controllers answer these requests again.

## Diffs in the ported areas

None are unexplained. These four are deliberate, are listed in `known-diffs.md`, and are now
allowlisted. Each is shown with the bytes on both sides.

| Allowlist id | Requests | C# (`octo-csharp:csharp-final`) | Rust (`octo-rust:dev`) | Where in Rust |
|---|---|---|---|---|
| `server-header` | all 367 answered | `Server: Kestrel` | no `Server` header | axum/hyper add none (known-diffs "HTTP" row) |
| `body-framing` | bytes mode, 16 in the ported area (every ProblemDetails 404/400 and the guard's 403) | `Transfer-Encoding: chunked` | `content-length: 162` (problem 404), `250` (validation 400), `111` (guard 403) | `crates/octo/src/http/error.rs:163` and `:176` build full bodies (known-diffs "HTTP" row) |
| `static-accept-ranges-once` | 16 static-file answers | `Accept-Ranges: bytes` twice | `accept-ranges: bytes` once | `crates/octo/src/http/static_files.rs:625` (known-diffs "Static files" row) |
| `static-compressed-variants` | `index-html-br`, `index-html-gzip`, `admin-js-br` | br `index.html`: 24331 bytes, `ETag: "Q+bZYXr9WyAcKmn0lUA2qcpF1/RUaUl8RXOX1Wvxngk="` | br `index.html`: 24327 bytes, `etag: "bsAdnpFCcBZ8xtRHzg7c3va2ZykEXuVquMfhTrnZGg0="`. The second, `W/"poA4o6…YBo="`, is identical | `static_files.rs:152-153` and `:348-367` compress at startup (known-diffs "Static files" row) |

The following answers are byte-identical apart from the volatile `traceId` and the
`Date`/`Server`/framing headers:

- **Problem 404** (`/favicon.ico`, `/admin/nope.js`, `/administrator`, `/assets/missing.png`,
  `/api/admin/does-not-exist`, `HEAD /api/admin/settings`, `POST /api/admin/nothing-here` with
  the header, `/admin/octo_logo.png`):
  `{"type":"https://tools.ietf.org/html/rfc9110#section-15.5.5","title":"Not Found","status":404,"traceId":"00-…-…-00"}`
  with `application/problem+json; charset=utf-8`.
- **Validation 400** for `GET /`: the same key order and the same `errors.endpoint` message.
- **Guard 403**: `{"error":"Admin changes must come from Octo's dashboard. A script can send the X-Octo-Admin header to opt in."}`
  with `application/json; charset=utf-8`. The apostrophe is unescaped, as with C#'s relaxed
  encoder. The guard's OPTIONS answer is a bare 204, and every `Access-Control-*` header is
  stripped on `/api/admin*`.
- **CORS**: the preflight 204s on `/rest`, `/api` and static paths, and
  `Access-Control-Allow-Origin`/`-Expose-Headers` on simple requests and on errors.
- **Static files**: plain, HEAD, `If-None-Match` 304, `Range` 206 and 416, upper-case paths,
  and `/Assets`. The ETags of the plain files are identical.
- **Faithful relay**: 56 Subsonic and native relays, including PUT/HEAD/OPTIONS falling through,
  POST bodies, `auth/login`, the native `/api` lists, `If-None-Match`, `download` with a Range,
  and the short `radio/stream` token. Status, the allowlisted headers (`Content-Type`,
  `Last-Modified`, `Accept-Ranges`, `Vary`, ...) and bodies are all the same.

### Harness fix made in this run

The first run showed `Last-Modified` diffs on three relayed `download`/`HEAD stream` answers.
The C# side had `Wed, 01 Jan 2020 20:04:05 GMT` and the Rust side had the checkout time. Git
does not keep mtimes, and a fresh worktree gives the fixture library the checkout time, which
Navidrome passes on. `parity.py up` now sets the fixture files and folders back to the
baseline's instant before it starts the stack. With that fix, the three requests match.

## Not yet ported, by cause

| Cause (C# handler) | Requests | Lands in |
|---|---:|---|
| `SubSonicController` actions (`ping`, `getSong`/`getAlbum`/`getArtist` merges, `search2/3`, `stream`, `getCoverArt`, playlists, lyrics, Octo extensions, `star`/`setRating`/`scrobble`, ...) | 223 | 6-A |
| `GenericEndpoint` steps 2-10 (native radio playlists, external-id safety net, native external song, album and artist, search appends) | 17 | 6-A |
| `AdminController` (85), `LyricsAdminController` (19), `CoverUpgradeController` (10), `UpdateController` (3): GETs, and writes that carry `X-Octo-Admin` | 117 | 6-B |

Re-run the corpus as each of these lands. The 86 skipped requests come back once 6-A answers
`search3` with discovery results (for `ext_song`, `ext_album` and `ext_artist`), and once 6-B
answers `browse/auth` (for `browse_token`).

## Found outside the corpus

1. **First-run auto-detect can adopt Octo itself.** This needs fixing before cutover. With no
   `SUBSONIC_URL`, `first_run` (`crates/octo/src/host.rs:181-191`) scans the /24 for Subsonic
   servers. The Rust server is already listening by the time the scan probes its own address on
   port 8080. Its catch-all answers `rest/ping.view` with a `subsonic-response` error envelope,
   which `probe` (`crates/octo/src/services/subsonic/subsonic_discovery_service.rs:105-110`)
   counts as a server. In a lone container this produced `found 1 Subsonic server(s)` →
   `auto-configured Navidrome URL -> http://172.17.0.2:8080` → restart, so Octo would then relay
   to itself. The C# code has the same race, but .NET starts slowly enough that its probe of its
   own port is refused (`found 0`). The check is the same after 6-A, because the ported `ping`
   also answers with an envelope. Suggested fix, recorded as a known diff: skip candidates whose
   address is one of the host's own interface addresses on the bind port.
2. **Startup compression costs about 0.8 s and about 23 MiB.** `StaticAssets::load`
   (`host.rs:110`) runs Brotli quality 11 and gzip-best over the four admin files (530 KB)
   before the listener binds (`static_files.rs:348-367`). With `OCTO_WWWROOT=/nonexistent`, the
   first answer comes at 0.38 s instead of 1.10 s after `docker run`, and idle RSS is 8.4 MiB
   instead of 31.5 MiB. Options are to precompress at image build time (as `MapStaticAssets` did
   at publish), to bind first and fill the variants in the background, or to drop to Brotli
   quality 9 or 10. This matters for Key Result 6 (cold start ≤ 1 s).

## Image and runtime (same machine, idle, no upstreams reachable)

| | `octo-csharp:csharp-final` | `octo-rust:dev` |
|---|---|---|
| Image size | 1.13 GB (1,126,336,765 B) | 971 MB (970,947,478 B); the binary is 14.4 MB, the rest is the same apt set (ffmpeg, fonts) |
| Cold build | n/a | 345 s with an empty cache, `CARGO_BUILD_JOBS=2`: cargo-chef install 70 s, dependency cook 159 s, `octo` 86 s; the runtime apt layer ran in parallel. Pulling `rust:1.99-bookworm` took another ~27 s |
| `docker run` → first HTTP answer | 0.79-0.93 s | 1.10-1.18 s (0.38 s without the startup compression, see above) |
| Idle RSS (`docker stats`, 10 and 30 s after ready) | 204-209 MiB | 31-32 MiB (15%) |
| `docker stop` | 0.19-0.28 s, graceful | 0.17-0.22 s, graceful (`Application is shutting down...`, PID 1 handles SIGTERM) |

Measured with `Subsonic__Url=http://127.0.0.1:4533` (refused) so that first-run discovery
does not run, and `Updates__Check=false`.
