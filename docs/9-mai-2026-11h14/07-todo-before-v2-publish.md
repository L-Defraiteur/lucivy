# TODO avant publication v2

## Bindings

### Emscripten
- [ ] `export_snapshot` — actuellement TODO/not supported
- [ ] `export_sharded_delta` + `apply_sharded_delta` — pas exposés
- [ ] `shard_versions` — pas exposé (nécessaire pour delta sync)
- [ ] Tester snapshot round-trip (export natif → import emscripten)

### Python
- [x] Prêt — tous les endpoints exposés

### Node.js
- [x] Prêt — tous les endpoints exposés

### C++ (cxx bridge)
- [x] Prêt — tous les endpoints exposés

## Playground

### Deux versions
- **Debug** (existant, garder tel quel) : `serve.mjs` + eval polling + POST /log + diag.log. Pour le développement et le diagnostic.
- **Standalone** (à créer) : version publique, pas de serveur Node requis.
  - [ ] HTML auto-contenu (ou petit dossier statique : index.html + pkg/)
  - [ ] Pas d'eval polling ni de POST /log — logs via `console.log`
  - [ ] Service worker pour injecter les headers COOP/COEP (SharedArrayBuffer)
    - Nécessaire car GitHub Pages / serveurs statiques ne les envoient pas
    - Le SW intercepte les requêtes et ajoute les headers
  - [ ] Servable par n'importe quoi (GitHub Pages, nginx, python -m http.server)
  - [ ] Mode debug activable via query param (`?debug=1` → active eval/logs distants si serveur dispo)

### UI playground
- [ ] Mettre à jour les query types affichés (retirer les deprecated ou les griser)
- [ ] Exposer `anchor_start`, `exact_match`, `distance` comme options sur contains
- [ ] Afficher les highlights dans les résultats
- [ ] Afficher le score (et expliquer les négatifs pour fuzzy = tiers par miss count)

## luciole — package standalone

### Publier sur crates.io
- [ ] `luciole` comme crate séparé (déjà dans son propre dossier luciole/)
- [ ] Vérifier que le Cargo.toml de luciole est autonome (pas de dépendance vers ld-lucivy)
- [ ] README luciole — actor system, DAG, scheduler, WASM-safe
- [ ] Exemples minimaux (actor ping-pong, DAG simple, pipe_to)
- [ ] Licence (MIT, comme le reste)

### Ce que luciole expose
- Actor trait + GenericActor + handlers
- Scheduler (persistent threads, WASM compatible)
- DAG (construction, exécution, checkpoint, undo)
- StreamDag (pipeline streaming)
- Pool (scatter/gather)
- pipe_to / collect_replies_to / task_pipe_to
- WaitGraph (diagnostics)
- BranchNode, GateNode, MergeNode, ScatterDAG

## Scoring

- [x] Scores négatifs en fuzzy — voulu (tiers par miss count, BM25 départage dans le tier). L'ordre est correct, on documente juste.

## Documentation

- [ ] README v2 final (draft dans 06-draft-readme-v2.md)
- [ ] CHANGELOG v1 → v2
- [ ] MIGRATION.md (pour les utilisateurs v1)
  - term → contains + anchor_start + exact_match
  - fuzzy → contains + distance
  - regex → contains + regex
  - startsWith → contains + anchor_start
  - phrase → contains
  - sfx_enabled retiré (toujours true)
- [ ] Mettre à jour CLAUDE.md (retirer les infos obsolètes : wasm-bindgen, sfx optional, etc.)

## Tests

- [ ] Tests E2E pour le compat layer (chaque ancien type retourne des résultats)
- [ ] Test cross-token exact_match ("rag3weaver" matche, "rag3weave" ne matche pas)
- [ ] Test anchor_start cross-token ("rag3weaver" avec startsWith)
- [ ] Bench de non-régression (les 37 ground truth checks)

## Qualité

- [ ] Audit `thread::spawn` — grep CI pour empêcher les régressions
- [ ] Les 9 tests pre-existing failures — investiguer ou documenter
- [ ] Retirer les eprintln de diagnostic (ou les garder derrière un feature flag)
- [ ] Nettoyer les `#[allow(dead_code)]` inutiles

## Publish

- [ ] Bumper les versions : lucivy-core 0.2.0, lucivy (PyPI) 2.0.0, lucivy (npm) 2.0.0
- [ ] Tag git v2.0.0
- [ ] `cargo publish` lucivy-core + luciole
- [ ] `maturin publish` (PyPI)
- [ ] `npm publish` (npm)
- [ ] Branche v1 pour maintenance
- [ ] Annonce (GitHub release notes)
