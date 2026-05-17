# Edge Cases Benchmark — repo rag3db

**Date** : 17 mai 2026  
**Source** : `https://github.com/L-Defraiteur/rag3db` (~2954 fichiers, ~83K lignes C++/Rust/H)  
**Objectif** : challenger SFX v3 highlights + ground truth + perf sur données réelles

---

## 1. Contains — identifiants longs (cross 8-byte token boundary)

Le MAX_TOKEN=8, donc tout identifiant >8 chars cross au moins une frontière.

| # | Query | Occurrences | Intérêt |
|---|-------|:-----------:|---------|
| C1 | `"rag3db_prepared_statement_bind_cpp_value"` (40 chars) | ~5 | 5 segments snake_case, 5+ tokens v3 |
| C2 | `"rag3db_connection_set_max_num_thread_for_exec"` (45 chars) | ~3 | 6 segments, le plus long identifiant |
| C3 | `"ku_dynamic_cast"` (15 chars) | ~799 | très fréquent, 2 tokens, highlight exact |
| C4 | `"constPtrCast"` (12 chars) | ~500+ | CamelCase split en 2 tokens v3 ("constPtr" + "Cast") |
| C5 | `"ProjectGraphNativeBindData"` (26 chars) | ~5 | 3+ tokens CamelCase |
| C6 | `"getNodePropertyInfos"` (20 chars) | ~5 | vérif highlight [0..20] |
| C7 | `"std::unique_ptr"` (15 chars) | ~1000+ | `::` = séparateur, cross-token |
| C8 | `"std::atomic<uint64_t>"` (21 chars) | ~178 | `::`, `<`, `>` — multi-sep |

### Ground truth attendu

Pour C1 `"rag3db_prepared_statement_bind_cpp_value"` :
- Doit matcher dans `src/c_api/prepared_statement.cpp` (définition)
- Highlight : `byte_from=0` ou offset de la déclaration, `byte_to = byte_from + len` exact
- Ne doit PAS matcher `rag3db_prepared_statement_bind_bool` (sous-chaîne différente)

Pour C3 `"ku_dynamic_cast"` :
- ~799 occurrences dans tout le repo
- Highlight doit couvrir exactement les 15 bytes à chaque occurrence

---

## 2. Contains strict_sep=false — sep-stripped

| # | Query | Texte attendu | Intérêt |
|---|-------|---------------|---------|
| S1 | `"kudynamiccast"` | `"ku_dynamic_cast"` | 2 underscores strippés |
| S2 | `"stdunique"` | `"std::unique_ptr"` | `::` strippé |
| S3 | `"stdatomic"` | `"std::atomic<"` | double-colon strippé |
| S4 | `"rag3dbpreparedstatement"` | `"rag3db_prepared_statement..."` | 2 underscores, long |
| S5 | `"constptrcast"` | `"constPtrCast"` | lowercase + no-sep |
| S6 | `"getNodeProperty"` vs `"getnodeproperty"` | même texte | case + sep |

### Ground truth attendu

S1 `"kudynamiccast"` strict_sep=false :
- Doit trouver TOUTES les ~799 occurrences de `ku_dynamic_cast`
- Même nombre de résultats que C3 avec strict_sep=true
- Highlight byte ranges identiques (même positions dans le texte original)

---

## 3. Fuzzy d=1 — typos réalistes

| # | Query | Distance | Texte attendu | Intérêt |
|---|-------|:---:|---------------|---------|
| F1 | `"ku_dinamic_cast"` | 1 | `"ku_dynamic_cast"` | y→i substitution |
| F2 | `"constPtrCas"` | 1 | `"constPtrCast"` | deletion 't' final |
| F3 | `"ptrCastt"` | 1 | `"ptrCast"` | insertion 't' |
| F4 | `"getNodePropretyInfos"` | 1 | `"getNodePropertyInfos"` | transposition r/e |
| F5 | `"getRelPropertyInfos"` | 0 | `"getRelPropertyInfos"` | exact (ne pas confondre avec Node/Foreign) |
| F6 | `"schdule"` | 1 | `"schedule"` | query du bench existant |

### Ground truth attendu

