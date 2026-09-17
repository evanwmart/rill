#!/usr/bin/env bash
# A self-signed certificate for the endpoint (specs/security.md §2–§3):
# there is no CA in Rill — a viewer trusts this endpoint by pinning the
# SHA-256 fingerprint of exactly this certificate (`rill auth trust`).
# P-256 ECDSA, which every TLS 1.3 stack (and rustls on the viewer) accepts.
set -euo pipefail
cd "$(dirname "$0")"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -keyout key.pem -out cert.pem -days 3650 -subj "/CN=rill-foreign-endpoint" 2>/dev/null
echo "cert.pem + key.pem written. Fingerprint the viewer will pin:"
openssl x509 -in cert.pem -outform DER | sha256sum | cut -d' ' -f1
