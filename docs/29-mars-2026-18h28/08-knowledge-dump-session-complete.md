# 08 — Knowledge dump : session 29-31 mars 2026

## Ce qui a été fait

### 1. Highlights fuzzy byte-exact
- L'ancien walk fspan→lspan utilisait des tokens entiers → highlights trop larges
- Nouveau : table `content_byte_starts` par token, mapping concat offset → content byte
- L'ancre `first_bf` est le suffix match start, pas le token start
- Fix : `token_start = first_bf - first_si` (si propagé dans tout le pipeline)
- Le merge des highlights (gap ≤ 1 byte) supprimé → dedup simple

### 2. Merger écrit tous les registry files
- Les 3 chemins de merge (merger.rs N-way, merger.rs fallback, sfx_dag.rs
  WriteSfxNode) utilisaient des méthodes legacy (write_sfxpost, write_posmap)
  qui n'écrivaient PAS les registry files
- Fix : tout passe par write_custom_index + reconstruction de posmap/bytemap/
  termtexts depuis sfxpost dans sfx_dag.rs
- Doc 15 : audit complet des 5 chemins d'écriture SFX

### 3. Fallbacks silencieux supprimés
- 5 fallbacks ord_to_term sur le term dict tantivy → erreurs explicites
- 1 fallback posmap "accept conservatively" → erreur
- Le .luce pré-construit (sans termtexts) crash maintenant → rebuild requis

### 4. Anchored sliding window fuzzy
- Au lieu de tester TOUTES les positions dans le concat (~50),
  ancrer autour de `fp_concat_start + first_si - query_positions[first_tri_idx]`
- Window = ±distance positions = ~3 positions au lieu de ~50
- ~90% réduction des DFA transitions par candidat

### 5. Trigram proven skip
- Quand TOUS les trigrams matchent avec byte span consistent,
  le pigeonhole garantit le match → skip DFA entièrement
- Flag `trigram_proven` calculé dans intersect_trigrams_with_threshold

### 6. Pipeline sélectivité (literal_pipeline.rs)
- 4 briques composables : fst_candidates, resolve_candidates,
  cross_token_falling_walk, resolve_chains
- Phase A : estimer sélectivité de chaque trigram via FST walk (quasi gratuit)
- Phase B : résoudre les `threshold` trigrams exacts les plus rares sans filtre,
  puis filtrer les restants par l'UNION de leurs doc sets
- Le filtre est union (pas intersection) car avec d>0 certains trigrams
  couvrent le typo et ont 0 matches

### 7. ByteMap DFA pré-filtre
- Module `dfa_byte_filter.rs` : `can_token_advance_dfa()` vérifie si
  ANY byte du token peut avancer le DFA via le bitmap 256 bits
- Branché dans `validate_path()` (regex cross-token validation)
- Pour les patterns restrictifs (`[a-z]+`), skip les tokens incompatibles
  sans lancer le DFA

### 8. Regex gap-by-gap validation
- `regex_gap_analyzer.rs` : parse le regex via `regex-syntax` HIR
- Classify chaque gap entre littéraux en 3 tiers :
  - AcceptAnything (`.*`, `.+`) → skip (ordre déjà vérifié par intersection)
  - ByteRangeCheck (`[a-z]+`, `\w+`, `\d*`) → check ByteMap O(1)/token
  - DfaValidation → validate_path complet
- Pour `rag3.*ver` : le `.*` est gratuit, pas de validate_path
- Pour `rag3[a-z]+ver` : ByteMap check + SepMap pour les séparateurs
- Fallback DFA quand tokens adjacents (pas de tokens intermédiaires à checker)

### 9. SepMap — bitmap séparateurs par ordinal
- Nouveau fichier `.sepmap` : 256 bits par ordinal, quels bytes de séparateur
  observés APRÈS ce token (tous docs confondus)
- Bit 0x00 = contiguous flag (gap=0 observé)
- Construit dans SfxCollector, remappé par ordinal, OR-merge pour merge
- Remplace le GapMap read per-doc dans le regex ByteRangeCheck
- `has_contiguous()`, `sep_bytes_in_ranges()`, `only_contiguous()`

