#!/usr/bin/env bash
# 毒锁守卫:全仓不许再出现裸的 std 取锁。
#
# 病(AGENT §6.10「毒锁」):`std::sync::Mutex` / `RwLock` 带毒锁机制 —— 持锁线程 panic,
# 这把锁被**永久**标记中毒,之后每次 `lock()` 都 `Err`。写成 `.lock().unwrap()` 就等于
# 「第一个 panic 之后每个访问点都跟着 panic」;而 tokio 任务 panic 不杀进程
#(`panic = "abort"` 是根 Cargo.toml 刻意不加的),于是程序活着、那块状态永久废掉。
#
# 药:一律走 `crate::lockext` 的 `.lk()` / `.rd()` / `.wr()`(中毒则 warn 一句后
# `into_inner()` 接着用)。这个脚本就是那条规则的机器守卫 —— 人肉 review 数 250 个
# 调用点是靠不住的(同 `check-i18n.mjs` 立的规矩)。
#
# 跑法:`bash scripts/check-locks.sh`(已进 push/PR CI)。

set -uo pipefail
cd "$(dirname "$0")/.."

DIRS=(larkwing-core/src src-tauri/src)
# lockext.rs 自己就是那个解毒实现,它**必须**调裸 `self.lock()`;
# 它的 doc 注释里也逐字写着被禁的形态当反例。故整文件豁免。
EXEMPT='larkwing-core/src/lockext.rs'

fail=0

# 一条禁令 = 名字 + ERE。scan() 命中即打印全部现场并置 fail。
scan() {
  local name="$1" re="$2" hits
  hits=$(grep -rnE --include='*.rs' "$re" "${DIRS[@]}" | grep -v "$EXEMPT" || true)
  if [[ -n "$hits" ]]; then
    echo "✗ $name —— 命中 $(printf '%s\n' "$hits" | wc -l | tr -d ' ') 处:"
    printf '%s\n' "$hits" | sed 's/^/    /'
    fail=1
  else
    echo "✓ $name —— 0 处"
  fi
}

echo "── 毒锁守卫(扫 ${DIRS[*]},豁免 $EXEMPT)"

# 直接 panic 的:.lock().unwrap() / .lock().expect(…) / .read()/.write() 同款
scan 'Mutex 裸取锁(.lock().unwrap / .expect)' '\.lock\(\)\.(unwrap|expect)\('
scan 'Mutex 裸取锁(.unwrap 无参形)'          '\.lock\(\)\.unwrap\(\)'
# 换行形:`.lock()` 单独一行,下一行才是 .unwrap()/.expect() —— 单行正则抓不到,
# 故直接禁「以 .lock() 结尾的行」(合法写法里不存在这种:`.lk()` 不需要续行)。
scan 'Mutex 裸取锁(换行形 .lock() ⏎ .unwrap)' '\.lock\(\)[[:space:]]*$'
# RwLock:空括号的 .read()/.write() 只可能是锁(io::Read/Write 那两个方法都收 buf 参数)
scan 'RwLock 裸取锁(.read()/.write() + unwrap/expect)' '\.(read|write)\(\)\.(unwrap|expect)\('
# 「中毒就静默跳过」也不行(§3.5 不静默失败):中毒是要记一句的,不是当没发生
scan '中毒静默跳过(if let Ok(..) = ..lock())' 'let Ok\(.*\.lock\(\)'

# ── 阳性对照:证明上面的扫描真的在看代码 ────────────────────────────────────
# check-i18n.mjs 立的规矩:每条都报「扫了多少」,防「正则没匹配到东西却静默全绿」。
# 若解毒调用点掉到个位数,几乎只能是路径写错 / 文件搬家 → 上面那堆 ✓ 全是假绿。
used=$(grep -rohE --include='*.rs' '\.(lk|rd|wr)\(\)' "${DIRS[@]}" | grep -cv '^$' || true)
echo "── 阳性对照:解毒取锁 .lk()/.rd()/.wr() 共 $used 处"
if (( used < 100 )); then
  echo "✗ 解毒调用点只有 $used 处(预期 200+)—— 扫描路径可能不对,上面的结论不可信"
  fail=1
fi

if (( fail )); then
  echo
  echo "改法:改用 crate::lockext 的 .lk() / .rd() / .wr()(src-tauri 侧:larkwing_core::lockext)。"
  echo "缘由见 larkwing-core/src/lockext.rs 头部与 AGENT.md §6.10。"
  exit 1
fi
echo "── 全部通过"
