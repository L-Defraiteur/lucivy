# Rapport investigation FN — session 7

> 28 mai 2026, branche feature/dag-query-refactor

## Score ground truth

11/15 a 5000 docs (meme que session 6). 4 FN, tous SCALE-DEPENDENT.

## Infra construite cette session

1. **execute_sequential** dans luciole (runner DAG sans scheduler)
2. **LocalDag / LocalNode<S>** (DAG sans Send/'static, fan-out via Rc)
3. **9 noeuds find_literal_v3** (FstCandidates, ResolveSingle, ChunkChain, SiblingChunk, ResolveChunk, WordChain, SiblingWord, ResolveWord, Merge)
4. **Mermaid explain** (DagResult::dump_mermaid)
5. **Edge annotations** (annotate_output + explain mode, JSON par arete)
6. **FST keys dump** dans FstCandidatesNode (cles brutes + parents)
7. **Per-candidate postings** dans ResolveSingleNode (ordinal → doc_ids)
8. **Doc forensics** dans le ground truth test (find segment, tokenize, reverse scan)

## Preuve du bug

### Le doc FN

- Global idx: 4742
- Path: `tools/rust_api/rag3db-src/extension/rag3weaver/codeparsers/src/base/language_parser.rs`
- Contenu pertinent: `"common functionality"` (ligne 19, tout en minuscule)
- Segment: 33, local doc_id: 30

### Ce que le tokenizer produit

`"functionality"` (13 chars) est un mot unique. `equal_chunks("functionality", "", 8)` :
- num_chunks = ceil(13/8) = 2
- base = 13/2 = 6, extra = 13%2 = 1
- Chunk 0: 7 bytes → `"functio"`, overlap = premier 2 bytes du chunk 1 = `"na"` → extended = `"functiona"`
- Chunk 1: 6 bytes → `"nality"`, overlap = "" → extended = `"nality"`

NOTE: own_len=7, PAS 8. Le tokenizer divise egalement (pas stride fixe).

### Ce que le FST contient

Le range scan `ge="function" lt="functioo"` trouve la cle `"functiona"` (9 bytes) car elle commence par "function". Cette cle existe dans 8 segments sur 80 (ceux qui ont un doc avec "functionality" ou similaire).

### Ce que les postings montrent

Dans le segment 33 (celui du doc FN) :
- 11 candidats FST pour "function" — **AUCUN** n'a doc 30 dans ses postings
- 155 ordinals totaux pour doc 30 — **AUCUN** dont le texte contient "functio"
- Les postings pour doc 30 passent de pos=95 `"common "` (bytes 549-556) directement a pos=97 `"ality\n\n"` (bytes 564-571)
- **Le posting pour pos=96 (bytes 556-564, le chunk "functio") n'existe pas**

### Conclusion

Le chunk pour "functionality" est bien produit par le tokenizer (pos 96, 7 bytes). Mais sa posting (doc_id=30, pos=96, byte_from=556, byte_to=564) n'a jamais ete ecrite dans le sfxpost.

C'est un **bug du collector_v3** a l'indexation. Le posting est perdue quelque part entre `add_value()` (qui appelle `token_postings[intern_id].push(...)`) et `into_data()` (qui construit `ord_map` et assigne les ordinals finaux).

## Piste pour le fix

Le bug est dans `collector_v3.rs`, probablement dans `into_data()`. Le chunk "functio" + overlap "na" = extended "functiona" est interned normalement via `intern_extended("functiona", ...)`. Le posting est pushee dans `token_postings[intern_id]`. Mais quand `into_data()` construit le `ord_map`, soit :

1. L'intern_id n'est pas ajoute a `sorted_indices` (filtre par `is_word_stripped` ?)
2. Les postings sont ecrasees par un `dedup` incorrect
3. L'intern_id est remapped vers un ordinal final qui ne recoit pas les postings

### Prochaine etape

Instrumenter le collector : quand le texte etendu contient "functio", logger chaque etape (intern, posting push, into_data mapping). Env var `V3_DIAG_COLLECTOR` + le mot cible.

## Fichiers generes

- `/tmp/v3_ground_truth_report.txt` — rapport complet
- `/tmp/v3_ground_truth_fails.json` — queries en echec
- `/tmp/v3_dag_function_strict.json` — DAG explain par segment (80 segments)
- `/tmp/v3_forensics_function_strict.json` — forensics du doc FN
- `/tmp/v3_diag_build.txt` — diag multi-parent du builder

## Commits cette session

```
4efaa89 feat(luciole): add execute_sequential
3c96d49 feat(luciole): add LocalDag / LocalNode
0819d2b feat: find_literal_v3 as LocalDag — 9 nodes, builder, parity tests
1b270f8 feat: mermaid explain for DAG
017e9ea feat: edge annotations for DAG explain
819c2dc diag: DAG explain per segment in V3_DIAG mode (fix sfx_file)
c985f45 diag: deep DAG explain — FST keys + per-candidate postings
dd8b24e diag: doc forensics — find FN doc segment, tokenize, check candidates + postings
321a94b diag: deep forensics — wider prefix scan + reverse ordinal scan
```