### 10. Skip DFA pour d>=3
- Le DFA Levenshtein d=3 a un état space énorme → construction très lente
- Early return avant la construction du DFA quand `distance >= 3`
- Les candidats trigram-intersectés sont acceptés directement

### 11. Multi-token fuzzy
- `generate_ngrams` skip les n-grams qui chevauchent des séparateurs
  non-alphanumériques (pas seulement les espaces)
- Dedup des entries dans `intersect_trigrams_with_threshold` — les doublons
  (cross-token + single-token SFX paths) cassaient le greedy chain builder

## Scripts et commandes utiles

### Test ground truth fuzzy
```bash
cargo test -p lucivy-core --test test_fuzzy_ground_truth -- --nocapture > /tmp/fuzzy_gt.txt 2>&1
# Valide recall, precision, highlights pour "rag3weaver" et "rak3weaver" d=1
```

### Test ground truth regex
```bash
cargo test -p lucivy-core --test test_regex_ground_truth -- --nocapture > /tmp/regex_gt.txt 2>&1
# 7 patterns regex, timing + recall
```

### Test multi-segment fuzzy
```bash
cargo test -p lucivy-core --test test_two_fields test_multi_segment_fuzzy -- --nocapture
# Teste "use rak3weaver for search" d=1 sur un doc unique
```

### Test sur .luce existant
```bash
cargo test -p lucivy-core --test test_luce_roundtrip -- --nocapture > /tmp/luce_test.txt 2>&1
```

### Rebuild .luce
```bash
cd bindings/python && maturin develop --release
cd ../.. && python3 playground/build_dataset.py
```

### Build WASM
```bash
bash bindings/emscripten/build.sh
# Servir : node playground/serve.mjs → http://localhost:9877/
```

## Pièges et leçons

### 1. first_bf ≠ token start
`first_bf` = `token_start + si` (byte du suffix match, pas du token).
Pour calculer le token start : `first_bf - first_si`. Le `si` est dans
`ParentEntry` et doit être propagé à travers LiteralMatch, MatchesByDoc,
intersect_trigrams_with_threshold.

### 2. Le merger ne passait PAS par le registry
Les 3 chemins de merge utilisaient des méthodes legacy (write_sfxpost,
write_posmap, write_bytemap) qui n'écrivent pas les fichiers du registry
(termtexts, gapmap séparé, sibling séparé, sepmap). Le segment_reader
charge via le registry → fichiers invisibles → search cassé.

### 3. Les fallbacks silencieux masquent les bugs
Le fallback `ord_to_term` sur le term dict tantivy retournait le mauvais
token (ordinal mismatch) SANS erreur. Le fallback posmap "accept
conservatively" retournait des faux positifs. Remplacés par des erreurs.

### 4. Le DFA Levenshtein d=3 est inutilisable
Construction très lente (secondes). Skip entièrement pour d>=3 et accepter
les candidats trigram-intersectés directement.

### 5. Les doublons d'entries cassent le greedy chain builder
`find_literal` peut retourner le même match 2 fois (cross-token path +
single-token path). Le greedy chain builder s'arrête quand le tri_index
ne croît plus → les doublons reset la chain. Fix : dedup par (tri_idx, bf).

### 6. Le pipeline sélectivité ne peut PAS filtrer par intersection pour d>0
Avec d>0, certains trigrams couvrent le typo et ont 0 matches. Filtrer
par intersection élimine les docs corrects. Fix : utiliser l'UNION des
doc sets des `threshold` trigrams exacts les plus rares.

### 7. Le ByteMap gap check est insuffisant pour tokens adjacents
`rag3[a-b]+ver` matchait faussement "rag3weaver" car le ByteRangeCheck
n'avait pas de tokens intermédiaires à checker (tokens adjacents/même token).
Fix : retourner None (inconclusive) et fallback DFA.

### 8. Le GapMap read per-doc est cher
`validate_gap_bytemap` avec GapMap : 63ms pour 64 docs.
Avec SepMap (bitmap global par ordinal) : 24ms. Le SepMap est un pré-filtre
O(1) ; le GapMap reste nécessaire pour la vérification per-doc exacte.

### 9. `\w` en regex-syntax est Unicode
`regex-syntax` parse `\w` comme `Class::Unicode` avec des ranges non-ASCII.
`extract_byte_ranges_from_class` retournait None → fallback DFA.
Fix : clamp les ranges Unicode à ASCII (0-127).

