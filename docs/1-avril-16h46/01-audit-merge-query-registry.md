# 01 — Audit : chemins de merge, query contains, et registry

Date : 1 avril 2026
Branche : `feature/fuzzy-via-literal-resolve`

## Contexte

Session d'investigation du multi-token d=0 qui ratait des résultats.
Le bug venait du code dupliqué dans `cross_token_resolve_for_multi` qui
divergeait du pipeline qui fonctionne (fix : unifier sur les briques
de `literal_pipeline.rs`). Ce rapport fait l'état des lieux des chemins
de merge, des chemins de query, et du registry d'index custom.

---

## 1. Chemins de merge SFX

### 1.1 Legacy merge (`merger.rs:merge_sfx_legacy`)

Appelé depuis `merger.rs:write()` (ligne ~597).

| Fichier | Méthode | Status |
|---------|---------|--------|
| .sfx | `serializer.write_sfx()` | ✅ |
| sfxpost | `write_custom_index` | ✅ |
| posmap | `write_custom_index` | ✅ |
| bytemap | `write_custom_index` | ✅ |
| termtexts | `write_custom_index` | ✅ |
| gapmap | `write_custom_index` | ✅ |
| sibling | `write_custom_index` | ✅ |
| **sepmap** | — | **❌ MANQUANT** |

### 1.2 DAG merge (`merge_dag.rs` → `sfx_dag.rs:WriteSfxNode`)

DAG parallèle : collect_tokens → build_fst + copy_gapmap + merge_sfxpost → write_sfx.

`WriteSfxNode` reconstruit manuellement posmap/bytemap/termtexts depuis
`SfxPostReaderV2` + tokens. **N'utilise PAS le registry pour merger.**

| Fichier | Méthode | Status |
|---------|---------|--------|
| .sfx | `serializer.write_sfx()` | ✅ |
| sfxpost | `write_custom_index` | ✅ |
| posmap | reconstruction manuelle + `write_custom_index` | ✅ |
| bytemap | reconstruction manuelle + `write_custom_index` | ✅ |
| termtexts | reconstruction manuelle + `write_custom_index` | ✅ |
| gapmap | `write_custom_index` | ✅ |
| sibling | `write_custom_index` | ✅ |
| **sepmap** | — | **❌ MANQUANT** |

### 1.3 Segment initial (`collector.rs` → registry `build()`)

Lors de l'écriture initiale d'un segment, `SfxCollector::build()` itère
`all_indexes()` et appelle `build()` pour chaque index. **Le SepMap EST
construit ici.** C'est le seul chemin qui passe par le registry.

| Fichier | Status |
|---------|--------|
| Tous les 7 (sfxpost, posmap, bytemap, termtexts, gapmap, sibling, sepmap) | ✅ via registry |

### 1.4 Résumé merge

```
Segment initial : registry.build() → TOUS les fichiers ✅
Legacy merge    : write_custom_index manuels → sepmap MANQUANT ❌
DAG merge       : write_custom_index manuels → sepmap MANQUANT ❌
```

**Le registry n'est utilisé qu'au build initial. Aucun chemin de merge
n'utilise `SfxIndexFile::merge()`.** Chaque chemin reconstruit les fichiers
manuellement.

---

## 2. Registry d'index (`index_registry.rs`)

### 2.1 Fichiers enregistrés dans `all_indexes()`

1. `SfxPostIndex` — sfxpost
2. `GapMapIndex` — gapmap
3. `SiblingIndex` — sibling
4. `PosMapIndex` — posmap
5. `ByteMapIndex` — bytemap
6. `TermTextsIndex` — termtexts
7. `SepMapIndex` — sepmap

### 2.2 Trait `SfxIndexFile`

```rust
pub trait SfxIndexFile: Send + Sync {
    fn id(&self) -> &'static str;
    fn extension(&self) -> &'static str;
    fn build(&self, ctx: &SfxBuildContext) -> Vec<u8>;
    fn merge(&self, sources: &[Option<&[u8]>], ctx: &SfxMergeContext) -> Vec<u8>;
}
```

- `build()` : appelé au segment initial ✅
- `merge()` : **JAMAIS appelé** ❌ — les merges reconstruisent manuellement

### 2.3 GC

Le GC est protégé via `all_components()` dans `segment_component.rs` qui
itère `all_indexes()` pour créer des `CustomSfxIndex` entries. **Tous les
fichiers du registry sont protégés du GC**, y compris sepmap.

### 2.4 Faiblesses de l'abstraction

1. **`merge()` jamais utilisé** : le trait expose une méthode merge mais aucun
   chemin de merge ne l'appelle. Les merges reconstruisent chaque fichier
   manuellement → chaque nouveau fichier doit être ajouté à la main dans
   2+ endroits.

2. **Pas d'enforcement** : rien ne garantit qu'un merge écrit tous les fichiers
   du registry. On peut oublier un fichier (comme sepmap) sans erreur.

3. **Reconstruction manuelle** : le DAG reconstruit posmap/bytemap/termtexts
   depuis sfxpost + tokens au lieu de lire les sources et merger. Fragile si
   le format change.

### 2.5 Proposition : merge via registry

