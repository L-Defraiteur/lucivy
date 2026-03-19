# Doc 01 — Architecture : système SFX propre, pensé pour le DAG

Date : 20 mars 2026

## Le constat

Le système SFX actuel est greffé sur tantivy. Chaque composant a été
ajouté incrémentalement, résultant en :

1. **sfxpost construit au merge mais le SfxCollector le construit aussi** → duplication, divergence
2. **Fichiers per-field (.N.sfx, .N.sfxpost) hors du SegmentComponent** → GC ne les connaît pas nativement
3. **Gapmap copié byte-par-byte au merge** → corruption silencieuse si format incompatible
4. **Reverse doc_map au merge** → docs perdus silencieusement si pas dans le map
5. **SfxCollector fait du double tokenization** → inefficace, divergence possible avec le term dict
6. **Pas d'observabilité** → on ne sait pas qui a construit quoi, quand, avec quelles données

## La vision : tout est un DAG, tout est un composant

### Principe 1 : le SFX est un SegmentComponent de première classe

```rust
enum SegmentComponent {
    Postings,
    Positions,
    FastFields,
    FieldNorms,
    Terms,
    Store,
    Offsets,
    // NOUVEAU : per-field SFX comme composant natif
    SuffixFst { field_id: u32 },    // .{field_id}.sfx
    SuffixPost { field_id: u32 },   // .{field_id}.sfxpost
}
```

Conséquences :
- `segment_meta.list_files()` inclut TOUS les .sfx et .sfxpost
- Le GC les protège nativement
- Le merge sait exactement quels composants existent
- Plus de manifest séparé

### Principe 2 : chaque segment a TOUJOURS un sfxpost complet

Le sfxpost est construit à l'écriture du segment (SfxCollector le fait
déjà !). Le merge ne fait que FUSIONNER les sfxpost des segments source.
Plus de segments sans sfxpost.

```
Écriture segment :
  tokenize → SfxCollector → build() → (sfx_bytes, sfxpost_bytes) → write both

Merge segments :
  read sfxpost_A + sfxpost_B → remap doc_ids → write sfxpost_merged

Jamais de segment sans sfxpost. Jamais de fallback silencieux.
```

### Principe 3 : le merge SFX est un sous-DAG observable

```
load_sources ──┬── collect_tokens ── build_fst ────────┐
               ├── copy_gapmap ── validate_gapmap ─────┼── assemble_write
               └── merge_sfxpost ── validate_sfxpost ──┘
```

Chaque étape :
- Chronométrée (métriques dans DagResult)
- Observable (subscribe_dag_events)
- Tappable (inspecter les bytes intermédiaires)
- Validée (erreur explicite si corruption)

### Principe 4 : la tokenization est faite UNE SEULE FOIS

Aujourd'hui : le segment_writer tokenize via le tokenizer du champ
(potentiellement stemmé), PUIS le SfxCollector re-tokenize via RAW_TOKENIZER.
Double travail, divergence possible.

Demain : le segment_writer produit les tokens UNE FOIS, avec les offsets.
Le SfxCollector reçoit les tokens déjà produits, pas le texte brut.

```
Aujourd'hui :
  text → Tokenizer → tokens → postings writer
  text → RAW_TOKENIZER → raw_tokens → SfxCollector  (DOUBLE TOKENIZATION)

Demain :
  text → RAW_TOKENIZER → raw_tokens_with_offsets
    ├── postings writer (positions, offsets)
    └── SfxCollector (suffixes, gapmap, sfxpost)
```

Si un stemmer est configuré, les deux tokenizations sont nécessaires
(stemmé pour BM25, raw pour contains). Mais le SfxCollector utilise
TOUJOURS les tokens raw, pas les stemmés.

### Principe 5 : le GC est un nœud DAG, pas un hack

Le GC actuel :
- `garbage_collect_files()` appelle `list_files()` qui essaie de lister
  les fichiers à garder via des heuristiques (manifest, gc_protected_segments)
- Fragile : oublie des fichiers → corruption

