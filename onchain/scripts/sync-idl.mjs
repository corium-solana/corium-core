// Copy the built IDL to shared/chain, where the live client, the crank scripts
// and onchain/Dockerfile all read it from.
//
// This is a hand copy in practice, and it drifted: shared/chain/soldust.json sat
// at `address` Coriumcq... while its `initialize.program_data` seed still held
// the pre-rename program id. Nothing broke, because only `initialize` derives
// that PDA and it had already run - but the same drift in any other seed would
// have every client-side PDA land on an address the program does not accept.
// Hence the check below rather than a bare cp.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const src = path.resolve(here, '../target/idl/soldust.json');
const dst = path.resolve(here, '../../shared/chain/soldust.json');

if (!fs.existsSync(src)) {
  console.error(`missing ${src} - run \`anchor build\` first`);
  process.exit(1);
}

const idl = JSON.parse(fs.readFileSync(src, 'utf8'));

const B58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';
function base58(bytes) {
  const digits = [0];
  for (const byte of bytes) {
    let carry = byte;
    for (let i = 0; i < digits.length; i++) {
      carry += digits[i] << 8;
      digits[i] = carry % 58;
      carry = (carry / 58) | 0;
    }
    while (carry) {
      digits.push(carry % 58);
      carry = (carry / 58) | 0;
    }
  }
  let leading = '';
  for (const byte of bytes) {
    if (byte !== 0) break;
    leading += '1';
  }
  return leading + digits.reverse().map((d) => B58[d]).join('');
}

// Every 32-byte const seed that equals a program id should be this program's.
const stale = [];
for (const ix of idl.instructions ?? []) {
  for (const acc of ix.accounts ?? []) {
    for (const seed of acc.pda?.seeds ?? []) {
      if (seed.kind !== 'const' || seed.value?.length !== 32) continue;
      const encoded = base58(seed.value);
      if (encoded.length >= 32 && encoded !== idl.address) {
        stale.push(`${ix.name}.${acc.name}: ${encoded}`);
      }
    }
  }
}

if (stale.length) {
  console.error(`IDL address is ${idl.address} but these 32-byte seeds disagree:`);
  for (const s of stale) console.error(`  ${s}`);
  console.error('run `anchor keys sync && anchor build` before syncing');
  process.exit(1);
}

fs.copyFileSync(src, dst);
console.log(`synced IDL -> ${path.relative(path.resolve(here, '../..'), dst)} (address ${idl.address})`);
