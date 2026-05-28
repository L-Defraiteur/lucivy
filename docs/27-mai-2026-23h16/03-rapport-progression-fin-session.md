# Rapport progression fin de session 7

> 28 mai 2026, branche feature/dag-query-refactor

## Score

- Avant session : **11/15** (4 FN, tous scale-dependent)
- Apres fix : **13/15** (2 strict FN corriges, 1 relax FN restant, 49 FP relax introduits)

## Bug trouve et corrige

### Root cause

`collector_v3.rs:intern_extended()` (ligne ~506) utilise le texte etendu comme cle unique pour l'interning. Mais les chunks (partition 0x00/0x01) et les word-stripped entries (partition 0x02) peuvent avoir le **meme texte etendu** (ex: "functional" = chunk de "functionality" vs word-stripped de "functional").

Quand le word-stripped est interned en premier, son meta `is_word_stripped: true` est enregistre. Le chunk qui arrive apres recoit le **meme intern_id** sans mettre a jour le meta. Dans `into_data()`, la boucle chunk fait `if is_word_stripped { continue }` → les postings du chunk sont ignorees.

### Fix applique

`collector_v3.rs:intern_extended()` — prefix la cle d'interning avec `"\x00ws:"` pour les word-stripped entries. Les deux namespaces ne collisionnent plus. Chaque chunk et chaque word-stripped a son propre intern_id et ses propres postings.

### Fichier modifie

- `src/suffix_fst/collector_v3.rs` — `intern_extended()` (seul changement fonctionnel)

## Problemes restants

### 1. TableFunction relax : 1 FN (existait AVANT le fix)

- Doc 2091 : `src/processor/operator/persistent/node_batch_insert.cpp`
- Texte : `"table function"` (deux mots separes)
- Query relax : `"tablefunction"` (13 chars, seps strippees)
- Le word pipeline (partition 0x02) ne trouve pas le match cross-word "table" + "function"
- Le forensics montre : doc 11 n'a **aucun ordinal** avec "function" dans son texte → meme symptome que le bug d'intern
- **Piste** : c'est probablement le MEME bug (collision intern chunk/ws) mais pour le mot "function" dans le contexte de ce doc. Le fix a corrige le namespace mais le `ord_map` dans `into_data()` fusionne encore chunk et word-stripped sous la meme cle texte. Il faudrait peut-etre un `ord_map` separe aussi.

### 2. function relax : 49 FP

- Le fix a cree des FP en mode relax (1516 v3 vs 1467 grep)
- Cause probable : avec la separation des namespaces, les ordinals chunk ET word-stripped sont maintenant DEUX entrees distinctes dans l'ord_map avec le meme texte. Le builder les enregistre sous la meme cle FST lowercase avec deux ordinals differents. Le range scan retourne les deux. Mais le word-stripped ordinal a des postings vides dans sfxpost (ses postings sont dans WordSfxPost). `resolve_single_v3` cree des matches avec byte_from=0 byte_to=0 pour ces ordinals vides → FP.

### 3. test_merge_two_segments : casse

- Le merge reconstruit des structures depuis termtexts, pas via le collector
- Le changement d'ordinals (plus d'ordinals maintenant avec la separation) casse les assertions du test

## Architecture DAG construite

### Luciole

| Fichier | Description |
|---|---|
| `luciole/src/runtime.rs` | `execute_sequential()` — runner DAG sans scheduler |
| `luciole/src/local_dag.rs` | `LocalDag<S>`, `LocalNode<S>`, `LocalNodeCtx`, `LocalPortValue` (Rc), `EdgeAnnotations` |
| `luciole/src/lib.rs` | exports |

### Lucivy — noeuds query

| Fichier | Description |
|---|---|
| `src/suffix_fst/briques/dag_nodes.rs` | 9 noeuds: FstCandidates, ResolveSingle, ChunkChain, SiblingChunk, ResolveChunk, WordChain, SiblingWord, ResolveWord, Merge |
| `src/suffix_fst/briques/dag_builder.rs` | `find_literal_v3_dag()`, `find_literal_v3_dag_explained()`, `LiteralDagResult` |
| `src/suffix_fst/briques/mod.rs` | modules dag_nodes, dag_builder |

### Diag ground truth

| Fichier | Description |
|---|---|
| `lucivy_core/tests/test_sfx_v3_ground_truth.rs` | V3_DIAG mode : DAG explain par segment + doc forensics (find segment, tokenize, reverse ordinal scan) |

## Ou regarder pour les next steps

### Pour les 49 FP relax

1. `src/suffix_fst/collector_v3.rs` — `into_data()`, le `ord_map` (ligne ~560)
   - Le chunk et le word-stripped avec le meme texte deviennent deux entrees distinctes
   - Le word-stripped entry a des postings vides dans sfxpost mais un ordinal valide dans le FST
   - `resolve_single_v3` recoit cet ordinal et produit des matches vides → FP
2. **Solution probable** : dans la boucle word-stripped de `into_data()` (ligne ~588), ne PAS ajouter au `ord_map` si un chunk entry avec le meme texte existe deja. Ou bien, filtrer les matches avec byte_from==0 && byte_to==0 dans resolve_single.

### Pour le 1 FN relax (TableFunction)

1. Verifier si c'est le meme bug d'intern — le forensics montre 0 ordinals avec "function" pour doc 11
2. Le fix devrait deja corriger ca pour les chunks. Mais en relax, la resolution passe par WordSfxPost pas sfxpost
3. Regarder `src/suffix_fst/collector_v3.rs` — section WordStrippedEntry (ligne ~380) et DeferredWordPostings (ligne ~622)
4. L'entree word-stripped "function" utilise `first_chunk_intern_ord` pour ses postings WordSfxPost. Si ce chunk intern a le mauvais `is_word_stripped` flag, les postings WordSfxPost ne seront pas construites

### Pour le test merge

1. `src/indexer/sfx_dag_v3.rs` — `merge_segments_v3()` (ligne ~284)
2. Cette fonction ne passe pas par `intern_extended` donc n'est pas directement affectee
3. Le probleme est que `build_segment()` (qui utilise le collector) produit maintenant plus d'ordinals → le sfxpost/termtexts ont un layout different → les assertions du test echouent

## Commits session 7

```
4efaa89 feat(luciole): add execute_sequential
3c96d49 feat(luciole): add LocalDag / LocalNode
0819d2b feat: find_literal_v3 as LocalDag — 9 nodes, builder, parity tests
1b270f8 feat: mermaid explain for DAG
017e9ea feat: edge annotations for DAG explain
a786b04 diag: DAG explain per segment in V3_DIAG mode
c985f45 diag: deep DAG explain — FST keys + per-candidate postings
dd8b24e diag: doc forensics — find FN doc segment, tokenize, check candidates + postings
321a94b diag: deep forensics — wider prefix scan + reverse ordinal scan
9e6bfcb diag: instrument collector_v3 with V3_DIAG_COLLECTOR
d351c1b fix: separate intern namespaces for chunk vs word-stripped
```
