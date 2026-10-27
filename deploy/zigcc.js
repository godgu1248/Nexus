// zig cc wrapper for cross-linking x86_64-unknown-linux-musl from Windows.
//
// Problems this solves:
//  1) cc-rs passes --target=x86_64-unknown-linux-musl, zig wants -target x86_64-linux-musl.
//  2) rustc passes its own musl CRT objects (rcrt1.o / crti.o / crtn.o) inside an @response
//     file, and zig injects its own crt1.o -> ld.lld: duplicate symbol: _start / _init.
//     So: expand @response files, drop the CRT objects, let zig supply crt + musl.
//  3) Windows caps a command line at 32767 chars. The final link of this crate passes ~195
//     object files with long absolute paths, which overflows it and dies as
//     `spawnSync ... ENAMETOOLONG` -- a message that says nothing about argument length.
//     So: when the argv would be too long, write our own response file and call zig with @file.
const fs = require('fs');
const os = require('os');
const path = require('path');
const { spawnSync } = require('child_process');

// 默认 C:\zig\zig.exe（ASCII-only 路径），可用 ZIG_EXE 覆盖（deploy/build-musl.ps1 会设）。
const ZIG = process.env.ZIG_EXE || 'C:\\zig\\zig.exe';
// CreateProcess 的上限是 32767（含程序名与结尾的 NUL）。留 4 KB 余量给程序名与转义。
const MAX_ARGV_CHARS = 28_000;

const CRT = new Set([
  'crt1.o', 'rcrt1.o', 'Scrt1.o', 'crti.o', 'crtn.o',
  'crtbegin.o', 'crtbeginS.o', 'crtbeginT.o', 'crtend.o', 'crtendS.o',
]);

function expand(args) {
  const out = [];
  for (const a of args) {
    if (a.startsWith('@')) {
      const file = a.slice(1);
      const text = fs.readFileSync(file, 'utf8');
      for (const line of text.split(/\r?\n/)) {
        const t = line.trim();
        if (t) out.push(t);
      }
    } else {
      out.push(a);
    }
  }
  return out;
}

// clang/gcc 风格的 @file 解析：按空白切分，双引号内保留空白。
// 注意：这里**不动反斜杠**——Windows 路径里全是反斜杠，按 C 字符串规则去转义会
// 把 `C:\zig\...` 变成 `C:\\zig\\...`，clang 的 tokenizer 只会把第一个反斜杠当普通字符，
// 结果路径整体错位。真出现带空格的路径时用双引号包住即可（本机没有这种路径，留兜底）。
function quote(arg) {
  if (!/[\s"]/.test(arg)) return arg;
  return '"' + arg.replace(/"/g, '\\"') + '"';
}

const args = expand(process.argv.slice(2)).filter((a) => {
  if (a.startsWith('--target=')) return false;
  const base = path.basename(a.replace(/\\/g, '/'));
  return !CRT.has(base);
});

// `cc` 必须留在命令行上：zig 把第一个参数当子命令，`zig @file` 会报
// `error: unknown command: @C:\...rsp`。
const tail = ['-target', 'x86_64-linux-musl', ...args];
const length = tail.reduce((sum, a) => sum + a.length + 3, ZIG.length + 64);

let argv = ['cc', ...tail];
if (length > MAX_ARGV_CHARS) {
  const rsp = path.join(os.tmpdir(), `zigcc-${process.pid}-${Date.now()}.rsp`);
  fs.writeFileSync(rsp, tail.map(quote).join('\n'), 'utf8');
  argv = ['cc', '@' + rsp];
  // 只在显式要求时说一句。这不是错误，但 cargo 会把链接器的 stderr 当成
  // "warning: <crate> generated 1 warning"，于是每次构建都像是带着问题。
  if (process.env.ZIGCC_VERBOSE) {
    console.error(`zig wrapper: argv ${length} chars, using response file ${rsp}`);
  }
}

const res = spawnSync(ZIG, argv, { stdio: 'inherit' });
if (res.error) {
  console.error('zig wrapper failed:', res.error.message);
  process.exit(1);
}
process.exit(res.status === null ? 1 : res.status);
