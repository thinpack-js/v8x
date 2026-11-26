import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { brotliCompressSync, brotliDecompressSync, constants } from 'node:zlib';

const zoneinfo = process.argv[2];
if (!zoneinfo) throw new Error('Pass zoneinfo64 0.3.0 src/data/zoneinfo64.res');
const zoneinfoSha256 = 'a24d2455af4efced6b261a2ed49da040a1e472c30899c68e7db377871db695bd';
if (createHash('sha256').update(readFileSync(zoneinfo)).digest('hex') !== zoneinfoSha256) {
  throw new Error('Expected the pinned zoneinfo64 0.3.0 resource');
}
const data = new URL('../vendor/temporal-polyfill/', import.meta.url);
for (const [input, output] of [
  [new URL('index.umd.js', data), new URL('index.umd.js.br', data)],
  [zoneinfo, new URL('zoneinfo64.res.br', data)],
]) {
  const raw = readFileSync(input);
  const compressed = brotliCompressSync(raw, {
    params: { [constants.BROTLI_PARAM_QUALITY]: 11 },
  });
  if (!brotliDecompressSync(compressed).equals(raw)) throw new Error('Brotli round-trip failed');
  writeFileSync(output, compressed);
}
