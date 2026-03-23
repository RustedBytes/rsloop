#!/usr/bin/env bash
set -euo pipefail

outdir="${1:-tests/fixtures/tls}"

mkdir -p "${outdir}"

rm -f \
  "${outdir}/ca-cert.pem" \
  "${outdir}/ca-key.pem" \
  "${outdir}/cert.pem" \
  "${outdir}/key.pem" \
  "${outdir}/cert.csr" \
  "${outdir}/cert.cnf" \
  "${outdir}/ca.cnf" \
  "${outdir}/ca-cert.srl"

cat <<'EOF' > "${outdir}/ca.cnf"
[req]
distinguished_name = dn
x509_extensions = v3_ca
prompt = no

[dn]
CN = rsloop-test-ca

[v3_ca]
basicConstraints = critical, CA:true
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer
EOF

cleanup() {
  rm -f \
    "${outdir}/cert.csr" \
    "${outdir}/cert.cnf" \
    "${outdir}/ca.cnf" \
    "${outdir}/ca-cert.srl"
}

trap cleanup EXIT

cat <<'EOF' > "${outdir}/cert.cnf"
[req]
distinguished_name = dn
req_extensions = v3_req
prompt = no

[dn]
CN = localhost

[v3_req]
subjectAltName = @alt_names
basicConstraints = CA:false
keyUsage = critical, digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth

[alt_names]
DNS.1 = localhost
IP.1 = 127.0.0.1
EOF

openssl req \
  -x509 \
  -newkey rsa:2048 \
  -sha256 \
  -nodes \
  -keyout "${outdir}/ca-key.pem" \
  -out "${outdir}/ca-cert.pem" \
  -days 3650 \
  -config "${outdir}/ca.cnf" \
  >/dev/null 2>&1

openssl req \
  -newkey rsa:2048 \
  -sha256 \
  -nodes \
  -keyout "${outdir}/key.pem" \
  -out "${outdir}/cert.csr" \
  -config "${outdir}/cert.cnf" \
  >/dev/null 2>&1

openssl x509 \
  -req \
  -in "${outdir}/cert.csr" \
  -CA "${outdir}/ca-cert.pem" \
  -CAkey "${outdir}/ca-key.pem" \
  -CAcreateserial \
  -out "${outdir}/cert.pem" \
  -days 3650 \
  -sha256 \
  -extfile "${outdir}/cert.cnf" \
  -extensions v3_req \
  >/dev/null 2>&1

echo "Generated TLS test certs in ${outdir}"
