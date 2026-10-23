// Parse ELF header + program headers: report class/type/machine and whether PT_INTERP exists.
const fs = require('fs');
const buf = fs.readFileSync(process.argv[2]);
if (buf[0] !== 0x7f || buf[1] !== 0x45 || buf[2] !== 0x4c || buf[3] !== 0x46) {
  console.log('NOT_ELF');
  process.exit(2);
}
const is64 = buf[4] === 2;
const little = buf[5] === 1;
const type = is64 ? buf.readUInt16LE(16) : buf.readUInt16LE(16);
const machine = buf.readUInt16LE(18);
const phoff = is64 ? Number(buf.readBigUInt64LE(32)) : buf.readUInt32LE(28);
const phentsize = is64 ? buf.readUInt16LE(54) : buf.readUInt16LE(42);
const phnum = is64 ? buf.readUInt16LE(56) : buf.readUInt16LE(44);
let interp = null;
for (let i = 0; i < phnum; i++) {
  const off = phoff + i * phentsize;
  const ptype = buf.readUInt32LE(off);
  if (ptype === 3) {
    const p_offset = is64 ? Number(buf.readBigUInt64LE(off + 8)) : buf.readUInt32LE(off + 4);
    interp = buf.toString('utf8', p_offset, p_offset + 64).replace(/\0+$/, '');
  }
}
console.log('class=' + (is64 ? 'ELF64' : 'ELF32'));
console.log('little_endian=' + little);
console.log('e_type=' + type + (type === 2 ? ' (ET_EXEC)' : type === 3 ? ' (ET_DYN/PIE)' : ''));
console.log('e_machine=' + machine + (machine === 62 ? ' (x86-64)' : ''));
console.log('PT_INTERP=' + (interp === null ? 'none (static)' : interp));
console.log('size=' + buf.length);
