#!/usr/bin/env bash
# Generate the parity test CA and the one TLS certificate the stub server presents for
# every hard-coded HTTPS upstream (Deezer, iTunes, MusicBrainz, ...).
#
# TEST ONLY. The private keys are checked in on purpose so runs are reproducible; the CA
# is trusted only inside the parity containers (mounted as their sole root store), never
# on a host.
#
# Usage: parity/certs/make-certs.sh   (rewrites ca.* and stub.* beside this script)
set -euo pipefail
cd "$(dirname "$0")"

# Keep this list in sync with the stubs service's network aliases in docker-compose.yml.
hosts=(
  api.deezer.com
  e-cdns-images.dzcdn.net cdn-images.dzcdn.net cdns-images.dzcdn.net
  itunes.apple.com is1-ssl.mzstatic.com
  musicbrainz.org coverartarchive.org
  api.acoustid.org acoustid.org
  ws.audioscrobbler.com www.last.fm lastfm.freetls.fastly.net
  lrclib.net lyrics.kugou.com mobileservice.kugou.com music.163.com api.lyrics.ovh
  api.github.com github.com
  api.listenbrainz.org ntfy.sh
)

san=""
for h in "${hosts[@]}"; do san+="DNS:$h,"; done
san="${san%,}"

openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -x509 -new -key ca.key -sha256 -days 7300 -subj "/CN=Octo Parity Test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -out ca.crt

openssl ecparam -name prime256v1 -genkey -noout -out stub.key
openssl req -new -key stub.key -subj "/CN=octo-parity-stubs" -out stub.csr
cat > stub.ext <<EOF
basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=$san
EOF
openssl x509 -req -in stub.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 7300 -sha256 \
  -extfile stub.ext -out stub.crt
rm -f stub.csr stub.ext ca.srl
chmod 644 ca.key stub.key   # read by the non-root python user inside the stub container
echo "Wrote ca.crt, ca.key, stub.crt, stub.key"
