#!/usr/bin/env bash
set -euo pipefail

outdir="${1:-tests/fixtures/tls}"

mkdir -p "${outdir}"

cat <<'EOF' > "${outdir}/cert.ext"
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost
EOF

openssl req \
  -x509 \
  -newkey rsa:2048 \
  -nodes \
  -keyout "${outdir}/ca-key.pem" \
  -out "${outdir}/ca-cert.pem" \
  -subj /CN=rsloop-test-ca \
  -days 1 \
  >/dev/null 2>&1

openssl req \
  -newkey rsa:2048 \
  -nodes \
  -keyout "${outdir}/key.pem" \
  -out "${outdir}/cert.csr" \
  -subj /CN=localhost \
  >/dev/null 2>&1

openssl x509 \
  -req \
  -in "${outdir}/cert.csr" \
  -CA "${outdir}/ca-cert.pem" \
  -CAkey "${outdir}/ca-key.pem" \
  -CAcreateserial \
  -out "${outdir}/cert.pem" \
  -days 1 \
  -extfile "${outdir}/cert.ext" \
  >/dev/null 2>&1

rm -f "${outdir}/cert.csr" "${outdir}/cert.ext" "${outdir}/ca-cert.srl"

echo "Generated TLS test certs in ${outdir}"
