#!/usr/bin/env bash
#
# Generate a self-signed dev PKI shared by this agent and a podium control-plane:
#
#   ca.crt / ca.key                 one shared Certificate Authority
#   podium.crt / podium.key         podium server cert   (agent  -> podium, discovery)
#   agent.crt  / agent.key          agent server cert    (podium -> agent,  TaskService)
#   agent-client.crt  / .key        agent client cert    (agent  -> podium, mTLS)
#   podium-client.crt / .key        podium client cert   (podium -> agent,  mTLS)
#
# All leaf certs are signed by the single CA, so each side trusts the other via ca.crt.
# Dev only — do NOT ship these.
#
# Requires: openssl (1.1.1+ for -addext / process substitution).

set -euo pipefail
cd "$(dirname "$0")"

DAYS=825
SAN="DNS:localhost,IP:127.0.0.1,IP:::1"

echo "==> CA"
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout ca.key -out ca.crt -days "$DAYS" -subj "/CN=solti-dev-ca"

gen() {
  local name="$1" eku="$2"
  echo "==> ${name} (${eku})"
  openssl req -newkey rsa:2048 -nodes -keyout "${name}.key" -out "${name}.csr" -subj "/CN=${name}"
  openssl x509 -req -in "${name}.csr" -CA ca.crt -CAkey ca.key -CAcreateserial \
    -days "$DAYS" -out "${name}.crt" \
    -extfile <(printf "subjectAltName=%s\nextendedKeyUsage=%s\n" "$SAN" "$eku")
  rm -f "${name}.csr"
}

gen podium        serverAuth
gen agent         serverAuth
gen agent-client  clientAuth
gen podium-client clientAuth

rm -f ca.srl
echo
echo "done:"
echo "  shared CA     : ca.crt (+ ca.key)"
echo "  agent  server : agent.crt / agent.key"
echo "  agent  client : agent-client.crt / agent-client.key"
echo "  podium server : podium.crt / podium.key"
echo "  podium client : podium-client.crt / podium-client.key"
