# Rapport final — Session 7

> 27-28 mai 2026, branche feature/dag-query-refactor

## Score ground truth

| | 500 docs | 5000 docs |
|---|---|---|
| Debut session | 15/15 | **11/15** |
| Fin session | 15/15 | **13/15** |

### Queries corrigees (strict)
- `function strict` : 1466 → **1467** (FN corrige)
- `rag3db strict` : 3075 → **3076** (FN corrige)

### Queries restantes (relax uniquement)
- `function relax` : 1 FP (doc 1849 `wal_record.cpp`, zero occurrence de "function")
- `TableFunction relax` : 1 FN (doc 2091 `node_batch_insert.cpp`, "table function" 2 mots)

**Zero FN en strict.** Les 2 fails restants sont dans le word pipeline (relax).

## Root cause identifiee et corrigee

### Le bug

`collector_v3.rs:intern_extended()` — un seul namespace pour chunk et word-stripped.
Quand les deux ont le meme texte etendu (ex: "functional") :
1. Le premier inscrit fixe la meta `is_word_stripped`
2. Le second herite du mauvais flag sans mise a jour
3. `into_data()` skip les chunks marques word-stripped → **postings perdues**

### Les 3 fix appliques

**Fix 1** — `intern_extended()` : prefixer la cle d'interning
```
chunk: "functional" → cle "functional"
word-stripped: "functional" → cle "\x00ws:functional"
```

**Fix 2** — `into_data()` ord_map : prefixer les cles du BTreeMap
```
chunk: "C:functional" → OrdEntry avec postings chunk
word-stripped: "W:functional" → OrdEntry avec postings vides (WordSfxPost)
```

**Fix 3** — `tokens` BTreeSet → Vec : correspondance 1:1 avec content_postings/own_lens

**Fix 4** — `FstCandidateV3.partition` : champ u8 pour identifier la partition d'origine.
`resolve_single_v3` skip les candidats partition 0x02 (postings dans WordSfxPost, pas sfxpost).

### Invariant a ne jamais violer

> Toute structure qui mappe texte → donnee dans le collector doit etre
> partitionnee par type (chunk vs word-stripped). Un BTreeSet qui deduplique
> par texte detruit cette partition → regression silencieuse.

## Infra construite

### luciole (2 ajouts)

| Fichier | Quoi |
|---|---|
| `luciole/src/runtime.rs` | `execute_sequential()` — DAG runner sans scheduler, ~50 lignes |
| `luciole/src/local_dag.rs` | `LocalDag<S>`, `LocalNode<S>` — DAG sans Send/'static, fan-out Rc, EdgeAnnotations |

### lucivy — DAG query (3 fichiers)

| Fichier | Quoi |
|---|---|
| `src/suffix_fst/briques/dag_nodes.rs` | 9 noeuds : FstCandidates, ResolveSingle, ChunkChain, SiblingChunk, ResolveChunk, WordChain, SiblingWord, ResolveWord, Merge |
| `src/suffix_fst/briques/dag_builder.rs` | `find_literal_v3_dag()`, `find_literal_v3_dag_explained()`, `LiteralDagResult` |
| `src/suffix_fst/briques/dag_nodes.rs` | Annotations explain : FST keys brutes, postings par candidat, chains JSON |

### Explain

- `DagResult::dump_mermaid(edges)` — rendu Mermaid avec metrics par noeud
- `EdgeAnnotations` — JSON par arete (candidats, chains, matches)
- `FstCandidateV3.partition` — identifie la partition d'origine

### Diagnostics ground truth

| Outil | Quoi |
|---|---|
| V3_DIAG=1 | DAG explain par segment + doc forensics |
| V3_DIAG_COLLECTOR=mot | Log chaque intern/posting/ordinal contenant "mot" |
| V3_DIAG_BUILD=1 | Log multi-parent keys dans le builder FST |
| Doc forensics | Trouve le segment du doc FN, tokenise, reverse scan ordinals |

## Comment on a trouve le bug

1. DAG explain → 80 segments, identifie que le doc FN n'a aucun candidat
2. Doc forensics → trouve le segment (33), le doc local (30)
3. Reverse ordinal scan → 155 ordinals pour doc 30, **aucun** avec "function" dans le texte
4. Postings manquantes → le chunk "function" (pos 96, bytes 556-564) n'a pas de posting
5. V3_DIAG_COLLECTOR → l'intern_id 3545 pour "functional" existe mais n'apparait pas dans into_data
6. Code review → `intern_extended` retourne l'ord existant sans mettre a jour les meta
7. Le word-stripped "functional" interned avant le chunk → meta `is_word_stripped: true` → chunk skip

## Piste pour les 2 derniers fails

Les deux sont dans le **word pipeline** (mode relax uniquement).

Le FP et le FN viennent probablement de `DeferredWordPostings` dans `into_data()` (collector_v3.rs lignes 690-752). Cette section joint les postings chunk via `content_key_to_interns` + `expected_distance`. Avec la separation des namespaces, il y a plus d'intern_ords par content key → les jointures par distance peuvent :
- matcher accidentellement (FP : doc sans "function" recoit un match fantome)
- rater un match (FN : cross-word "table" + "function" pas joint)

**Audit complet** fait par agent Explore — 7 issues identifies dans le query-time, les critiques (#1 resolve_single skip 0x02) deja fixes.

## Tous les commits

```
4efaa89 feat(luciole): add execute_sequential
3c96d49 feat(luciole): add LocalDag / LocalNode
0819d2b feat: find_literal_v3 as LocalDag — 9 nodes, builder, parity tests
1b270f8 feat: mermaid explain for DAG
017e9ea feat: edge annotations for DAG explain
a786b04 diag: DAG explain per segment in V3_DIAG mode
c985f45 diag: deep DAG explain — FST keys + per-candidate postings
dd8b24e diag: doc forensics
321a94b diag: deep forensics — wider prefix scan + reverse ordinal scan
9e6bfcb diag: instrument collector_v3 with V3_DIAG_COLLECTOR
d351c1b fix: separate intern namespaces for chunk vs word-stripped
e17ab4c fix: partition ord_map by chunk/word-stripped namespace
01a29d2 fix: partition tokens Vec + ord_map alignment
9a08dc2 docs: theories pour les 2 derniers fails
35984ce fix: add partition field to FstCandidateV3 + skip word-stripped in resolve_single
```

## Tests casees a fixer

- `test_merge_two_segments` — le merge reconstruit des structures sans passer par intern_extended, les ordinals ont change
- `test_tokens_should_be_sorted` — tokens est maintenant un Vec avec doublons possibles, le test attend un tri strict sans doublons
