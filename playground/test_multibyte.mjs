#!/usr/bin/env node
import { readFileSync } from 'fs';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const text = readFileSync(join(__dirname, '../docs/19-mars-2026/11-plan-observabilite-avancee-luciole.md'), 'utf-8');

const encoder = new TextEncoder();
const bytes = encoder.encode(text);

// Find all multi-byte chars and their positions
let charIdx = 0, byteIdx = 0;
const multibyte = [];
while (byteIdx < bytes.length) {
  const b = bytes[byteIdx];
  const len = b < 0x80 ? 1 : b < 0xE0 ? 2 : b < 0xF0 ? 3 : 4;
  if (len > 1) {
    const char = text[charIdx];
    const codePoint = text.codePointAt(charIdx);
    // Check if this char is a surrogate pair in UTF-16
    const jsLen = codePoint > 0xFFFF ? 2 : 1;
    multibyte.push({ byteIdx, charIdx, len, jsLen, char, codePoint: codePoint.toString(16) });
    if (jsLen !== 1) {
      console.log(`⚠️  SURROGATE PAIR at byte ${byteIdx} char ${charIdx}: U+${codePoint.toString(16)} (${len} bytes, ${jsLen} JS chars)`);
    }
  }
  byteIdx += len;
  charIdx++;
}

console.log(`Total multi-byte chars: ${multibyte.length}`);
console.log(`3-byte chars: ${multibyte.filter(m => m.len === 3).length}`);
console.log(`4-byte chars: ${multibyte.filter(m => m.len === 4).length}`);

// Show first 3-byte chars
const threeByte = multibyte.filter(m => m.len === 3);
if (threeByte.length > 0) {
  console.log('\n3-byte characters:');
  for (const m of threeByte.slice(0, 10)) {
    console.log(`  byte ${m.byteIdx} char ${m.charIdx}: '${m.char}' U+${m.codePoint}`);
  }
}

// The critical question: does the byteToChar mapping match text.length?
console.log(`\nFinal: bytes=${bytes.length}, text.length=${text.length}, charIdx=${charIdx}`);
console.log(`Match: ${charIdx === text.length ? '✓' : '✗ MISMATCH'}`);
