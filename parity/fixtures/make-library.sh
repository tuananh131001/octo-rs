#!/usr/bin/env bash
# Generate the parity fixture music library: 10 tiny tagged tracks by 3 artists on
# 3 albums, one album per container format (FLAC, MP3, M4A), with non-ASCII names.
#
# The output is checked in (parity/fixtures/music/) so every machine serves the same
# bytes: encoder output differs between ffmpeg builds, and Navidrome reports sizes and
# bit rates, so regenerating with another ffmpeg changes the recorded baseline. Re-record
# parity/recordings/csharp/ after running this.
#
# Usage: parity/fixtures/make-library.sh   (writes parity/fixtures/music/, replacing it)
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
out="$here/music"
rm -rf "$out"
mkdir -p "$out"

# One fixed instant for every file and folder, so mtimes never vary between machines.
stamp="202001020304.05"

ff() { ffmpeg -nostdin -hide_banner -loglevel error -y "$@"; }

# Bit-exact flags keep encoder version strings and random ids out of the files.
bitexact=(-fflags +bitexact -flags:a +bitexact -flags:v +bitexact -map_metadata -1)

# A 64x64 cover per album, drawn from a solid colour (deterministic JPEG/PNG bytes).
cover() { # colour out-file
  ff -f lavfi -i "color=c=$1:s=64x64:d=1" -frames:v 1 "${bitexact[@]}" "$2"
}

# track <dir> <file> <codec args...> -- <tag args...>
# Audio is a sine whose pitch and length differ per track, so durations differ too.
track() {
  local dir="$1" file="$2" freq="$3" secs="$4"; shift 4
  local codec=() tags=()
  while [[ $# -gt 0 && "$1" != "--" ]]; do codec+=("$1"); shift; done
  shift
  tags=("$@")
  mkdir -p "$dir"
  ff -f lavfi -i "sine=frequency=$freq:duration=$secs:sample_rate=22050" -ac 1 \
     "${bitexact[@]}" "${codec[@]}" "${tags[@]}" "$dir/$file"
}

meta() { # artist album albumartist year genre track title
  printf -- '-metadata\0artist=%s\0-metadata\0album=%s\0-metadata\0album_artist=%s\0-metadata\0date=%s\0-metadata\0genre=%s\0-metadata\0track=%s\0-metadata\0title=%s\0-metadata\0disc=1\0' \
    "$1" "$2" "$3" "$4" "$5" "$6" "$7"
}

with_meta() { # dir file freq secs codec... -- artist album year genre track title
  local dir="$1" file="$2" freq="$3" secs="$4"; shift 4
  local codec=()
  while [[ "$1" != "--" ]]; do codec+=("$1"); shift; done
  shift
  local artist="$1" album="$2" year="$3" genre="$4" no="$5" title="$6"
  local tags=()
  while IFS= read -r -d '' t; do tags+=("$t"); done < <(meta "$artist" "$album" "$artist" "$year" "$genre" "$no" "$title")
  track "$dir" "$file" "$freq" "$secs" "${codec[@]}" -- "${tags[@]}"
}

# --- Album 1: FLAC, folder cover ------------------------------------------------------
a1="$out/Aurora Vale/Northern Lights (2020)"
flac=(-c:a flac -compression_level 5)
with_meta "$a1" "01 - Café del Mar.flac"   440 2 "${flac[@]}" -- "Aurora Vale" "Northern Lights" 2020 Electronic 1 "Café del Mar"
with_meta "$a1" "02 - Polar Night.flac"    494 3 "${flac[@]}" -- "Aurora Vale" "Northern Lights" 2020 Electronic 2 "Polar Night"
with_meta "$a1" "03 - Midnight Sun.flac"   523 2 "${flac[@]}" -- "Aurora Vale" "Northern Lights" 2020 Electronic 3 "Midnight Sun"
with_meta "$a1" "04 - Aurora (Live).flac"  587 4 "${flac[@]}" -- "Aurora Vale" "Northern Lights" 2020 Electronic 4 "Aurora (Live)"
cover "0x2a6f97" "$a1/cover.jpg"
# A sidecar .lrc for track 1, so local lyrics have something to find.
printf '[ar:Aurora Vale]\n[ti:Café del Mar]\n[00:00.00]Sol y mar\n[00:01.00]Café del Mar\n' > "$a1/01 - Café del Mar.lrc"

# --- Album 2: MP3, folder cover, Icelandic names --------------------------------------
a2="$out/Bjørn Ålund/Ágætis Prófun (2018)"
mp3=(-c:a libmp3lame -b:a 32k -id3v2_version 4 -write_xing 0)
with_meta "$a2" "01 - Hjarta.mp3"          392 3 "${mp3[@]}" -- "Bjørn Ålund" "Ágætis Prófun" 2018 "Post-Rock" 1 "Hjarta"
with_meta "$a2" "02 - Ljós í myrkri.mp3"   440 2 "${mp3[@]}" -- "Bjørn Ålund" "Ágætis Prófun" 2018 "Post-Rock" 2 "Ljós í myrkri"
with_meta "$a2" "03 - Þögn.mp3"            349 3 "${mp3[@]}" -- "Bjørn Ålund" "Ágætis Prófun" 2018 "Post-Rock" 3 "Þögn"
cover "0xb5523b" "$a2/folder.jpg"

# --- Album 3: M4A (AAC), Japanese names, no cover file --------------------------------
a3="$out/宇多田テスト/初恋テスト (2021)"
m4a=(-c:a aac -b:a 32k -movflags +faststart)
with_meta "$a3" "01 - First Love.m4a"      330 3 "${m4a[@]}" -- "宇多田テスト" "初恋テスト" 2021 "J-Pop" 1 "First Love"
with_meta "$a3" "02 - 桜の歌.m4a"          370 2 "${m4a[@]}" -- "宇多田テスト" "初恋テスト" 2021 "J-Pop" 2 "桜の歌"
with_meta "$a3" "03 - Ñandú (Remix).m4a"   415 2 "${m4a[@]}" -- "宇多田テスト" "初恋テスト" 2021 "J-Pop" 3 "Ñandú (Remix)"

# Fixed mtimes everywhere (files first, then folders bottom-up).
find "$out" -type f -exec touch -t "$stamp" {} +
find "$out" -depth -type d -exec touch -t "$stamp" {} +

( cd "$out" && find . -type f -print0 | sort -z | xargs -0 sha256sum ) > "$here/music.sha256"
echo "Wrote $(find "$out" -type f | wc -l) files to $out"
