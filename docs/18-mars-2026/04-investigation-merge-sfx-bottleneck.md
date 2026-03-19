# Investigation — merge_sfx bottleneck sur indexation à grande échelle

Date : 18 mars 2026
Status : diagnostiqué, design en cours
Branche : `experiment/decouple-sfx`

## Contexte

Bench sur le kernel Linux (~90K fichiers C), 4 shards RR, release build.
L'indexation ralentit de manière super-linéaire :

```
 5K docs →  3.8s
10K docs →  5.3s
15K docs →  9.9s
20K docs → 17.0s  (extrapolation 90K → plusieurs minutes)
```

## Diagnostique par instrumentation

Instrumentation ajoutée dans `merge_state.rs` (merge incrémental par phases)
et `segment_updater_actor.rs` (merge policy decisions).

### Résultats : timing par phase de merge

Pour un merge de ~1000 docs (8 segments × ~125 docs) :
```
postings:  ~500ms  (2 passes: init 10-15ms, puis ~500ms pour les champs indexés)
sfx:       ~1500ms (suffix FST rebuild)
store:     ~2ms
fast_fields: <1ms
close:     ~40ms
TOTAL:     ~2000ms
```

Pour un merge de ~2700 docs :
```
postings:  ~1050ms
sfx:       ~2900ms
TOTAL:     ~4000ms
```

Pour un merge de ~5000 docs :
```
postings:  ~1200ms
sfx:       ~3450ms
close:     ~500ms
TOTAL:     ~5200ms
```

**Le sfx (suffix FST rebuild) prend ~70% du temps de chaque merge.**

### Cascade des merges

La LogMergePolicy fusionne des segments dès qu'il y en a 8 :
1. Segments de 60-90 docs → merge en ~1000 docs (2s)
2. Segments de ~1000 docs → merge en ~2700 docs (4s)
3. Segments de ~2700 docs → merge en ~5000 docs (5.2s)
4. Extrapolation : ~5000 → ~10000 (>10s), ~10000 → ~20000 (>20s), etc.

Chaque niveau de cascade double le temps. C'est la source de la non-linéarité.

## Analyse du code : merge_sfx (src/indexer/merger.rs:826-1063)

Le merge_sfx a 5 phases :

### Phase 1 — Collect unique tokens (lignes 886-932)
Itère les term dictionaries de tous les segments sources, insère dans un
BTreeSet. Complexité : O(T × log T) avec T = tokens uniques total.
Pas le bottleneck.

### Phase 2 — Build suffix FST (lignes 934-941) ← BOTTLENECK
```rust
let mut sfx_builder = SuffixFstBuilder::new();
for (ordinal, token) in unique_tokens.iter().enumerate() {
    sfx_builder.add_token(token, ordinal as u64);
}
let (fst_data, parent_list_data) = sfx_builder.build()?;
```

`add_token` génère TOUS les suffixes de chaque token. Pour "function" (8 chars)
→ 8 entrées: "function", "unction", "nction", "ction", "tion", "ion", "on", "n".

Pour 5000 docs de code C : ~50K tokens uniques × ~8 suffixes moyen = ~400K entrées.
`build()` fait un sort O(E log E) de toutes les entrées puis construit le FST.

C'est ici que 70% du temps est passé.

### Phase 3 — GapMap copy (lignes 944-956)
Simple copie doc par doc depuis les gapmaps sources. O(N docs). Rapide.

### Phase 4 — Merge sfxpost (lignes 958-1037)
- Re-parcourt les term dictionaries pour construire des maps token→ordinal
- Pour chaque token, merge les posting entries avec doc_id remapping
- Sort par (new_doc, ti) par token
Coût : O(T × postings_par_token). Significatif mais pas le bottleneck principal.

### Phase 5 — Write (lignes 1041-1054)
Sérialisation. Rapide.

## Pourquoi le FST doit être reconstruit

Le suffix FST mappe chaque suffixe vers ses "parents" (tokens qui contiennent
ce suffixe). Les parents sont identifiés par leur **ordinal** — la position du
token dans l'ordre alphabétique du term dictionary.

Lors d'un merge, les term dictionaries de N segments sont fusionnés en un seul.
Les ordinals changent : le token "function" qui était ordinal 42 dans le segment A
et ordinal 18 dans le segment B devient ordinal 31 dans le segment mergé.

Donc le FST entier doit être reconstruit avec les nouveaux ordinals.

## Pourquoi gapmap et sfxpost n'ont pas ce problème

