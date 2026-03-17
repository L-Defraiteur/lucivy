# Optimisation : Suffix FST prefix byte partitioning (SI=0 vs SI>0)

Date : 17 mars 2026

## Contexte

Le SI=0 filter dans SuffixContainsQuery a donné 3.2x de gain sur startsWith.
Mais le suffix FST walk traverse encore les entrées SI>0 sans les résoudre.
startsWith reste ~15% plus lent que contains sur le même terme.

## Idée

Préfixer chaque entrée du suffix FST avec `\x00` (SI=0) ou `\x01` (SI>0).
Le tri lexicographique du FST sépare naturellement les deux groupes.

```
Avant (un seul espace) :
  "ment" → parents: [(ordinal_ment, si=0), (ordinal_segment, si=4)]

Après (partitionné) :
  "\x00ment" → parents: [(ordinal_ment, si=0)]     ← token complet
  "\x01ment" → parents: [(ordinal_segment, si=4)]   ← substring
```

## Impact

### startsWith
Walk uniquement les `\x00` entrées. Le FST skip toutes les entrées `\x01` nativement (range scan). Aucun suffix SI>0 traversé. startsWith devient **plus rapide que contains**.

### contains
Walk les deux (`\x00` + `\x01`). Même travail total qu'avant. Parent lists plus petites (splittées) → légèrement meilleur cache CPU. Net : pareil ou mieux.

### Indexation
Le builder ajoute 2 entrées pour les suffixes qui ont les deux SI. Mais la plupart n'ont QUE SI>0 (seul le token complet a SI=0). Le FST grossit de ~5%, pas 2x. Overhead négligeable.

## Fichiers à modifier

### 1. `src/suffix_fst/builder.rs` — SuffixFstBuilder
Quand `add_token(token, parent_entries)` est appelé avec les suffixes :
- Pour chaque suffix, splitter les parents en SI=0 et SI>0
- Si SI=0 non vide : ajouter `\x00{suffix}` → parent list SI=0
- Si SI>0 non vide : ajouter `\x01{suffix}` → parent list SI>0

```rust
fn add_suffix(&mut self, suffix: &str, parents: Vec<ParentEntry>) {
    let si0: Vec<_> = parents.iter().filter(|p| p.si == 0).collect();
    let si_rest: Vec<_> = parents.iter().filter(|p| p.si > 0).collect();

    if !si0.is_empty() {
        let key = format!("\x00{suffix}");
        self.fst_builder.insert(&key, encode_parents(&si0));
    }
    if !si_rest.is_empty() {
        let key = format!("\x01{suffix}");
        self.fst_builder.insert(&key, encode_parents(&si_rest));
    }
}
```

### 2. `src/suffix_fst/file.rs` — SfxFileReader
- `prefix_walk(query)` : walk `\x00{query}` + `\x01{query}` (contains, both)
- `prefix_walk_si0(query)` : walk `\x00{query}` only (startsWith)
- `fuzzy_walk` : idem, version with/without SI filter
- Strip le prefix byte quand on retourne les résultats

### 3. `src/suffix_fst/collector.rs` — SfxCollector
Adapter `build()` pour générer les entrées avec le prefix byte.

### 4. `src/query/phrase_query/suffix_contains.rs`
- `suffix_contains_single_token_prefix` : utilise `prefix_walk_si0` au lieu de `prefix_walk` + filtre
- Le filtre runtime `if prefix_only && parent.si > 0` est remplacé par le partitionnement FST

### 5. Format version
`.sfx` format version bump. Les anciens fichiers ne sont plus lisibles.
Déclenche un reindex.

## Estimation

~80 lignes modifiées. Format break (version bump .sfx).
Gain attendu : startsWith ~15% plus rapide que la version SI=0 filter actuelle.
Zero régression sur contains ou indexation.

## Priorité

Basse. Le SI=0 runtime filter donne déjà 3.2x de gain. Le prefix byte
est un bonus de 15% sur startsWith. À faire lors d'un format bump planifié
(pour ne pas forcer un reindex juste pour ça).
