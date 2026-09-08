#!/usr/bin/env node
// ─────────────────────────────────────────────────────────────────────────────
// i18n 词典体检 —— 把 AGENT.md §6.6「文案 / i18n」里几条**原本只能靠人肉**的硬规则机器化。
// 跑法:`node scripts/check-i18n.mjs`(或 `pnpm check:i18n`);任一条不过 → 打中文报告 + exit 1。
// CI 在 `.github/workflows/ci.yml` 的 frontend job 里跑它,PR 上就能挡住漂移。
//
// 守的是 §6.6 的这几条:
//   ① **同步纪律**:「`zh-CN.ts` 加 key **必须**同步 `en.ts`,否则英文模式静默回落中文」
//      —— 静默 = 最难发现,所以这条是本脚本头号目标。§6.6 原文的复核办法(「两文件 flatten
//      后比 key 集 + 占位符集」)就是检查 ① 和 ② 的做法,这里逐字落成代码。
//   ② 每个 key 的**占位符集合**两边相等(漏了 `{name}` → 界面上少一截信息,也不报错)。
//   ③ **特殊字符陷阱**:字面 `{` `}` `@` `|` 进 t() 会被 vue-i18n 当自己的语法解析 →
//      编译失败 → 整组件 render 抛错,**表现为「tab 点了切不过去」且只有 warn 级**,极难查。
//      合法占位符 `{name}` 形不算违规(先扣掉再查裸露的语法字符);`@`(linked-message)、
//      `|`(复数分隔符)一律报 —— 要展示这类字符,把字面量留模板里、别进 `t()`。
//   ④ **绝不硬编助手名字**(§6.6 用户准则):名字 = 用户数据(`ui.pet_name`,空则回落
//      `pet.name`),用户随时能改;且「旺财」是暖萌皮名,写进中立底座即违反 §5。故词典值里
//      「旺财 / 7274 / Larkwing」只允许出现在 `pet.name` 这一个 key 上,别处一律 `{name}` 占位。
//      判据 = 「改一次昵称,所有露出的名字都跟着变」。
//   ⑤ 静态引用核对:代码里 `t('x.y')` 写了、两个词典都没定义 → 界面上直接显示 key 原文。
//      ⚠️ 大量 key 是**动态拼接**消费的(`tool.*`、`err.*`、`'reminders.repeat.' + v` …),
//      所以只查**完整字面量**;以 `.` 结尾的前缀形、含 `${` 的模板串一律跳过(见 SKIP 判据)。
//
// 设计取舍:
//   · locales 是 `export default { … }` 的 TS 模块,但内容是**纯 JS 对象字面量**(无类型标注),
//     所以首选把整个文件当 ESM 用 data: URL 直接 import —— 零依赖、零解析器、注释天然被丢掉。
//     万一以后有人往里写了 TS 语法,自动回落用 devDependencies 里的 `typescript` 转译一遍。
//   · 每条检查都打印「扫了多少」:一个匹配不到任何东西的正则会**静默全绿**,是这类脚本最
//     常见的假绿。数字在报告里露出来,人一眼能看出「扫到 0 处」不对。
// ─────────────────────────────────────────────────────────────────────────────

import { readFile, readdir } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const LOCALES = { zh: 'src/locales/zh-CN.ts', en: 'src/locales/en.ts' }
const SRC_DIR = path.join(ROOT, 'src')

/** 词典值里只允许出现在这个 key 上的助手名(§6.6 ④) */
const NAME_KEY_ALLOWLIST = 'pet.name'
const HARDCODED_NAMES = ['旺财', '7274', 'Larkwing']

/** vue-i18n 的命名 / 列表插值:{name} / {0}。用于「扣掉合法占位符」和「比占位符集」两处。 */
const PLACEHOLDER = /\{\s*([A-Za-z0-9_]+)\s*\}/g

const problems = []
const fail = (title, lines) => problems.push({ title, lines })

// ── 加载 ────────────────────────────────────────────────────────────────────

