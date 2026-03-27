#!/usr/bin/env node
/**
 * Test the byte→char offset mapping used by buildSnippets in the playground.
 * Verifies that the highlights from the Rust engine (byte offsets) are
 * correctly mapped to JS string char offsets.
 */

import { readFileSync } from 'fs';
import { fileURLToPath } from 'url';
import { dirname, join } from 'path';

const __dirname = dirname(fileURLToPath(import.meta.url));

// Read the doc that has highlight issues
const text = readFileSync(
  join(__dirname, '../docs/19-mars-2026/11-plan-observabilite-avancee-luciole.md'),
  'utf-8'
);

// These are the correct byte offsets from the Rust engine (verified by test)
const byteOffsets = [
  [96, 106], [819, 829], [3542, 3552], [7134, 7144], [7319, 7329],
  [7678, 7688], [8271, 8281], [9734, 9744], [10099, 10109], [10257, 10267],
  [10372, 10382], [10769, 10779], [11463, 11473], [11948, 11958],
  [12033, 12043], [12278, 12288], [12506, 12516], [12709, 12719],
  [12898, 12908], [13034, 13044], [13445, 13455], [15927, 15937],
  [15979, 15989], [18859, 18869],
];

// This is EXACTLY the buildSnippets byte→char mapping from index.html
const encoder = new TextEncoder();
const bytes = encoder.encode(text);

const byteToChar = new Int32Array(bytes.length + 1);
let charIdx = 0, byteIdx = 0;
while (byteIdx < bytes.length) {
  byteToChar[byteIdx] = charIdx;
  const b = bytes[byteIdx];
  const len = b < 0x80 ? 1 : b < 0xE0 ? 2 : b < 0xF0 ? 3 : 4;
  for (let k = 1; k < len && byteIdx + k < bytes.length; k++) {
    byteToChar[byteIdx + k] = charIdx;
  }
  byteIdx += len;
  charIdx += (len === 4) ? 2 : 1;
}
byteToChar[bytes.length] = charIdx;

console.log(`Text: ${text.length} chars, ${bytes.length} bytes`);
console.log(`Multi-byte chars: ${bytes.length - text.length}`);
console.log();

let errors = 0;
for (const [bs, be] of byteOffsets) {
  const cs = byteToChar[Math.min(bs, bytes.length)];
  const ce = byteToChar[Math.min(be, bytes.length)];
  const highlighted = text.slice(cs, ce);
  const ok = highlighted.toLowerCase() === 'rag3weaver';

  if (!ok) errors++;
  console.log(
    `bytes ${bs}..${be} → chars ${cs}..${ce} → ${JSON.stringify(highlighted)} ${ok ? '✓' : '✗ WRONG'}`
  );
}

console.log();
if (errors > 0) {
  console.log(`❌ ${errors} highlights are WRONG`);

  // Debug: show what's around the first wrong offset
  const [bs, be] = byteOffsets[0];
  const cs = byteToChar[bs];
  const ce = byteToChar[be];
  console.log(`\nDebug first highlight:`);
  console.log(`  byte ${bs} → char ${cs}`);
  console.log(`  byte ${be} → char ${ce}`);
  console.log(`  text.slice(${cs-2}, ${ce+2}) = ${JSON.stringify(text.slice(cs-2, ce+2))}`);
  console.log(`  text.slice(${cs}, ${ce}) = ${JSON.stringify(text.slice(cs, ce))}`);

  // Check: where is "rag3weaver" in chars?
  const charPos = text.toLowerCase().indexOf('rag3weaver');
  console.log(`  actual char pos of first "rag3weaver" = ${charPos}`);
  console.log(`  text.slice(${charPos}, ${charPos+10}) = ${JSON.stringify(text.slice(charPos, charPos+10))}`);
} else {
  console.log('✅ All highlights are correct');
}
