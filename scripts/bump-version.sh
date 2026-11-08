#!/usr/bin/env bash
# 版本号「一个口子」:一条命令把所有清单改到 X.Y.Z + 同步 Cargo.lock。
#   用法:scripts/bump-version.sh 0.1.5
# 只改版本号,不提交、不打 tag(那两步你确认后手动)。
#
# 版本真源只剩两处(两个生态本就分家,Rust 与 npm 没法共用同一字面量):
#   ① Rust 工作区版本 —— 根 Cargo.toml [workspace.package].version;两个 crate 用
#      `version.workspace = true` 继承,所以 Rust 侧只此一处。
#   ② 前端 + Tauri 应用 —— package.json + tauri.conf.json(app/安装包/getVersion/Release 标题
#      都取 tauri.conf)。
# 本脚本一次写齐这两处;Cargo.lock 由 cargo check 自动同步。
set -euo pipefail
V="${1:?用法: scripts/bump-version.sh X.Y.Z}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# ① Rust 工作区版本(根 Cargo.toml 的 [workspace.package].version 是唯一 `^version = "x.y.z"`)
sed -i '' -E "s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"$V\"/" "$ROOT/Cargo.toml"
# ② 前端 + Tauri
sed -i '' -E "s/\"version\": \"[0-9]+\.[0-9]+\.[0-9]+\"/\"version\": \"$V\"/" \
  "$ROOT/package.json" "$ROOT/src-tauri/tauri.conf.json"

# Cargo.lock 同步 + 编译校验(两个 crate 继承 workspace 版本)
( cd "$ROOT" && cargo check --workspace >/dev/null )

# 在线朗读真网冒烟(AGENT §7.6):微软 Edge 朗读是逆向接口,服务端会单方面改握手 ——
# 2026-10 强制 Sec-MS-GEC v2,msedge-tts 0.4.0 全量 403,朗读哑了近一个月、两个版本照发才发现。
# 挂了就停:先查 crates.io 有没有新版 msedge-tts。确知上游还没修、又必须先发别的修复时,
# SKIP_TTS_SMOKE=1 跳过(CHANGELOG 里如实写「朗读暂不可用」)。
SMOKE="在线朗读冒烟已过"
if [ "${SKIP_TTS_SMOKE:-}" = "1" ]; then
  SMOKE="在线朗读冒烟已跳过"
  echo "⚠ 已跳过在线朗读冒烟(SKIP_TTS_SMOKE=1)—— 记得在 CHANGELOG 里如实说明"
elif ! ( cd "$ROOT" && cargo test -q -p larkwing-core --lib edge_real_synth_smoke -- --ignored >/dev/null 2>&1 ); then
  echo "✗ 在线朗读冒烟没过:微软 Edge 朗读连不上 / 被拒(403 = 握手改了)。" >&2
  echo "  复现:cargo test -p larkwing-core --lib edge_real_synth_smoke -- --ignored --nocapture" >&2
  echo "  先查 crates.io 的 msedge-tts 有没有新版;版本号已改,修好后重跑本脚本即可。" >&2
  exit 1
fi

echo "✓ 版本号已全部改到 $V(Cargo.toml 工作区 / package.json / tauri.conf.json + Cargo.lock);$SMOKE"
echo "  下一步:CHANGELOG.md 顶部加一节 '## $V — 日期',然后"
echo "          git add -A && git commit -m \"v$V:…\" && git tag v$V && git push origin main v$V"
