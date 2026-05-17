# Knowledge Dump — Session 3 (17-18 mai 2026)

**Branche** : `feature/sfx-v3-overlap-tokenizer`
**Dernier commit** : `0b6baa7`
**Etat** : 77/77 integration tests, ground truth en cours

---

## 1. Problemes resolus

### Tail entry faux positifs (Bug 1 de session 2)
- **Cause** : tail entry pour mots >8 bytes utilisait l'ordinal du dernier chunk. byte_from du posting + own_len du tail = byte range faux.
- **Fix** : seuil releve de `> max_token` (8) a `> MAX_SUFFIX_INDEX + max_token` (264). collector_v3.rs.

### Word-stripped own ordinals
- **Cause** : word-stripped entries reutilisaient l'ordinal du premier chunk. Deux mots differents partageant le meme premier chunk ("include" et "inclusive" ont tous les deux le chunk "inclu") partageaient le meme ordinal → postings melangees → faux positifs.
- **Fix** : word-stripped entries sont internees comme leurs propres tokens via `intern_extended(ws_extended, ...)` avec `is_word_stripped: true`. Postings copiees du premier chunk sous un ordinal distinct. collector_v3.rs.

### is_word_stripped flag
- **Cause** : les word-stripped entries internees via `intern_extended` passaient dans le build loop `add_token` et apparaissaient dans les partitions 0x00/0x01 (ghost matches).
- **Fix** : `is_word_stripped: bool` dans TokenMetaV3. Build loops skip `if meta.is_word_stripped { continue; }`. Tous les fichiers.

### Tests stripped pre-existants
- **Cause** : 10 tests unitaires (builder_v3, fst_walk, resolve, composite, orchestrator) appelaient `add_token` sans `add_word_stripped` → partition 0x02 vide.
- **Fix** : `with_reader` et `build_index` helpers mis a jour pour appeler `add_word_stripped` quand `sep_len > 0`.

### Ground truth test dual-mode
- **Fix** : deux modes (strict/relaxed) avec grep adapte (literal vs sep-agnostic). test_sfx_v3_ground_truth.rs.

---

## 2. Content ordinals (architecture)

### Principe
Tokens avec meme contenu+sep mais overlaps differents partagent un **content ordinal**. Les postings sont agregees dans `into_data()` par content key (`text[..own_len]`).

### Implementation
- `collector_v3.rs` / `into_data()` : `content_key_map` BTreeMap, `content_postings`, `num_content_ords`, `intern_to_final` mappe vers content ordinals.
- `SfxCollectorDataV3` : `content_postings` (par content ordinal), `tokens` (BTreeSet de content keys).
- Tous les consumers (sfx_dag_v3, integration tests, etc.) itèrent `content_postings` pour le sfxpost et utilisent `intern_to_final` pour le raw_ordinal dans le FST.

### Probleme fondamental decouvert
Le query "include" (7 bytes) matche le FST key "include" (= chunk "inclu" 5 bytes + overlap "de" 2 bytes). Le content ordinal pour "inclu" a des postings de "inclusive" aussi → **faux positif**. L'overlap fait partie de la preuve du match mais le content ordinal agrege sans distinction.

### Solutions en place
1. **Word-stripped own ordinals** : resout le partage chunk↔mot
2. **Marker entries dans le FST** (builder_v3.rs) : cles tronquees a own_len pour que le falling walk detecte les splits aux frontieres de contenu. Fix UTF-8 boundary.
3. **Content_len filter dans l'orchestrateur** : `span >= query_content_len`. Filtre les matches trop courts (le span ne couvre que le contenu, pas l'overlap). **Probleme : trop agressif sur le vrai corpus.**
4. **Anchor start fix** : cross-token chains autorisees pour anchor_start avec `first_sti == 0` filtre.

### Overlap sibling table
- **Module** : `src/suffix_fst/overlap_siblings.rs`
- **Format** : offset table + packed u32 entries. O(1) lookup.
- **Construction** : dans `into_data()` et `merge_segments_v3`, depuis le content_key_map.
- **Usage** : pas encore branche cote query. Pret pour validation overlap post-match.

---

## 3. Probleme ouvert : content_len filter trop agressif

### Symptome
77/77 integration tests passent mais le ground truth a des regressions massives (function strict : 62 → 22, uint64_t strict : 11 → 0).

### Cause
Le filtre `span >= query_content_len` dans l'orchestrateur compare le span d'un **single-token match** (= content_len - sti du token) avec le nombre de chars content du query. Pour les queries qui traversent des tokens (comme "uint64_t" qui est "uint64_" + "t"), le single-token match ne couvre qu'une partie du query → filtre. Le cross-token chain devrait prendre le relais, mais le falling walk ne trouve pas toujours le split.

### Piste pour la prochaine session
Le marker entry dans le FST devrait aider le falling walk a trouver les splits aux frontieres own_len. Mais le filtre content_len est encore trop agressif — il filtre des matches ou le span est court mais CORRECT (le token couvre bien le query dans ses content bytes, avec le sep qui n'est pas compte dans le span).

**Idee** : le filtre devrait comparer `span + sep_matched >= query_len` au lieu de `span >= query_content_len`. Ou mieux : propager le sep dans le span (byte_to = bf + own_len au lieu de bf + own_len - sep_len).

---

## 4. Architecture fichiers (etat actuel)

```
src/suffix_fst/
  collector_v3.rs     — content ordinals, word-stripped own ordinals, is_word_stripped, overlap siblings
  builder_v3.rs       — marker entries aux frontieres own_len
  overlap_siblings.rs — NEW: content_ord → [intern_ords]
  briques/
    fst_walk.rs       — cross_token_chain simple (pas de forking)
    composite.rs      — anchor_start chain avec first_sti==0, sort sans dedup
    orchestrator.rs   — content_len filter, dedup apres filtre
```

---

## 5. Commits de la session

| Hash | Description |
|------|-------------|
| `7cf5d9c` | wip: content ordinals + word-stripped own ordinals + ground truth fixes |
| `cf7dc70` | feat: add overlap sibling table |
| `eac38b6` | fix: exclude word-stripped entries from partitions 0x00/0x01 |
| `0b6baa7` | wip: marker entries + content_len filter (77/77 unit, GT WIP) |

---

## 6. Prochaines etapes

1. **Fixer le content_len filter** : le span exclut les seps → le filtre est trop strict. Options : inclure sep dans le span, ou comparer differemment.
2. **Valider sur le ground truth** : une fois le filtre corrige, relancer les 500 fichiers.
3. **Brancher la sibling table** : pour les cas ou le content ordinal ne suffit pas, utiliser la sibling table pour valider l'overlap post-match.
4. **Merge v3** : cabler dans merge_dag.rs (actuellement NoMergePolicy).
