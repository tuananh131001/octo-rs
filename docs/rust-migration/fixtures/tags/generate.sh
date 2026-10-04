#!/usr/bin/env bash
# Regenerates the tag fixtures for the Rust tag port (task 3-D).
#
#   input/   tiny untagged files made by ffmpeg (0.25 s of a 440 Hz tone) and an 8x8 PNG cover.
#            Made only when missing: the C# dumps hash them, so keep them unless you regenerate all.
#   csharp/  each scenario's file as the C# code (TagLibSharp 2.3.0) wrote it, its dump
#            (<file>.dump), and TagLib's genre table (genres.txt).
#
# The C# side runs in the .NET 9 SDK image with no LANG, like the shipped image. Set
# NUGET_PACKAGES to a host package cache to skip the download. The Rust side is
# crates/octo-media/src/tags/fixture_tests.rs.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../../.." && pwd)"
cd "$here/input"

tone=(-y -nostdin -hide_banner -v error -f lavfi -i sine=frequency=440:duration=0.25 -ac 1
      -map_metadata -1 -fflags +bitexact -flags:a +bitexact)
[[ -f input.mp3 ]] || ffmpeg "${tone[@]}" -ar 44100 -c:a libmp3lame -b:a 32k -id3v2_version 0 -write_xing 0 input.mp3
[[ -f input.flac ]] || ffmpeg "${tone[@]}" -ar 8000 -c:a flac -sample_fmt s16 input.flac
[[ -f input.m4a ]] || ffmpeg "${tone[@]}" -ar 44100 -c:a aac -b:a 32k input.m4a
[[ -f input.opus ]] || ffmpeg "${tone[@]}" -ar 48000 -c:a libopus -b:a 16k input.opus
[[ -f cover.png ]] || ffmpeg -y -nostdin -hide_banner -v error -f lavfi -i color=c=0x3366cc:s=8x8 -frames:v 1 cover.png

nuget=()
if [[ -n "${NUGET_PACKAGES:-}" ]]; then nuget=(-e NUGET_PACKAGES=/nuget -v "$NUGET_PACKAGES":/nuget); fi
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e DOTNET_CLI_HOME=/tmp -e DOTNET_NOLOGO=1 \
  "${nuget[@]}" -v "$repo":/repo -w /repo/docs/rust-migration/fixtures/tags/generator \
  mcr.microsoft.com/dotnet/sdk:9.0 dotnet run -- /repo/docs/rust-migration/fixtures/tags
rm -rf "$here/generator/bin" "$here/generator/obj"