F1 `"ku_dinamic_cast"` d=1 :
- Doit trouver `ku_dynamic_cast` (substitution y→i = 1 edit)
- Ne doit PAS trouver `ku_dynamic_cast` avec d=0
- Nombre résultats ≈ nombre résultats de C3

F5 exact match :
- `"getRelPropertyInfos"` d=0 doit trouver SEULEMENT les occurrences de `getRelPropertyInfos`
- PAS `getNodePropertyInfos` ni `getForeignPropertyInfos`

---

## 4. Regex — patterns cross-token

| # | Pattern | Intérêt |
|---|---------|---------|
| R1 | `"ku_dynamic_cast<.*>"` | extraction littéral "ku_dynamic_cast" + DFA validation |
| R2 | `"get(Node\|Rel\|Foreign)PropertyInfos"` | alternation, 3 variantes |
| R3 | `"rag3db_.*_bind_.*"` | wildcards autour de littéraux |
| R4 | `"std::(unique\|shared)_ptr"` | alternation dans namespace |
| R5 | `"[A-Z][a-z]+[A-Z][a-z]+"` | pattern CamelCase générique |

### Ground truth attendu

R2 `"get(Node|Rel|Foreign)PropertyInfos"` :
- Doit matcher exactement les 3 variantes dans table_info.cpp
- Highlight couvre l'identifiant complet à chaque occurrence

---

## 5. Highlights — vérification byte ranges

| # | Texte | Query | byte_from attendu | byte_to attendu |
|---|-------|-------|:-:|:-:|
| H1 | `"ku_dynamic_cast<StructColumn&>"` | `"ku_dynamic_cast"` | 0 | 15 |
| H2 | `"entry->ptrCast<RelGroupCatalogEntry>()"` | `"ptrCast"` | 7 | 14 |
| H3 | `"std::unique_ptr<TableFuncBindData>"` | `"unique_ptr"` | 5 | 15 |
| H4 | `"input.bindData->constPtrCast<...>()"` | `"constPtrCast"` | 16 | 28 |

---

## 6. Edge cases purs — patterns inhabituels

| # | Pattern dans le repo | Intérêt |
|---|---------------------|---------|
| E1 | `"Café résumé"` (chunker.rs test) | UTF-8 accents, `é` = 2 bytes |
| E2 | `"🎉 Unicode est supporté"` (chunker.rs test) | emoji content |
| E3 | `"UTF_8"` (stem.cpp) | underscore = sep ou content ? |
| E4 | Lignes >200 chars (template C++) | tokens très longs avec <> |
| E5 | `"INVALID_NODE_GROUP_IDX"` (all caps + underscore) | 3 segments majuscules |
| E6 | `"GDSDenseObjectManager"` (21 chars CamelCase) | 3 tokens de 7-8 bytes |

---

## 7. Queries du bench existant (baseline v2)

Le bench_sharding.rs utilise ces queries sur linux kernel. À adapter pour rag3db :

| v2 query | Équivalent rag3db | Type |
|----------|-------------------|------|
| `contains 'mutex_lock'` | `contains 'ku_dynamic_cast'` | substring fréquent |
| `contains 'function'` | `contains 'TableFunction'` | substring CamelCase |
| `startsWith 'sched'` | `startsWith 'rag3db_'` | prefix fréquent |
| `contains_split 'struct device'` | `contains_split 'unique_ptr TableFuncBindData'` | multi-token |
| `fuzzy 'schdule' d=1` | `fuzzy 'ku_dinamic_cast' d=1` | typo réaliste |

---

## 8. Plan benchmark v3

1. **Dataset** : clone `L-Defraiteur/rag3db` (~2954 fichiers, ~83K lignes)
2. **Mode** : RR 4 shards seulement (pas single/TA pour vitesse)
3. **Index** : v3 avec EqualChunkTokenizer + overlap + word-stripped
4. **Queries** : les 25+ cas ci-dessus
5. **Vérifications** :
   - Ground truth : grep naïf vs résultats v3 (count exact match)
   - Highlights : byte_from/byte_to correspondent au texte original
   - Perf : temps par query type (contains, fuzzy d=1, regex)
   - Pas de faux négatifs : tout ce que grep trouve, v3 doit aussi trouver
   - Pas de faux positifs : v3 ne doit pas retourner de docs qui ne contiennent pas la query