/** 把 `export default {…}` 的 locale 模块读成对象。 */
async function loadLocale(rel) {
  const abs = path.join(ROOT, rel)
  const src = await readFile(abs, 'utf8')
  const asDataUrl = (code) =>
    import('data:text/javascript;base64,' + Buffer.from(code, 'utf8').toString('base64'))
  try {
    return (await asDataUrl(src)).default
  } catch (e) {
    // 回落:文件里出现了 TS 语法 → 用 typescript 转译后再 import。
    try {
      const ts = (await import('typescript')).default
      const js = ts.transpileModule(src, {
        compilerOptions: { target: ts.ScriptTarget.ESNext, module: ts.ModuleKind.ESNext },
      }).outputText
      return (await asDataUrl(js)).default
    } catch (e2) {
      console.error(`✗ 读不动 ${rel}:先直接当 ESM import 失败(${e.message}),`)
      console.error(`  用 typescript 转译后仍失败(${e2.message})。`)
      console.error('  提示:typescript 在 devDependencies 里,先跑 pnpm install。')
      process.exit(1)
    }
  }
}

/** 嵌套对象拍平成 { 'a.b.c': 值 };数组按下标展开(现在没有,留着别踩坑)。 */
function flatten(obj, prefix = '', out = {}) {
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k
    if (v !== null && typeof v === 'object') flatten(v, key, out)
    else out[key] = v
  }
  return out
}

const dicts = {}
for (const [lang, rel] of Object.entries(LOCALES)) dicts[lang] = flatten(await loadLocale(rel))
const zh = dicts.zh
const en = dicts.en
const zhKeys = Object.keys(zh)
const enKeys = Object.keys(en)
const known = new Set([...zhKeys, ...enKeys])

console.log(`词典:${LOCALES.zh} ${zhKeys.length} 键 · ${LOCALES.en} ${enKeys.length} 键`)

// ── ① 键集完全相等 ──────────────────────────────────────────────────────────
{
  const zs = new Set(zhKeys)
  const es = new Set(enKeys)
  const missingInEn = zhKeys.filter((k) => !es.has(k))
  const missingInZh = enKeys.filter((k) => !zs.has(k))
  if (missingInEn.length || missingInZh.length) {
    const lines = []
    if (missingInEn.length)
      lines.push(
        `en.ts 缺 ${missingInEn.length} 个(英文模式会**静默回落中文**):`,
        ...missingInEn.map((k) => `    ${k}`),
      )
    if (missingInZh.length)
      lines.push(`zh-CN.ts 缺 ${missingInZh.length} 个:`, ...missingInZh.map((k) => `    ${k}`))
    fail('① 两个词典的键集不相等(§6.6 同步纪律)', lines)
  } else {
    console.log(`✓ ① 键集完全相等(${zhKeys.length} 键,零漂移)`)
  }
}

// ── ② 占位符集合相等 ────────────────────────────────────────────────────────
{
  const namesOf = (v) =>
    new Set(typeof v === 'string' ? [...v.matchAll(PLACEHOLDER)].map((m) => m[1]) : [])
  const bad = []
  let checked = 0
  for (const k of zhKeys) {
    if (!(k in en)) continue // ① 已经报过缺键,这里不重复刷屏
    checked++
    const a = namesOf(zh[k])
    const b = namesOf(en[k])
    const onlyZh = [...a].filter((x) => !b.has(x))
    const onlyEn = [...b].filter((x) => !a.has(x))
    if (onlyZh.length || onlyEn.length) {
      bad.push(
        `${k}:zh 有 {${[...a].join(', ')}} / en 有 {${[...b].join(', ')}}` +
          (onlyZh.length ? ` → en 漏了 ${onlyZh.map((x) => `{${x}}`).join(' ')}` : '') +
          (onlyEn.length ? ` → zh 漏了 ${onlyEn.map((x) => `{${x}}`).join(' ')}` : ''),
      )
    }
  }
  if (bad.length) fail('② 占位符集合两边不一致', bad.map((s) => `  ${s}`))
  else console.log(`✓ ② 占位符集合两边一致(比了 ${checked} 个共有键)`)
}

// ── ③ 不含字面 { } @ |(vue-i18n 语法字符) ─────────────────────────────────
{
  const bad = []
  let scanned = 0
  for (const [lang, dict] of Object.entries(dicts)) {
    for (const [k, v] of Object.entries(dict)) {
      if (typeof v !== 'string') continue
      scanned++
      const hits = []
      // 先扣掉合法占位符,剩下的花括号就是裸露的语法字符
      if (/[{}]/.test(v.replace(PLACEHOLDER, ''))) hits.push('裸 { 或 }')
      if (v.includes('@')) hits.push('@(linked-message)')
      if (v.includes('|')) hits.push('|(复数分隔符)')
      if (hits.length) bad.push(`${lang}:${k} → ${hits.join(' / ')}\n      ${JSON.stringify(v)}`)
    }
  }
  if (bad.length)
    fail('③ 消息串里有 vue-i18n 语法字符(会让整组件 render 抛错,表现为「tab 点了切不过去」)', [
      ...bad.map((s) => `  ${s}`),
      '  修法:纯文本描述这类字符,或把含它的字面量留在模板里、不要进 t()。',
    ])
  else console.log(`✓ ③ 无裸露的 { } @ |(扫了 ${scanned} 条消息串)`)
}

