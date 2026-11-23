#!/usr/bin/env bash
set -euo pipefail

out="${1:?usage: prepare.sh <output dir> [directory of the files as RabbitMQ sees it] [plain port] [TLS port]}"
seen="${2:-$out}"
plain="${3:-5672}"
tls="${4:-5671}"

mkdir -p "$out"
cd "$out"

openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=FastMock test CA" \
  -keyout ca.key -out ca.pem 2>/dev/null

openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 \
  -extfile server.ext -out server.pem 2>/dev/null

openssl req -newkey rsa:2048 -nodes -subj "/CN=fastmock-capture" -keyout client.key -out client.csr 2>/dev/null
printf 'extendedKeyUsage=clientAuth\n' > client.ext
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 \
  -extfile client.ext -out client.pem 2>/dev/null

openssl req -newkey rsa:2048 -nodes -subj "/CN=fastmock-unknown" -keyout unknown.key -out unknown.csr 2>/dev/null
openssl x509 -req -in unknown.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 \
  -extfile client.ext -out unknown.pem 2>/dev/null

rm -f ca.key ./*.csr ./*.ext ca.srl
chmod 644 ./*.pem ./*.key

cat > rabbitmq.conf <<EOF
listeners.tcp.default = $plain
listeners.ssl.default = $tls
ssl_options.cacertfile = $seen/ca.pem
ssl_options.certfile = $seen/server.pem
ssl_options.keyfile = $seen/server.key
ssl_options.verify = verify_peer
ssl_options.fail_if_no_peer_cert = false
auth_mechanisms.1 = PLAIN
auth_mechanisms.2 = AMQPLAIN
auth_mechanisms.3 = EXTERNAL
ssl_cert_login_from = common_name
default_user = fastmock
default_pass = fastmock
loopback_users = none
EOF

printf '[rabbitmq_auth_mechanism_ssl].\n' > enabled_plugins