### 10. generate_ngrams doit skip les n-grams cross-séparateur
Pour "use rak3weaver", les trigrams "e r", " ra" n'existent pas dans le SFX.
Fix : skip les n-grams contenant des chars non-alphanumériques.

## Branches et état

| Branche | Contenu | État |
|---------|---------|------|
| `feature/fuzzy-via-literal-resolve` | Tout ce qui est décrit ici | active, HEAD |

## Fichiers clés modifiés cette session

### Nouveau
- `src/query/phrase_query/dfa_byte_filter.rs` — pré-filtre DFA via ByteMap
- `src/query/phrase_query/literal_pipeline.rs` — 4 briques composables
- `src/query/phrase_query/regex_gap_analyzer.rs` — parse regex + gap classification
- `src/suffix_fst/sepmap.rs` — SepMapWriter/Reader + SepMapIndex
- `lucivy_core/tests/test_fuzzy_ground_truth.rs` — ground truth fuzzy
- `lucivy_core/tests/test_regex_ground_truth.rs` — ground truth regex

### Modifié
- `src/query/phrase_query/regex_continuation_query.rs` — pipeline sélectivité,
  anchored window, proven skip, gap-by-gap, multi-token fuzzy
- `src/query/phrase_query/literal_resolve.rs` — si propagation, dedup entries,
  validate_path + bytemap, intersect_trigrams types
- `src/query/phrase_query/suffix_contains_query.rs` — termtexts required
- `src/query/phrase_query/mod.rs` — exports nouveaux modules
- `src/indexer/merger.rs` — write_custom_index pour tous les registry files
- `src/indexer/sfx_dag.rs` — WriteSfxNode reconstruit posmap/bytemap/termtexts
- `src/indexer/sfx_merge.rs` — write_custom_index pour sfxpost
- `src/indexer/segment_writer.rs` — cleanup diag
- `src/suffix_fst/collector.rs` — SepMap construction + remapping ordinals
- `src/suffix_fst/index_registry.rs` — SepMapIndex + sepmap_data dans SfxBuildContext
- `src/suffix_fst/mod.rs` — exports sepmap
- `src/index/segment_reader.rs` — cleanup diag
- `lucivy_core/src/query.rs` — cleanup diag
- `docs/arsenal.md` — mis à jour

### Docs
- `docs/28-mars-2026-14h54/14-fix-fuzzy-highlight-byte-exact.md`
- `docs/28-mars-2026-14h54/15-audit-chemins-indexation-registre.md`
- `docs/29-mars-2026-18h28/01-optimisations-fuzzy-regex.md`
- `docs/29-mars-2026-18h28/02-arsenal-sous-utilise-fuzzy-regex.md`
- `docs/29-mars-2026-18h28/03-design-find-literal-pipeline.md`
- `docs/29-mars-2026-18h28/04-design-regex-syntax-bytemap-gaps.md`
- `docs/29-mars-2026-18h28/05-design-sep-bytemap-index.md`
- `docs/29-mars-2026-18h28/06-design-multi-segment-fuzzy.md`
- `docs/29-mars-2026-18h28/07-rapport-session-fuzzy-regex-optims.md`
- `docs/29-mars-2026-18h28/08-knowledge-dump-session-complete.md` (ce fichier)

## Prochaines étapes

1. **Fix multi-token d=0** — `suffix_contains_multi_token_impl_pub` retourne
   0 résultats sur le .luce. Bug pré-existant à investiguer.
2. **Optimiser le pipeline sélectivité** — le surcoût du pipeline
   (fst_candidates + cross_token_falling_walk pour chaque trigram) peut être
   supérieur au gain du filtrage sur les petits index.
3. **ByteRangeCheck avec constraint de longueur** — `{6}`, `{3,5}` pas encore
   implémentés dans le gap analyzer.
4. **Unifier les chemins de merge via le registry** — les 3 chemins reconstruisent
   chacun les fichiers manuellement. Idéalement utiliser `SfxIndexFile::merge()`.
5. **Bench perf sur 90K docs** — re-mesurer après tous les changements.
6. **Supprimer les méthodes legacy du serializer** — write_sfxpost, write_posmap,
   write_bytemap ne sont plus utilisées.