- **GapMap** : indexé par (doc_id, token_index), pas par ordinal. Le doc_id est
  remappé via le doc_id_mapping (déjà calculé pour les postings). Copie directe.

- **sfxpost** : indexé par ordinal, donc il DOIT être reconstruit avec les
  nouveaux ordinals. Mais c'est beaucoup plus rapide que le FST car il n'y a
  pas de génération de suffixes ni de sort massif — juste une copie + remap
  des entries existantes.

## Approche proposée : skip le FST au merge, rebuild lazy au search

### Principe : commit / commit_fast

L'utilisateur contrôle quand le FST est reconstruit via deux modes de commit :

- **`commit()`** (défaut) : persist les données ET reconstruit les FST des
  segments deferred. Safe, prêt pour search immédiat.

- **`commit_fast()`** : persist les données seulement. Les merges qui se
  déclenchent écrivent un .sfx avec FST vide (gapmap + sfxpost copiés).
  Rapide, mais les segments mergés sont invisibles au search jusqu'au
  prochain `commit()`.

Pattern d'usage bulk :
```rust
for batch in docs.chunks(5000) {
    index(batch);
    handle.commit_fast();  // persist, pas de FST rebuild → rapide
}
handle.commit();  // rebuild tous les FST deferred, prêt pour search
```

Le contrat : après un `commit()`, tous les segments ont un FST valide.
Après un `commit_fast()`, certains segments mergés peuvent avoir un FST vide
→ les queries retournent EmptyScorer pour ces segments (résultats partiels).

### Détail des modifications

#### 1. merge_sfx_deferred() dans merger.rs
Remplace merge_sfx() pendant les merges déclenchés après un commit_fast.
- Skip phases 1-2 (collect tokens + build FST) → gain ~70%
- Phase 3 : copie gapmap doc par doc (identique)
- Phase 4 : reconstruit sfxpost avec doc_id remapping (identique)
- Phase 5 : écrit .sfx avec FST vide (fst_length=0, num_suffix_terms=0)

#### 2. rebuild_deferred_sfx() dans segment_updater_actor.rs
Appelée par `commit()` (pas commit_fast). Scanne les segments committés,
trouve ceux avec FST vide, reconstruit le FST depuis le term dictionary :
```
pour chaque segment avec num_suffix_terms == 0 :
    lire le term dictionary du segment mergé
    SuffixFstBuilder::add_token() pour chaque terme
    SuffixFstBuilder::build() → fst_data + parent_list
    réécrire le .sfx complet (FST + gapmap existant)
```

#### 3. Fallback EmptyScorer dans les queries
Les queries sfx (SuffixContainsQuery, AutomatonPhraseWeight,
RegexContinuationQuery) retournent EmptyScorer quand sfx_file() est None
ou que le FST est vide. Pas de crash, juste 0 résultats de ce segment.

#### 4. SfxFileReader : handle FST vide
`SfxFileReader::open()` construit un empty Map quand fst_length == 0.
`load_sfx_files()` dans segment_reader skip les .sfx avec num_suffix_terms == 0.

#### 5. Flag dans IndexWriter / commit message
Le SegmentUpdaterActor reçoit un flag `rebuild_sfx: bool` dans le message
Commit. `commit()` envoie `rebuild_sfx: true`, `commit_fast()` envoie false.

### Avantages

- L'indexation bulk scale linéairement : les merges ne font plus de FST rebuild
- Le coût FST est payé UNE SEULE FOIS au dernier commit(), pas à chaque merge
- Contrôle explicite par l'utilisateur, pas de magie async
- Simple à comprendre et à debugger

### Risques

- **Oubli du commit() final** : si l'utilisateur ne fait que des commit_fast()
  sans jamais commit(), les segments mergés restent sans FST. Les queries
  retournent des résultats partiels. Mitigation : documenter clairement,
  log warning si search détecte des segments deferred.

- **Coût du commit() final** : reconstruire les FST de N segments deferred
  peut prendre du temps. Mais c'est un coût ponctuel et prévisible, pas un
  blocage surprise pendant l'indexation.

## Résultats — implémentation v2 (18 mars soir)

### Changements implémentés

1. **`merge_sfx_deferred()`** dans `merger.rs` — skip le FST, copie gapmap + sfxpost
2. **`merge_state.rs`** — `step_sfx()` appelle `merge_sfx_deferred` au lieu de `merge_sfx`
3. **`rebuild_sfx_inline()`** dans `segment_reader.rs` — rebuild le FST à la volée au
   `load_sfx_files()` quand un segment deferred est détecté (num_suffix_terms == 0)
