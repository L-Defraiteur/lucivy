import { Index } from './index.js'
import { rmSync, existsSync } from 'fs'

const path = '/tmp/lucivy_nodejs_e2e'
if (existsSync(path)) rmSync(path, { recursive: true })

console.log('=== NODE.JS BINDING E2E ===\n')

// Create
const idx = Index.create(path, [
  { name: 'title', type: 'text', stored: true },
  { name: 'body', type: 'text', stored: true },
  { name: 'category', type: 'text', stored: true },
  { name: 'priority', type: 'u64', fast: true },
], 2)

// Add
idx.add(1, { title: 'Mutex locking', body: 'The mutex lock mechanism is simple', category: 'kernel', priority: 3 })
idx.add(2, { title: 'Spinlocks', body: 'Multiple locks are held for spinlock', category: 'kernel', priority: 2 })
idx.add(3, { title: 'Scheduling', body: 'The scheduler handles locking primitives', category: 'kernel', priority: 5 })
idx.add(4, { title: 'Unlocking', body: 'Unlock the resource safely', category: 'userspace', priority: 1 })
idx.add(5, { title: 'Clock hw', body: 'This clock hardware has no lock', category: 'drivers', priority: 4 })
idx.add(6, { title: 'Memory', body: 'Memory management and allocation routines', category: 'kernel', priority: 3 })
idx.add(7, { title: 'Network', body: 'TCP socket locking and mutex handling', category: 'network', priority: 4 })
idx.add(8, { title: 'Filesystem', body: 'File locking with flock and lockf', category: 'filesystem', priority: 2 })
idx.commit()
console.log(`Created ${idx.numDocs} docs, ${idx.numShards} shards\n`)

// 1. Contains + highlights
console.log('--- 1. Contains "lock" + highlights ---')
let r = idx.search({ type: 'contains', field: 'body', value: 'lock' }, { highlights: true, limit: 3 })
r.forEach(x => console.log(`  doc=${x.docId} score=${x.score.toFixed(3)} hl=${JSON.stringify(x.highlights)}`))

// 2. startsWith
console.log('\n--- 2. startsWith "lock" ---')
r = idx.search({ type: 'startsWith', field: 'body', value: 'lock' }, { highlights: true, limit: 3 })
r.forEach(x => console.log(`  doc=${x.docId} score=${x.score.toFixed(3)} hl=${JSON.stringify(x.highlights)}`))

// 3. Fuzzy
console.log('\n--- 3. Fuzzy "mutx" d=1 ---')
r = idx.search({ type: 'contains', field: 'body', value: 'mutx', distance: 1 }, { highlights: true })
r.forEach(x => console.log(`  doc=${x.docId} score=${x.score.toFixed(3)} hl=${JSON.stringify(x.highlights)}`))

// 4. Regex
console.log('\n--- 4. Regex "lock[a-z]*" ---')
r = idx.search({ type: 'contains', field: 'body', value: 'lock[a-z]*', regex: true }, { highlights: true, limit: 3 })
r.forEach(x => console.log(`  doc=${x.docId} score=${x.score.toFixed(3)} hl=${JSON.stringify(x.highlights)}`))

// 5. Filter priority >= 3
console.log('\n--- 5. Contains "lock" + filter priority>=3 ---')
r = idx.search({
  type: 'contains', field: 'body', value: 'lock',
  filters: [{ field: 'priority', op: 'gte', value: 3 }]
}, { highlights: true })
r.forEach(x => console.log(`  doc=${x.docId} score=${x.score.toFixed(3)}`))

// 6. Filter category eq
console.log('\n--- 6. Contains "lock" + filter category="kernel" ---')
r = idx.search({
  type: 'contains', field: 'body', value: 'lock',
  filters: [{ field: 'category', op: 'eq', value: 'kernel' }]
}, { highlights: true })
r.forEach(x => console.log(`  doc=${x.docId} score=${x.score.toFixed(3)}`))

// 7. allowed_ids
console.log('\n--- 7. Contains "lock" + allowed_ids=[1,2,7] ---')
r = idx.search({ type: 'contains', field: 'body', value: 'lock' }, { allowedIds: [1, 2, 7], highlights: true })
r.forEach(x => console.log(`  doc=${x.docId} score=${x.score.toFixed(3)}`))

// 8. Phrase
console.log('\n--- 8. Phrase "mutex lock" ---')
r = idx.search({ type: 'phrase', field: 'body', value: 'mutex lock' }, { highlights: true })
r.forEach(x => console.log(`  doc=${x.docId} score=${x.score.toFixed(3)} hl=${JSON.stringify(x.highlights)}`))

// 9. Boolean
console.log('\n--- 9. Boolean must:lock + must_not:clock ---')
r = idx.search({
  type: 'boolean',
  must: [{ type: 'contains', field: 'body', value: 'lock' }],
  must_not: [{ type: 'contains', field: 'body', value: 'clock' }]
}, { highlights: true, limit: 3 })
r.forEach(x => console.log(`  doc=${x.docId} score=${x.score.toFixed(3)}`))

// 10. Fields
console.log('\n--- 10. Contains "lock" + fields ---')
r = idx.search({ type: 'contains', field: 'body', value: 'lock' }, { fields: true, limit: 2 })
r.forEach(x => console.log(`  doc=${x.docId} fields=${JSON.stringify(x.fields)}`))

// 11. Snapshot
console.log('\n--- 11. Snapshot round-trip ---')
const snap = idx.exportSnapshot()
console.log(`  snapshot: ${snap.length} bytes`)
const idx2 = Index.importSnapshot(snap, '/tmp/lucivy_nodejs_e2e_import')
const r2 = idx2.search({ type: 'contains', field: 'body', value: 'lock' })
console.log(`  imported: ${idx2.numDocs} docs, search 'lock': ${r2.length} hits`)

console.log('\n=== NODE.JS: ALL OK ===')
