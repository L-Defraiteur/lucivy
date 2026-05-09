# Audit pre-publish — 9 mai 2026

## 1. Playground standalone

**Statut : DEJA PRET**

- `coi-serviceworker.js` injecte les headers COOP/COEP pour SharedArrayBuffer
- Auto-detect du serveur debug (`serve.mjs`) — si pas dispo, tourne en standalone
- Servable par GitHub Pages, nginx, `python -m http.server`, etc.
- Pas de modif nécessaire

## 2. Playground UI — options query

**Statut : PARTIEL**

| Option | Exposée ? | Notes |
|--------|-----------|-------|
| Query types | Oui | substring, multi-word, prefix, regex |
| distance (fuzzy) | Oui | Input number, 0-3 |
| highlights | Oui | Checkbox + rendu `<mark>` dans les résultats |
| Extension filter | Oui | Dropdown (.rs, .js, .ts, .py, etc.) |
| anchor_start | NON | Pas d'UI — à ajouter si souhaité |
| exact_match | NON | Pas d'UI — à ajouter si souhaité |
| strict_separators | Caché | `display:none` |

**Conclusion :** distance et highlights sont déjà la. anchor_start et exact_match sont les seuls manquants — pas bloquant pour la v2, optionnel.

## 3. luciole — standalone

**Statut : PRET pour publish**

- **Cargo.toml** : seule dépendance `flume = "0.11"`, aucune dépendance vers ld-lucivy
- **README.md** : 131 lignes, complet (quick start, features, WASM, tests)
- **LICENSE** : fichier MIT présent
- **Exemples** : aucun dossier `examples/` — pas bloquant mais nice-to-have
- 154 tests passent

## 4. eprintln — audit

**595 appels au total dans 45 fichiers.**

### Répartition :

| Catégorie | Nombre approx | Action |
|-----------|---------------|--------|
| Benches (bench_*.rs) | ~50 | OK — sortie normale pour benchmarks |
| Tests (test_*.rs, tests/) | ~300 | OK — sortie de diagnostic test |
| Examples (acid_postgres, etc.) | ~100 | OK — démo/validation |
| **Lib src/** | ~80 | A NETTOYER — eprintln dans du code de prod |
| **lucivy_core/src/** | ~40 | A NETTOYER — handle, sharded_handle, directory |
| **luciole/src/** | ~15 | A NETTOYER — scheduler, mailbox, reply |
| **bindings/** | ~10 | A NETTOYER — emscripten init |

### Les plus bruyants en lib (à retirer ou gater) :

- `src/indexer/segment_updater_actor.rs` — `[finalize]` logs à chaque commit
- `src/indexer/merger.rs` — progression merge
- `lucivy_core/src/sharded_handle.rs` — sharding diagnostics
- `lucivy_core/src/handle.rs` — lifecycle
- `luciole/src/scheduler.rs` — thread pool warnings

**Aucun guard `LUCIVY_DEBUG` env var** — tout est print inconditionnel.

**Recommandation :** Les `[finalize]` sont les plus visibles (on les voit dans TOUS les tests CI). Priorité #1.

## 5. Benchmarks

### Existants :

| Script | Dataset | Ce qu'il teste |
|--------|---------|----------------|
| `lucivy_core/benches/bench_sharding.rs` | Linux kernel 90K docs | single vs 4-shard, scoring consistency |
| `lucivy_core/benches/bench_vs_tantivy.rs` | Linux kernel 90K docs | lucivy vs tantivy head-to-head |
| `lucivy_core/benches/bench_contains.rs` | rag3db clone | substring matching, exporte .luce |
| `benches/agg_bench.rs` | wiki | aggregation |
| `benches/index-bench.rs` | wiki | indexation speed |

### Comment lancer le bench 90K :

```bash
# 1. Build l'index (Linux kernel source requis localement)
BENCH_MODE=SINGLE MAX_DOCS=90000 cargo test -p lucivy-core --test bench_sharding -- --nocapture > /tmp/bench_sharding.txt 2>&1

# 2. Comparer avec tantivy
cargo test -p lucivy-core --test bench_vs_tantivy -- --nocapture > /tmp/bench_vs_tantivy.txt 2>&1
```

**Note :** ces benchs nécessitent un clone local du kernel Linux.

### Pas de bench dans bindings/python

Pas de script Python benchmark trouvé. Le bench 90K est 100% Rust.

## 6. dataset.luce

- **Taille :** 56 MB
- **Dernière modif :** 25 avril 2026
- **Contenu :** source code lucivy (952 docs)
- **Schema :** `path` (text), `content` (text), `extension` (text)
- **Script de rebuild :** `playground/build_dataset.py`

```bash
cd playground
python build_dataset.py                    # 1 shard (default)
python build_dataset.py --shards 4         # 4 shards
python build_dataset.py --source /path/to  # repo custom
```

**Le dataset date du 25 avril** — avant les changements v2 de cette session (9 mai). Le code source indexé n'est pas à jour.

## Résumé des actions

| # | Action | Priorité | Effort |
|---|--------|----------|--------|
| 1 | Retirer/gater les eprintln `[finalize]` | Haute | Rapide |
| 2 | Rebuild dataset.luce avec le code à jour | Moyenne | Rapide |
| 3 | Lancer bench 90K (si kernel dispo localement) | Moyenne | ~10 min |
| 4 | Ajouter anchor_start + exact_match au playground | Basse | Rapide |
| 5 | Ajouter luciole/examples/ | Basse | Moyen |