4. **`rebuild_deferred_sfx()`** dans `segment_updater_actor.rs` — rebuild au `commit()`
   (quand `rebuild_sfx: true`)
5. **`commit()` / `commit_fast()`** dans `IndexWriter` et `PreparedCommit` — flag
   `rebuild_sfx` dans le message SuCommitMsg
6. **`SfxFileReader`** dans `file.rs` — handle FST vide (empty Map)
7. **`lucivy_trace!()`** macro dans `lib.rs` — debug conditionnel via `LUCIVY_DEBUG=1`

### Bench : kernel Linux 90K fichiers C, 4 shards RR, release

```
 5K:    3.6s   (3.6s/batch)
10K:    5.3s   (1.7s)
15K:   19.9s   (14.6s — cascades + rebuild inline)
20K:   25.3s   (5.4s)
25K:   29.8s   (4.5s)
30K:   34.7s   (4.9s)
35K:   42.3s   (7.6s)
40K:   46.2s   (3.9s)
45K:   52.0s   (5.8s)
50K:   58.2s   (6.2s)
55K:   74.9s   (16.7s — gros merge cascade)
60K:   83.7s   (8.8s)
65K:   90.5s   (6.8s)
70K:   97.2s   (6.7s)
75K:  107.9s   (10.7s)
80K:  120.0s   (12.1s)
85K:  132.5s   (12.5s)
90K:  145.6s   (13.1s)
TOTAL: 148.4s
```

Query times : ~30ms pour contains/startsWith sur 90K docs.
Highlights : corrects sur toutes les queries testées.

### Comparaison avec baseline (merge_sfx complet)

La baseline avait bloqué à 45K docs (>1min pour un batch de 5K) à cause du
SuffixFstBuilder rebuild O(E log E) dans chaque merge. Le deferred sfx passe
90K docs sans blocage, mais des sauts restent (55K: 16.7s, 85K: 12.5s) dus
aux merges postings sur gros segments + rebuild inline au reader.reload().

### Problèmes identifiés (non résolus)

1. **Blocage intermittent** : lors d'un run précédent (même code), le bench
   est resté bloqué à 85K docs pendant >5min (189% CPU, 6.4GB RAM stable).
   Le run suivant (identique) a passé sans problème en 148s. Cause non
   identifiée — potentiellement une contention sur le mmap cache ou un merge
   cascade qui tombe mal.

2. **`contains 'drivers' (path)` retourne 0 hits** : le field "path" devrait
   matcher "drivers/" dans les chemins du kernel. Possible que les segments
   mergés aient un FST rebuild incorrect pour le champ path (field_id différent
   du content), ou que le rebuild inline échoue silencieusement.

3. **Mmap cache stale** : le rebuild inline écrit le .sfx via `atomic_write`
   puis relit via `open_read_custom`. Le mmap cache peut retourner l'ancien
   contenu si le `Weak` ref est encore vivant. Fix appliqué (drop file_slice
   avant rebuild) mais pas vérifié sur tous les chemins.

4. **Coût du rebuild inline** : les merges de 8-11K docs prennent 3-9s dont
   une part significative est le rebuild FST au reader.reload(). Ce coût est
   payé à chaque commit, même en mode `commit_fast()` (car le bench utilise
   `commit()` qui fait le rebuild).

## Fichiers clés

| Fichier | Rôle |
|---------|------|
| `src/indexer/merger.rs` | merge_sfx (original) + merge_sfx_deferred (skip FST) |
| `src/indexer/merge_state.rs` | MergeState incrémental (step_sfx → deferred) |
| `src/index/segment_reader.rs` | load_sfx_files + rebuild_sfx_inline |
| `src/indexer/segment_updater_actor.rs` | rebuild_deferred_sfx + SuCommitMsg flag |
| `src/indexer/prepared_commit.rs` | commit() vs commit_fast() |
| `src/indexer/index_writer.rs` | commit_fast() exposé |
| `src/suffix_fst/file.rs` | SfxFileReader handle FST vide |
| `src/suffix_fst/builder.rs` | SuffixFstBuilder — le bottleneck O(E log E) |
| `src/lib.rs` | lucivy_trace!() macro (LUCIVY_DEBUG=1) |

## Historique

- **c9f1dc8** : première tentative deferred sfx, couplée au bug fuzzy distance=1
- **5fc5871** : fix contains default distance 0 + tests regression
- **12a12e9** : revert deferred sfx (pour baseline propre)
- **Session actuelle** : réimplémentation propre avec commit/commit_fast,
  rebuild inline, lucivy_trace!, fix mmap cache
