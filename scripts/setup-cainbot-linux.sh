#!/usr/bin/env bash
set -euo pipefail

if [[ "${EUID}" -ne 0 ]]; then
  echo "请用 root 运行这个脚本。"
  exit 1
fi

PROJECT_DIR="${1:-/Cainbot}"
NODE_VERSION="${NODE_VERSION:-v24.14.1}"
NODE_DISTRO="node-${NODE_VERSION}-linux-arm64"
NODE_ARCHIVE="${NODE_DISTRO}.tar.xz"
NODE_ROOT="/opt/${NODE_DISTRO}"
NODE_LINK="/opt/node24"
PROFILE_FILE="/etc/profile.d/cainbot-node.sh"
SERVICE_FILE="/etc/systemd/system/cainbot.service"

mkdir -p /opt

if [[ ! -x "${NODE_ROOT}/bin/node" ]]; then
  cd /tmp
  curl -fsSLO "https://nodejs.org/dist/${NODE_VERSION}/${NODE_ARCHIVE}"
  rm -rf "${NODE_ROOT}"
  tar -xJf "${NODE_ARCHIVE}" -C /opt
fi

ln -sfn "${NODE_ROOT}" "${NODE_LINK}"

cat > "${PROFILE_FILE}" <<'EOF'
export PATH="/opt/node24/bin:${PATH}"
EOF
chmod 644 "${PROFILE_FILE}"

mkdir -p "${PROJECT_DIR}/data/logs"
chmod +x "${PROJECT_DIR}/run-cain-bot.sh"

cat > "${SERVICE_FILE}" <<EOF
[Unit]
Description=CainBot
After=network.target

[Service]
Type=simple
WorkingDirectory=${PROJECT_DIR}
Environment=PATH=/opt/node24/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ExecStart=${PROJECT_DIR}/run-cain-bot.sh
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
echo "Node 已安装到 ${NODE_LINK}"
echo "systemd 服务文件已写入 ${SERVICE_FILE}"
echo "接下来可执行：systemctl enable --now cainbot"
