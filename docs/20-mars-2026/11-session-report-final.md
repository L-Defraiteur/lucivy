# Doc 11 — Rapport de session final

Date : 20 mars 2026

## Résultat headline

**5/5 ground truth MATCH sur bench 5K linux kernel.** Zéro diff.

| Terme | Avant session | Après session |
|-------|--------------|---------------|
| mutex | 610/610 ✓ | 610/610 ✓ |
| lock | 2454/2455 ✗ | **2455/2455 ✓** |
| function | 1285/1305 ✗ | **1305/1305 ✓** |
| printk | 178/178 ✓ | 178/178 ✓ |
| sched | 420/424 ✗ | **424/424 ✓** |

## Ce qui a été fait

### Architecture — tout en DAG
- Pipeline commit/merge redesigné — state machine supprimée (~780 lignes)
- Merge en DAG complet (postings ∥ store ∥ fast_fields → sfx → close)
- SFX merge en sous-DAG (collect → build_fst ∥ copy_gapmap ∥ merge_sfxpost)
- Scatter DAG avec résultats nommés (index opening, SFX build)
- Inline DAG sur scheduler thread (évite deadlocks de thread pool starvation)
- Zéro submit_task dans le codebase

### Bugs corrigés
- merge_sfxpost `if` → `else if` (données perdues après merge)
- step() propage les erreurs au lieu de les avaler
- Double save_metas/GC (segment mergé écrasé puis supprimé)
- PortValue::take() panic sur fan-out (était silent None → SIGSEGV)
- DiagBus unbounded channel (events droppés pour gros volumes)

### Fonctionnalités
- Stemming supprimé (inutile pour code search)
- Cross-token continuation (hybrid walk 1 + gapmap + walk 2)
- Expansion uppercase (multi-token pour ALL_CAPS splits)
- DiagBus avec events souscriptibles (SearchMatch, SearchComplete, TokenCaptured)
- trace_search() read-only sans deadlock
- SubstringAutomaton pour DFA continuation (prêt, pas encore utilisé)

### Observabilité
- DiagBus global avec filtres (All, Sfx, SfxTerm, Tokenization, Merge)
- Zero overhead quand pas de subscribers (atomic bool)
- trace_search lit directement les bytes (pas de build_resolver)

## Commits (12 commits)

```
d108c42 refactor: single DAG pipeline, remove stemming, fix merge bugs
6729371 feat: wire sfx_dag into merge pipeline
a0b4e75 feat: parallel index opening + parallel SFX build
38b1019 feat: merge as full DAG — postings ∥ store ∥ fast_fields
d3cbb4e refactor: replace all submit_task with DAGs, delete MergeState
7d932a4 fix: PortValue::take() panics on fan-out instead of silent None
f7c8135 feat: scatter DAG with named results, zero submit_task
29c2b80 docs: session report — tout en DAG
4b36422 feat: cross-token contains + DiagBus + inline DAG
cb2ec7a docs: continuation DFA design + fix DiagBus unbounded channel
0e796e4 feat: cross-token continuation — 5/5 ground truth MATCH
```

## Tests
- ld-lucivy : 1199 pass, 0 fail
- luciole : 132+ pass, 0 fail
- lucivy-core : 83+ pass, 0 fail

## Prochaines étapes

1. Flag global diagnostics on/off (router eprintln DAG vers DiagBus)
2. Fix CamelCaseSplitFilter pour ne pas splitter ALL_CAPS (réduire besoin continuation)
3. Continuation pour multi-token queries (chaque token passe par continuation si activé)
4. Bench perf: mesurer overhead continuation vs sans
5. Bench 20K/90K pour stress test
6. Nettoyer warnings (~80)
