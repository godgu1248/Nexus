// zig cc wrapper used by deploy/build-musl.ps1 when cross-compiling Nexus from Windows
// to x86_64-unknown-linux-musl. Keep this file pure ASCII.
//
// Two things it has to do that a plain "zig cc" invocation does not:
//  1) cc-rs passes --target=x86_64-unknown-linux-musl; zig only knows x86_64-linux-musl,
//     and reports "error: UnknownOperatingSystem" otherwise. So drop it and pass zig's form.
//  2) On Windows rustc hands the linker an @response file that contains its own musl CRT
//     objects (rcrt1.o, crti.o, ...). zig injects crt1.o too, so ld.lld stops with
//     "duplicate symbol: _start". Expand every @file and drop the CRT objects; zig then
//     provides crt + libc and the link succeeds.
const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const ZIG = process.env.ZIG_EXE || 'C:\\zig\\zig.exe';

const CRT = new Set([
  'crt1.o', 'rcrt1.o', 'Scrt1.o', 'crti.o', 'crtn.o',
  'crtbegin.o', 'crtbeginS.o', 'crtbeginT.o', 'crtend.o', 'crtendS.o',
]);

function expand(args) {
  const out = [];
  for (const a of args) {
    if (a.startsWith('@')) {
      const text = fs.readFileSync(a.slice(1), 'utf8');
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

const args = expand(process.argv.slice(2)).filter((a) => {
  if (a.startsWith('--target=')) return false;
  const base = path.basename(a.replace(/\\/g, '/'));
  return !CRT.has(base);
});

const res = spawnSync(ZIG, ['cc', '-target', 'x86_64-linux-musl', ...args], { stdio: 'inherit' });
if (res.error) {
  console.error('zig wrapper failed:', res.error.message);
  process.exit(1);
}
process.exit(res.status === null ? 1 : res.status);