Le GC demain :
- Un nœud `GCNode` dans le commit DAG
- Il reçoit la liste EXACTE des segments actifs (output de FinalizeNode)
- Il calcule les fichiers à garder à partir des SegmentComponents
- Aucune heuristique, aucun fichier oublié

```
finalize ── gc(active_segments) ── reload
```

### Principe 6 : validation intégrée, pas optionnelle

Chaque construction de sfxpost est suivie d'une validation :

```rust
let sfxpost_bytes = build_sfxpost(...);
validate_sfxpost(&sfxpost_bytes, &term_dict, num_docs)?;  // ERREUR si invalide
write_sfxpost(field, &sfxpost_bytes);
```

La validation vérifie :
- Tous les doc_ids dans le sfxpost sont < num_docs
- Tous les ordinals dans le sfxpost correspondent à des termes existants
- Le nombre d'entrées par ordinal est cohérent avec le doc_freq du term dict
- Le gapmap a exactement num_docs entrées

Si la validation échoue → erreur explicite, pas de segment corrompu écrit.

## Plan d'implémentation

### Phase 1 : SegmentComponent natif pour SFX

1. Ajouter `SuffixFst { field_id }` et `SuffixPost { field_id }` à l'enum
2. `relative_path()` retourne `"{uuid}.{field_id}.sfx"` / `.sfxpost`
3. `list_files()` inclut automatiquement les per-field SFX
4. Supprimer le manifest SFX séparé (remplacé par le SegmentComponent)
5. Adapter le GC pour utiliser les composants natifs

### Phase 2 : validation systématique

1. `validate_sfxpost(bytes, term_dict, num_docs)` dans sfx_merge.rs
2. Appelée après chaque construction (segment_writer ET merge)
3. Erreur explicite si validation échoue (pas de write)
4. Métriques : `errors_found`, `docs_validated`, `ordinals_checked`

### Phase 3 : merge sfxpost fiable

1. Vérifier que CHAQUE segment source a un sfxpost
2. Si un segment n'a pas de sfxpost → ERREUR (pas de fallback silencieux)
3. Le merge produit un sfxpost validé
4. Tout via le sfx_dag avec observabilité

### Phase 4 : tokenization unique

1. Le segment_writer produit les raw tokens UNE FOIS
2. Le SfxCollector reçoit les tokens (pas le texte)
3. Si stemmer : double tokenization explicite (stemmé → BM25, raw → SFX)
4. Pas de divergence possible

### Phase 5 : GC propre

1. GCNode reçoit la liste des segments actifs
2. Calcule les fichiers via SegmentComponent natif
3. Supprime tout le reste
4. Plus de gc_protected_segments, plus de heuristiques

## Ce qui ne change PAS

- Le format .sfx (FST + parent list + gapmap)
- Le format .sfxpost (ordinal → posting entries)
- Le SuffixFstBuilder (add_token, build)
- Le GapMapWriter/Reader
- Le prefix_walk dans la search
- Le SuffixContainsQuery

Le code de search ne change pas. C'est la construction et la maintenance
des fichiers qui est refactorisée.

## Estimation

```
Phase 1 (SegmentComponent)  ~100 lignes (enum + list_files + relative_path)
Phase 2 (validation)        ~80 lignes (validate_sfxpost + intégration)
Phase 3 (merge fiable)      ~50 lignes (checks + errors au lieu de fallbacks)
Phase 4 (tokenization)      ~200 lignes (refactor segment_writer + SfxCollector API)
Phase 5 (GC propre)         ~50 lignes (GCNode + segments list)

Total : ~480 lignes
Supprimé : ~200 lignes (manifest, hacks, fallbacks silencieux)
Net : ~280 lignes
```

## Résultat attendu

- 100% des docs trouvés par contains search (plus de sfxpost manquant)
- Zéro corruption silencieuse (validation systématique)
- Observabilité totale (DAG + events + taps)
- GC fiable (composants natifs)
- Performance identique ou meilleure (une seule tokenization)