```rust
// Au lieu de write_custom_index manuels pour chaque fichier :
for index_file in all_indexes() {
    let source_bytes: Vec<Option<&[u8]>> = source_segments
        .iter()
        .map(|s| s.sfx_index_file(index_file.id(), field))
        .collect();
    let merged = index_file.merge(&source_bytes, &merge_ctx);
    serializer.write_custom_index(field, index_file.id(), &merged);
}
```

Avantages :
- Un seul endroit à maintenir
- Nouveau fichier = implémenter `SfxIndexFile` et l'ajouter à `all_indexes()`
- Impossible d'oublier un fichier lors du merge

---

## 3. Chemins de query contains

### 3.1 Routage dans `build_contains_query`

```
contains d=0          → SuffixContainsQuery
contains d>=1         → RegexContinuationQuery (trigram pigeonhole + DFA)
contains regex        → RegexContinuationQuery (regex mode)
contains_split        → split whitespace → boolean should de contains
startsWith            → SuffixContainsQuery.with_prefix_only()
```

### 3.2 SuffixContainsQuery (d=0)

**Single-token** :
- `suffix_contains_single_token_with_terms()` dans suffix_contains.rs
- Logique propre : resolve_suffix → prefix_walk → cross_token_search_with_terms
- N'utilise PAS les briques du pipeline (code dupliqué)

**Multi-token** :
- `suffix_contains_multi_token_impl()` dans suffix_contains.rs
- **Utilise maintenant `resolve_token_for_multi` du pipeline** ✅ (fix de cette session)
- Chain building + separator validation (GapMap si strict_separators)

**Fichiers lus** : .sfx, .sfxpost, .termtexts, (.gapmap si strict)

### 3.3 RegexContinuationQuery (d>0 et regex)

**Fuzzy (d>0)** :
- `fuzzy_contains_via_trigram()` — trigram pigeonhole + DFA Levenshtein
- `generate_ngrams()` extrait les trigrams en skippant les cross-separator
- Pipeline sélectivité : `fst_candidates` + `cross_token_falling_walk` ✅
- Validation finale : Levenshtein DFA sur le concat posmap

**Regex** :
- `regex_contains_via_literal()` — extrait les littéraux du regex
- `regex_gap_analyzer.rs` classifie les gaps (AcceptAnything/ByteRangeCheck/DfaValidation)
- SepMap lu pour ByteRangeCheck
- DFA validate_path pour gaps complexes

**Fichiers lus** : .sfx, .sfxpost, .termtexts, .posmap, .bytemap, .gapmap, .sepmap

### 3.4 Code dupliqué vs unifié

| Logique | suffix_contains.rs | literal_pipeline.rs | Status |
|---------|-------------------|---------------------|--------|
| FST walk (prefix_walk) | inline | `fst_candidates()` | dupliqué |
| Falling walk + sibling DFS | `cross_token_search_with_terms()` | `cross_token_falling_walk()` | dupliqué |
| Resolve postings | inline closures | `resolve_candidates()` | dupliqué |
| Chain adjacency | inline | `resolve_chains()` | dupliqué |
| Per-token multi-token resolve | ~~`cross_token_resolve_for_multi()`~~ | `resolve_token_for_multi()` | **unifié** ✅ |

**Le single-token d=0 utilise encore son propre code.** Il devrait réutiliser
les briques du pipeline comme le multi-token le fait maintenant.

### 3.5 Code mort après le refactoring

- `cross_token_resolve_for_multi()` — plus appelée par multi-token, mais
  potentiellement encore référencée pour le fuzzy multi-token via suffix_contains
  (même si en prod d>0 passe par RegexContinuationQuery)

---

## 4. SepMap : état actuel

| Opération | Status |
|-----------|--------|
| Build initial (collector) | ✅ via registry |
| Legacy merge | ❌ pas écrit |
| DAG merge | ❌ pas écrit |
| GC protection | ✅ via all_components |
| Lecture regex (ByteRangeCheck) | ✅ avec fallback si absent |
| Lecture contains d=0 | non nécessaire |
| Lecture fuzzy | non nécessaire (DFA directe) |

**Impact** : après merge, les queries regex avec patterns type `[a-z]+`
retombent en fallback DFA au lieu d'utiliser le SepMap O(1). Pas de
résultats manquants mais perf dégradée.

---

## 5. Prochaines étapes prioritaires

### Merge
1. **Ajouter sepmap aux 2 chemins de merge** — le plus rapide
2. **Refactorer les merges pour utiliser `SfxIndexFile::merge()`** —
   itérer `all_indexes()` dans une boucle unique. Élimine le risque
   d'oubli de fichiers futurs.

### Query
3. **Unifier single-token d=0 sur le pipeline** — `suffix_contains_single_token_with_terms`
   devrait utiliser `fst_candidates` + `cross_token_falling_walk` au lieu
   de son propre code inline.
4. **Supprimer `cross_token_resolve_for_multi`** — code mort depuis le fix.

### Fuzzy multi-token
5. **Design : distance par segment ou globale ?** — actuellement les trigrams
   sont générés sur la query entière (pas par segment alphanum). La validation
   DFA finale vérifie la distance totale. Si on veut d=1 par segment, il
   faut splitter avant la recherche trigram.

### Registry
6. **Enforcement : test qui vérifie que merge écrit tous les fichiers du registry**
