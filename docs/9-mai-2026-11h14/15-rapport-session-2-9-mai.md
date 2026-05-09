# Rapport session 2 — 9 mai 2026 (suite, ~18h-00h)

## Résumé

Session de finalisation v2 : fix regex, documentation complète, tests CI sharded,
MmapDirectory natif, bench 90K clean, eprintln cleanup.

## Commits (chronologique)

```
ed31709 fix: regex character classes [a-z]+ now work in RegexContinuationQuery
1ed752b docs: v2 documentation — README, CHANGELOG, MIGRATION, binding READMEs
5e79eba docs: sync README-pypi.md with updated Python binding README
1b61444 fix(ci): fix binding test failures
c5439c0 feat(ci): add sharded + playground .luce tests to all bindings
37f34ba feat: auto-clone in bench + rebuild dataset.luce + audit doc
c610f09 perf: MmapDirectory on native + WRITER_HEAP_SIZE 200MB
6a00874 chore: gate [finalize] eprintln behind LUCIVY_VERBOSE + mmap-wasm research doc
57a38cc chore: clean bench output — DAG traces + snippets under LUCIVY_VERBOSE
4d2eaeb docs: add performance baseline to README (90K Linux kernel bench)
b63a42f docs: add substring notice to performance section
795dcef docs: add regex benchmarks to performance table
```

## Fix regex character classes

**Root cause :** dans `regex_contains_via_literal` (single-literal path), après
avoir nourri les bytes du littéral ("program") au DFA, les bytes restants du
token courant (ex: "ming" dans "programming") n'étaient jamais passés au DFA.
Le code sautait directement à `validate_path` qui commence au token suivant.

Pour `[a-z]+`, le séparateur entre tokens tuait le DFA (pas dans [a-z]).
Pour `.+`, les séparateurs matchent `.` donc ça passait par chance.

**Fix :** après le littéral, nourrir `token_text[si + literal_len..]` via
`ord_to_term` avant de vérifier `is_match`.

5 tests ajoutés : char_class, char_class_suffix, word_class, multi_word, custom_data.

## Documentation v2

- **README** principal réécrit (SFX, sharding, distributed search, performance table)
- **CHANGELOG** v2.0.0 (SFX, compat layer, luciole, bindings, WASM, delta sync)
- **MIGRATION.md** créé (v1→v2 : query types, scoring, sharding, WASM)
- **READMEs bindings** Python/Node.js/Emscripten mis à jour (delta sync, distributed search, anchor_start, exact_match)
- **AUTHORS** supprimé, **NOTICE** reformulé ("derived from" au lieu de "forked from")
- **README-pypi.md** synced

## CI

- Fix Python : `from_snapshot` → `import_snapshot`
- Fix Node.js : retrait arg `'english'` (stemmer supprimé)
- Fix C++ : retrait arg `'english'`, ajout param `shards` à `lucivy_create`
- Ajout tests sharded (2 shards, 20 docs, snapshot round-trip, shard_versions)
- Ajout tests playground .luce (import 952 docs, search, regex)
- Retrait tests "uncommitted export should throw" (obsolètes avec lazy commit)
- **CI verte** sur tous les jobs

## Performance — MmapDirectory + WRITER_HEAP_SIZE

### Changements
- `NativeDirectory` type alias : `MmapDirectory` natif, `StdFsDirectory` WASM
- `FsShardStorage` utilise `NativeDirectory` (zero-copy reads via mmap)
- `BlobDirectory` utilise `NativeDirectory` pour le cache local
- `WRITER_HEAP_SIZE` : 50MB → 200MB natif (15MB WASM inchangé)
- Feature `mmap` ajoutée à la dépendance ld-lucivy de lucivy-core

### Résultats bench (90K docs Linux kernel)

```
AVANT (debug, StdFsDirectory, 50MB heap):
  Single:  733s | RAM: 20GB+ swap 38GB

APRES (release, MmapDirectory, 200MB heap):
  Single:  50s  | RAM: 14GB, pas de swap
  Speedup: 14.7x
```

### Tableau de recherche (release, 90K docs, 3-run avg)

| Query                          | 1 shard | 4 shards |
|--------------------------------|---------|----------|
| contains 'mutex_lock'          | 261ms   | 137ms    |
| contains 'function'            | 127ms   | 131ms    |
| contains_split 'struct device' | 338ms   | 347ms    |
| contains 'sched'               | 119ms   | 128ms    |
| startsWith 'sched'             | 185ms   | 178ms    |
| fuzzy 'schdule' (d=1)          | 559ms   | 318ms    |
| regex 'mutex.*lock'            | -       | 373ms    |
| regex 'kmalloc.*sizeof'        | -       | 442ms    |
| contains 'drivers' (path)      | 7ms     | 7ms      |

### Regex via Python (4 shards, 90K docs)

| Pattern                  | Hits | Time    |
|--------------------------|------|---------|
| `mutex.*lock`            | 20   | 373ms   |
| `sched.*init`            | 20   | 360ms   |
| `kmalloc.*sizeof`        | 20   | 442ms   |
| `spin_lock[a-z_]*`       | 20   | 597ms   |
| `config_[a-z]+`          | 20   | 1533ms  |
| `device_[a-z]+`          | 20   | 2580ms  |

Temps corrélé à la fréquence du littéral extrait (plus c'est commun, plus de candidats).

## Cleanup eprintln

- `[finalize]` logs dans `index_writer.rs` : sous `diag::is_verbose()` (LUCIVY_VERBOSE=1)
- DAG merge summary dans `segment_updater_actor.rs` : sous `is_verbose()`
- Traces DAG et snippets dans bench_sharding : sous `LUCIVY_VERBOSE`
- Tableau de résultats bench déplacé dans la section Summary (plus de pollution)

## Autres

- Bench auto-clone : `BENCH_DATASET` accepte des URLs git (clone automatique, cache /tmp)
- dataset.luce rebuild (972 docs, code v2 à jour)
- Audit pre-publish (playground standalone OK, luciole prêt, eprintln inventaire)
- Doc recherche mmap WASM (ncruces/go-sqlite3 approach, pour plus tard)

## Warnings compilation

- 134 warnings compilation (70 missing_docs, 17 unused vars, 10 unused imports, 18 dead code)
- 273 warnings clippy + 4 erreurs (loop never loops dans term_dictionary.rs)
- Fichiers de trace : `13-compilation-warnings.txt`, `14-clippy-warnings.txt`

## Prochaine session

1. **Clippy cleanup** — 4 erreurs + 273 warnings (plan dans 16-plan-warnings.md)
2. **Compilation warnings** — 134 warnings (unused, missing_docs, dead_code)
3. **Bump versions + publish** — tag v2.0.0, cargo/maturin/npm publish
