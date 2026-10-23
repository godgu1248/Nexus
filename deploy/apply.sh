#!/bin/sh
# Server-side installer for the Nexus bundle. Run as root from the unpacked bundle:
#   tar -xzf nexus-linux-x86_64.tar.gz && cd nexus-bundle && sh apply.sh
# Idempotent: re-running upgrades the binary/static/unit/nginx conf and restarts the service.
set -e

BUNDLE_DIR=$(cd "$(dirname "$0")" && pwd)
APP_DIR=/opt/nexus
ENV_FILE=/etc/nexus/nexus.env
UNIT=/etc/systemd/system/nexus.service
NGINX_CONF=/etc/nginx/conf.d/nexus.conf
STAMP=$(date +%Y%m%d-%H%M%S)

echo "== 1/6 layout"
id nexus >/dev/null 2>&1 || useradd --system --create-home --home-dir "$APP_DIR" --shell /usr/sbin/nologin nexus
mkdir -p "$APP_DIR/static" /etc/nexus/certs /var/log/nexus
chown -R nexus:nexus "$APP_DIR" /var/log/nexus
chmod 700 /etc/nexus/certs

echo "== 2/6 binary"
install -o nexus -g nexus -m 0755 "$BUNDLE_DIR/nexus" "$APP_DIR/nexus.new"
mv -f "$APP_DIR/nexus.new" "$APP_DIR/nexus"

echo "== 3/6 static"
cp -a "$BUNDLE_DIR/static/." "$APP_DIR/static/"
chown -R nexus:nexus "$APP_DIR/static"

echo "== 4/6 systemd unit"
cp "$BUNDLE_DIR/deploy/nexus.service" "$UNIT"
systemctl daemon-reload

echo "== 5/6 nginx conf"
if [ -f "$NGINX_CONF" ]; then cp "$NGINX_CONF" "$NGINX_CONF.bak-$STAMP"; fi
cp "$BUNDLE_DIR/deploy/nginx.conf" "$NGINX_CONF"
nginx -t
systemctl enable --now nginx
systemctl reload nginx

echo "== 6/6 start and verify"
if [ ! -f "$ENV_FILE" ]; then
  echo "!! $ENV_FILE is missing; copy deploy/nexus.env.example and fill it in, then re-run." >&2
  exit 3
fi
# wait for MySQL (the container may still be starting after a reboot)
i=0
while [ $i -lt 60 ]; do
  if (echo > /dev/tcp/127.0.0.1/13306) 2>/dev/null; then echo "db port up"; break; fi
  i=$((i+1)); sleep 3
done
systemctl enable nexus
systemctl restart nexus
sleep 4
systemctl is-active nexus
curl -sS --max-time 5 http://127.0.0.1:8080/healthz || true
echo
journalctl -u nexus -n 15 --no-pager || true
echo "bundle applied; verify from outside with deploy/smoke.ps1"
