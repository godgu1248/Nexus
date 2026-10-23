#!/bin/sh
# 开启图片上传所需的三件事：目录、环境变量、systemd 写权限，外加 nginx 的 /uploads 直出。
# 幂等：重复执行不会重复追加。
set -eu

APP_DIR=/opt/nexus
UPLOAD_DIR="$APP_DIR/uploads"
ENV_FILE=/etc/nexus/nexus.env
UNIT=/etc/systemd/system/nexus.service
NGINX_CONF=/etc/nginx/conf.d/nexus.conf

echo "== 1. 上传目录 =="
mkdir -p "$UPLOAD_DIR"
chown nexus:nexus "$UPLOAD_DIR"
chmod 755 "$UPLOAD_DIR"
ls -ld "$UPLOAD_DIR"

echo "== 2. 环境变量 =="
add_env() {
  key=$1
  value=$2
  if grep -q "^${key}=" "$ENV_FILE"; then
    echo "  已有 $key"
  else
    printf '%s=%s\n' "$key" "$value" >> "$ENV_FILE"
    echo "  追加 $key=$value"
  fi
}
add_env UPLOAD_DIR "$UPLOAD_DIR"
add_env UPLOAD_BASE_URL "/uploads"

echo "== 3. systemd 写权限 =="
if grep -q "ReadWritePaths=.*${UPLOAD_DIR}" "$UNIT"; then
  echo "  已包含 $UPLOAD_DIR"
else
  if grep -q '^ReadWritePaths=' "$UNIT"; then
    sed -i "s#^ReadWritePaths=\(.*\)#ReadWritePaths=\1 ${UPLOAD_DIR}#" "$UNIT"
  else
    sed -i "/^\[Service\]/a ReadWritePaths=${UPLOAD_DIR}" "$UNIT"
  fi
  systemctl daemon-reload
  echo "  已加入 $UPLOAD_DIR"
fi
grep '^ReadWritePaths=' "$UNIT" || true

echo "== 4. nginx /uploads 直出 =="
if [ -f /tmp/nexus.conf ]; then
  if grep -q 'location /uploads/' "$NGINX_CONF"; then
    echo "  nginx 配置已含 /uploads，跳过"
  else
    cp -a "$NGINX_CONF" "${NGINX_CONF}.bak.$(date +%Y%m%d%H%M%S)"
    cp /tmp/nexus.conf "$NGINX_CONF"
    echo "  已替换 nexus.conf（原文件已备份）"
  fi
else
  echo "  /tmp/nexus.conf 不存在，跳过 nginx 修改"
fi

nginx -t
systemctl reload nginx
echo "== done =="
