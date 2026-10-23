#!/bin/sh
# 安装带 /uploads 直出的 nginx 配置，并校验 + 平滑重载。
set -eu

NGINX_CONF=/etc/nginx/conf.d/nexus.conf

echo "== 现状 =="
ls -l /tmp/nexus.conf 2>/dev/null || echo "  没有 /tmp/nexus.conf"
grep -c "location /uploads/" /tmp/nexus.conf 2>/dev/null || echo "  上传的配置里没有 /uploads"

echo "== 安装 =="
cp -a "$NGINX_CONF" "${NGINX_CONF}.bak"
cp /tmp/nexus.conf "$NGINX_CONF"
grep -n "location /" "$NGINX_CONF" || true

echo "== 校验并重载 =="
nginx -t
systemctl reload nginx
systemctl is-active nginx
