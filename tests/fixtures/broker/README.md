Test-only TLS material for `broker::upstream`'s tests: a CA and a leaf for
`localhost` / `127.0.0.1` it signed, valid for 100 years. Never trusted outside
those tests. Regenerate with:

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout /tmp/ca.key -out ca.pem -days 36500 -subj "/CN=devsandbox test CA" \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout localhost.key -out /tmp/leaf.csr -subj "/CN=localhost"
openssl x509 -req -in /tmp/leaf.csr -CA ca.pem -CAkey /tmp/ca.key -days 36500 \
  -out localhost.pem -extfile <(printf '%s\n' \
  'subjectAltName=DNS:localhost,IP:127.0.0.1' 'basicConstraints=critical,CA:FALSE' \
  'keyUsage=critical,digitalSignature' 'extendedKeyUsage=serverAuth')
```
