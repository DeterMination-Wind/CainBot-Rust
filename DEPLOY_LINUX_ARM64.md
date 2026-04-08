# Linux ARM64 Deployment

本仓库当前线上部署目标是 ARM64 Linux，例如 OrangePi 上的 `/Cainbot`。

## 目标

- 本地在 Windows WSL 内交叉编译 ARM64 静态二进制
- 上传到远端 `/Cainbot`
- 重启 `cainbot.service`
- 避免 `run-cain-bot.sh` 因 CRLF 或 `HOME` 缺失导致启动失败

## 前置条件

在 WSL 中准备好：

```bash
rustup target add aarch64-unknown-linux-musl
cargo install cargo-zigbuild
```

需要 `zig` 可执行文件在 PATH 中。

## 构建命令

在 Windows PowerShell 中执行：

```powershell
wsl.exe -e bash -lc "cd /mnt/c/Users/华硕/Documents/NapCatCainBot-worktree-rust && RUSTFLAGS='-Awarnings' cargo zigbuild --release --target aarch64-unknown-linux-musl --bin cainbot-rs"
```

产物位置：

```text
target/aarch64-unknown-linux-musl/release/cainbot-rs
```

## 为什么带 `RUSTFLAGS='-Awarnings'`

当前 WSL 环境里的 `rustc 1.94.1` 在这个项目上做 ARM64 交叉编译时，可能会在发 dead-code warning 时 ICE。禁用 warning 输出可以稳定拿到产物。

如果编译器版本后续修复了这个问题，可以去掉这段 `RUSTFLAGS` 再试。

## 上传与替换

上传新二进制：

```powershell
& 'C:\Program Files\PuTTY\pscp.exe' -pw orangepi C:\Users\华硕\Documents\NapCatCainBot-worktree-rust\target\aarch64-unknown-linux-musl\release\cainbot-rs root@192.168.110.98:/Cainbot/target/release/cainbot-rs.new
```

远端安装并覆盖：

```powershell
& 'C:\Program Files\PuTTY\plink.exe' -batch -pw orangepi root@192.168.110.98 "bash -lc 'cd /Cainbot && install -m 755 target/release/cainbot-rs.new target/release/cainbot-rs && install -m 755 target/release/cainbot-rs.new cainbot-rs && rm -f target/release/cainbot-rs.new'"
```

## 重启与验证

```powershell
& 'C:\Program Files\PuTTY\plink.exe' -batch -pw orangepi root@192.168.110.98 "systemctl restart cainbot; sleep 4; systemctl --no-pager --full status cainbot | head -n 80; echo '---PORTS---'; ss -ltnp | grep -E '(:3000|:3186|:15721)' || true; echo '---LOGS---'; journalctl -u cainbot -n 80 --no-pager"
```

至少确认：

- `cainbot.service` 为 `active (running)`
- `127.0.0.1:3186` 由 `cainbot-rs` 监听
- 日志出现 `NapCat SSE 已连接。`

## 换行与启动脚本约束

仓库通过 `.gitattributes` 强制 `*.sh` 使用 LF。

此外，`scripts/setup-cainbot-linux.sh` 现在会在远端执行：

```bash
sed -i 's/\r$//' "${PROJECT_DIR}/run-cain-bot.sh"
```

这意味着即使 `run-cain-bot.sh` 被 Windows 工具错误写成 CRLF，安装脚本也会在部署机上修正它。

## systemd 环境约束

`run-cain-bot.sh` 不能假设 systemd 一定提供 `HOME`。

因此：

- `run-cain-bot.sh` 直接使用 `CARGO_BIN=/root/.cargo/bin/cargo`
- `scripts/setup-cainbot-linux.sh` 生成的 service 文件会额外写入 `Environment=HOME=/root`

这两层同时存在，避免下次因为环境变量差异导致启动失败。
