# Doc 12 — 5/5 MATCH sur 90K : rapport final

Date : 20 mars 2026
Branche : `feature/luciole-dag`

## Résultat

**90 000 documents du kernel Linux. 5 termes. Zéro diff.**

```
mutex:     8850/ 8850  ✓ MATCH
lock:     40389/40389  ✓ MATCH
function: 21525/21525  ✓ MATCH
printk:    4681/ 4681  ✓ MATCH
sched:     8945/ 8945  ✓ MATCH
```

Ground truth : `text.to_lowercase().contains(term)` sur chaque doc stocké.
Search : SFX suffix FST walk + sfxpost resolution + cross-token continuation.
Vérification : itération brute-force de 90K docs × 5 termes.

## Bugs trouvés et corrigés dans cette session

### 1. merge_sfxpost `if` → `else if`
Le check d'erreur Phase 3 s'exécutait TOUJOURS, pas seulement quand sfxpost absent.
Causait : `Done(None)` → `end_merge(ids, None)` → données perdues après merge.

### 2. step() silent error swallowing
`MergeState::step()` avalait les erreurs avec `warn!` → `Done(None)`.
Fix : propage `Err` au caller.

### 3. Double save_metas/GC (pipeline redesign)
`drain_all_merges()` faisait save+gc, puis le commit DAG refaisait save+gc.
Le deuxième `segment_manager.commit()` écrasait le segment mergé.
Fix : suppression complète de la state machine (780 lignes), un seul chemin via DAG.

### 4. PortValue::take() silent None on fan-out
`Arc::try_unwrap` échouait silencieusement → downstream SIGSEGV.
Fix : panic avec message clair.

### 5. CamelCaseSplitFilter splittait ALL_CAPS (DIFF=20 pour "function")
`FUNCTION` → `FUNC` + `TION`. Le tokenizer ne devrait pas splitter les mots tout en majuscules.
Fix : `find_boundaries` ne split que sur les vraies frontières camelCase (lower→UPPER, UPPER→UPPER+lower).

### 6. Pas de cross-token continuation (DIFF=4 pour "sched")
Quand un query span une frontière de tokens (ex: "sched" dans "SCHE"+"DULER"),
le search ne le trouvait pas.
Fix : continuation hybride — walk 1 détecte les candidats partiels, walk 2 via gapmap.

### 7. Parent list u8 overflow (DIFF=663 pour "lock")
Le suffix FST encodait le nombre de parents par suffix en `u8` → max 255.
"lock" a 300+ parents (clock, block, unlock, deadlock, ...) → overflow silencieux.
Fix : `u8` → `u16` (1 ligne).

## Architecture — tout en DAG

```
commit_dag
  └── merge_dag (postings ∥ store ∥ fast_fields)
        └── sfx_dag (collect → build_fst ∥ copy_gapmap ∥ merge_sfxpost → validate → write)

scatter_dag (index opening, SFX build)
search_dag (drain → flush → build_weight → shard_N ∥ → merge_results)
```

- Zéro submit_task
- Inline DAG sur scheduler thread (anti-deadlock)
- Scatter DAG avec résultats nommés

## Observabilité

- DiagBus : events souscriptibles (SearchMatch, SearchComplete, TokenCaptured)
- trace_search() : diagnostic read-only sans deadlock
- set_verbose(false) : coupe les eprintln DAG
- SubstringAutomaton : prêt pour continuation DFA future

## Stats

- 1200 tests pass, 0 fail
- ~20 commits dans cette session
- ~1500 lignes supprimées (state machine, stemming, dead code)
- ~2000 lignes ajoutées (DAGs, continuation, DiagBus, diagnostics)

## Prochaines étapes

1. Bench perf propre (sans ground truth verification, sans DiagBus)
2. Cleanup warnings (~80 restants, surtout code ngram mort)
3. Adaptation des bindings (CXX, WASM, Node.js, Python, C++)
   - stemming supprimé → adapter les configs
   - MergeState supprimé → vérifier les paths de merge
   - SfxTokenInterceptor unique → vérifier les bindings qui avaient double tokenization
4. Tests WASM emscripten multi-thread
5. Optimisation continuation (cache les walks, early exit)
6. Fix lock file (close() cascade)

## Optimisation future : continuation via stored text

La continuation hybride actuelle fait N walks successifs pour traverser N frontières
de tokens. Chaque walk est un `prefix_walk_si0(remaining)` + join. À depth 3+,
ça multiplie les lookups FST.

Optimisation possible :
- **Depth 1-2** : continuation hybride (walk + gapmap + walk). Rapide, pas besoin du store.
- **Depth 3+** : fallback sur le texte stocké (stored doc). Le `byte_from` est connu
  dès le walk 1 (dans le sfxpost entry). Il suffit de lire `text[byte_from..byte_from+query_len]`
  et comparer avec le query. Une seule lecture mmap, O(1).

Avantage : on peut monter à depth illimité sans coût supplémentaire. À partir du moment
où on a intersecté au moins 2-3 walks (suffisamment sélectif), les candidats restants sont
peu nombreux. La vérification sur le store est quasi-gratuite (mmap déjà chargé).

```
depth 1-2: walk hybride (rapide, sélectif)
depth 3+:  store.get(doc_id) → text[byte_from..].starts_with(remaining)
           → O(1) par candidat, pas de walk FST supplémentaire
```

Pour l'instant la depth 3+ est rare (le CamelCaseSplit ne split plus les ALL_CAPS).
À implémenter quand un bench montre que la continuation est un bottleneck.

## Nettoyage post-validation

L'expansion uppercase (re-tokeniser la query en majuscules → multi-token search)
a été supprimée. C'était un fix intermédiaire, rendu redondant par :
- Le fix CamelCaseSplitFilter (ALL_CAPS ne sont plus splittés)
- La continuation hybride (couvre les edge cases restants)

Validé : **5/5 MATCH sur 90K** sans l'expansion uppercase. -55 lignes de code.

## Performances (avec DiagBus + eprintln actifs)

Contains search sur 90K docs kernel Linux :
- **~650ms** par query substring (avec overhead debug)
- Sans DiagBus/eprintln, sera encore plus rapide
