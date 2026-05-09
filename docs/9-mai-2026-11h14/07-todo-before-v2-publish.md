# TODO avant publication v2

## Bindings

### Emscripten
- [x] `export_snapshot` — implémenté
- [x] `export_sharded_delta` + `apply_sharded_delta` — implémentés
- [x] `shard_versions` — implémenté
- [x] `export_stats` + `search_with_global_stats` — implémentés
- [ ] Tester snapshot round-trip (export natif → import emscripten)

### Python
- [x] Prêt — parité complète

### Node.js
- [x] Prêt — parité complète

### C++ (cxx bridge)
- [x] Prêt — parité complète

### Parité vérifiée (100%)
export_stats, search_with_global_stats, export_snapshot, import_snapshot,
shard_versions, export_sharded_delta, apply_sharded_delta, highlights,
close, search — tout dispo sur les 4 bindings.

## Fait cette session

- [x] Deadlock WASM (deferred I/O + elimination thread::spawn)
- [x] Compat layer v2 (term/fuzzy/regex/phrase/parse/phrase_prefix → contains)
- [x] startsWith unifié (anchor_start)
- [x] exact_match paramètre
- [x] Supprimé wasm-bindgen binding
- [x] Default balance_weight=1.0 (round-robin)
- [x] 9 tests merge-timing → ignored (1200 pass, 0 fail)
- [x] CLAUDE.md réécrit
- [x] Feature inventory v2
- [x] Draft README v2
- [x] Scores négatifs — voulu, documenté

## Reste à faire

### Playground
- **Debug** (existant, garder tel quel) : `serve.mjs` + eval polling + POST /log
- **Standalone** (à créer) : version publique
  - [ ] HTML auto-contenu (ou petit dossier statique : index.html + pkg/)
  - [ ] Pas d'eval polling ni de POST /log — logs via `console.log`
  - [ ] Service worker pour injecter les headers COOP/COEP (SharedArrayBuffer)
  - [ ] Servable par n'importe quoi (GitHub Pages, nginx, python -m http.server)
  - [ ] Mode debug activable via query param (`?debug=1`)

### UI playground
- [ ] Mettre à jour les query types affichés
- [ ] Exposer `anchor_start`, `exact_match`, `distance` comme options sur contains
- [ ] Afficher les highlights dans les résultats

### luciole — package standalone
- [ ] Vérifier autonomie Cargo.toml (pas de dépendance vers ld-lucivy)
- [ ] README luciole — actor system, DAG, scheduler, WASM-safe
- [ ] Exemples minimaux (actor ping-pong, DAG simple, pipe_to)
- [ ] Licence MIT

### Documentation
- [ ] README v2 final (draft dans 06-draft-readme-v2.md)
- [ ] CHANGELOG v1 → v2
- [ ] MIGRATION.md

### Tests
- [ ] Tests E2E compat layer
- [ ] Test cross-token exact_match
- [ ] Bench de non-régression (37 ground truth checks)

### Qualité
- [ ] Audit `thread::spawn` — grep CI
- [ ] Retirer les eprintln de diagnostic (ou feature flag)

### Publish
- [ ] Bumper les versions : lucivy-core 0.2.0, lucivy (PyPI) 2.0.0, lucivy (npm) 2.0.0
- [ ] Tag git v2.0.0
- [ ] `cargo publish` lucivy-core + luciole
- [ ] `maturin publish` (PyPI)
- [ ] `npm publish` (npm)
- [ ] Branche v1 pour maintenance
- [ ] Annonce (GitHub release notes)