// ── ④ 不硬编助手名字 ───────────────────────────────────────────────────────
{
  const bad = []
  let scanned = 0
  for (const [lang, dict] of Object.entries(dicts)) {
    for (const [k, v] of Object.entries(dict)) {
      if (typeof v !== 'string' || k === NAME_KEY_ALLOWLIST) continue
      scanned++
      const hit = HARDCODED_NAMES.filter((n) => v.includes(n))
      if (hit.length) bad.push(`${lang}:${k} → 出现「${hit.join('」「')}」\n      ${JSON.stringify(v)}`)
    }
  }
  if (bad.length)
    fail('④ 词典值里硬编了助手名字(§6.6 用户准则:名字是用户数据)', [
      ...bad.map((s) => `  ${s}`),
      `  修法:改成 {name} 占位,渲染点注入 petName = settings.get('ui.pet_name') || t('pet.name')。`,
      `  名字跨语言相同、不进词典,所以 zh/en 共用同一个 {name} 占位。唯一例外是 ${NAME_KEY_ALLOWLIST}。`,
    ])
  else console.log(`✓ ④ 无硬编助手名(扫了 ${scanned} 条,${NAME_KEY_ALLOWLIST} 已豁免)`)
}

// ── ⑤ 静态引用核对 ─────────────────────────────────────────────────────────
{
  // 匹配 t('a.b') / $t("a.b") / te('a.b');前面不能紧跟标识符字符或 `.`,
  // 免得 `format(` 之类尾部的 t 被当成调用。
  const CALL = /(?<![A-Za-z0-9_$.])\$?(?:t|te)\(\s*(['"`])((?:[^'"`\\\n]|\\.)*?)\1/g

  /** 跳过判据:动态拼接的前缀形一律不当错报(见文件头 ⑤)。 */
  const skip = (key) =>
    !key || // 空串
    key.includes('${') || // 模板串
    key.endsWith('.') || // 'reminders.repeat.' + v 这种前缀
    !/^[A-Za-z][A-Za-z0-9_.]*$/.test(key) // 不长得像 key(含空格/中文 = 普通字符串参数)

  async function walk(dir, acc = []) {
    for (const e of await readdir(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name)
      if (e.isDirectory()) await walk(p, acc)
      else if (/\.(vue|ts)$/.test(e.name)) acc.push(p)
    }
    return acc
  }

  const files = await walk(SRC_DIR)
  const miss = new Map()
  let refs = 0
  for (const f of files) {
    const src = await readFile(f, 'utf8')
    for (const m of src.matchAll(CALL)) {
      const key = m[2]
      if (skip(key)) continue
      refs++
      if (known.has(key)) continue
      const rel = path.relative(ROOT, f)
      if (!miss.has(key)) miss.set(key, new Set())
      miss.get(key).add(rel)
    }
  }
  if (miss.size)
    fail('⑤ 代码里引用了两个词典都没定义的 key(界面会直接显示 key 原文)', [
      ...[...miss].map(([k, fs]) => `  ${k}  ←  ${[...fs].join(', ')}`),
      '  若它其实是动态拼接的前缀,请把它改成以 . 结尾的形式,或调整本脚本的 skip 判据。',
    ])
  else
    console.log(
      `✓ ⑤ 静态引用全部有定义(扫了 ${files.length} 个文件里 ${refs} 处字面量 key;动态拼接的前缀形已跳过)`,
    )
}

// ── 报告 ────────────────────────────────────────────────────────────────────
if (problems.length) {
  console.error(`\n✗ i18n 体检不通过:${problems.length} 类问题\n`)
  for (const { title, lines } of problems) {
    console.error(title)
    for (const l of lines) console.error(l)
    console.error('')
  }
  console.error('规则出处:AGENT.md §6.6「文案 / i18n」。')
  process.exit(1)
}
console.log('\n✓ i18n 体检全绿(5/5)')
