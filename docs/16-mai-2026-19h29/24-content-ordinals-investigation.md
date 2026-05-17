# Content Ordinals — Investigation et design

**Date** : 17 mai 2026 (session 3, soir)  
**Branche** : `feature/sfx-v3-overlap-tokenizer`  
**État** : en cours d'implémentation

---

## 1. Problème d'origine : overlap-dependent ordinals

### Symptôme
`std::unique_ptr` : 3 faux négatifs sur le ground truth (500 fichiers rag3db). v3 trouve 5 docs, grep 8.

### Cause racine
En v3, l'ordinal est assigné par **texte étendu** (content + sep + overlap). Le token `ptr<` suivi de `Connection` a l'extended text `ptr<Co`, tandis que `ptr<` suivi de `Value` a `ptr<Va`. Ces deux ont des **ordinals différents** malgré le même contenu `ptr<`.

Le cross-token chain construit une séquence d'ordinals `[std::un, unique_pt, ptr<Co]`. Cette séquence ne matche que dans les documents où `ptr` est suivi de `Connection`. Les documents où `ptr` est suivi de `Value` ou `rag3db` sont manqués.

### Premier fix tenté : forking
Dans `cross_token_chain_v3`, au lieu de garder `cands[0].raw_ordinal` pour le dernier step, générer une chain pour chaque variante d'ordinal (récursif, `extend_chain`).

**Résultat** : `std::unique_ptr` passe à 8=8 ✅ mais nécessite un `MAX_CHAIN_VARIANTS` hardcodé (d'abord 64, puis 1024). Avec 1024 : `function` strict passe de 27 à 52, mais reste 10 faux négatifs. Et des **faux positifs** apparaissent (`TableFunction` relax 6 vs 5).

**Verdict** : scotch. Le cap hardcodé est arbitraire, le nombre de variantes explose combinatoirement.

---

## 2. Design : content ordinals

### Idée
Séparer l'ordinal de posting (stable, indépendant de l'overlap) de la clé FST (qui inclut l'overlap pour la validation du walk).

**Content key** = `text[..own_len]` (content + sep, sans overlap).  
Tous les tokens avec le même content key partagent un **content ordinal**.  
Le sfxpost est indexé par content ordinal → postings agrégés.

### Implémentation
- `collector_v3.rs` / `into_data()` : groupement par content key dans un `BTreeMap`, agrégation des postings, mapping `intern_to_final` vers content ordinals.
- `SfxCollectorDataV3` : nouveau champ `content_postings` (par content ordinal), `num_content_ords`, `tokens` (BTreeSet de content keys).
- Tous les consumers (sfx_dag_v3, integration tests, etc.) mis à jour pour itérer `content_postings`.
- `cross_token_chain_v3` simplifié : plus de forking (un content ordinal couvre toutes les variantes d'overlap).

### Résultat

Deux catégories de bugs :

#### Bug A : faux positifs par débordement overlap
Le query "include" (7 bytes) matche la clé FST "include" (= chunk "inclu" 5 bytes + overlap "de" 2 bytes). `fst_candidates` retourne le content ordinal pour "inclu". Les postings incluent des documents où le chunk "inclu" est suivi de "si" (mot "inclusive"), pas "de" (mot "include").

**Fix** : filtrer dans `fst_candidates` les candidats où le query déborde de `own_len` dans la zone overlap. Règle : `query_len ≤ own_len - sti`. Ce fix fonctionne correctement.

#### Bug B : faux positifs par partage d'ordinal chunk ↔ word-stripped
Plus subtil. Le word-stripped entry du mot "include" utilise `first_intern_ord` = ordinal du chunk "inclu". Le mot "inclusive" a aussi un chunk "inclu" avec le MÊME content ordinal. Résultat : quand `fst_candidates` matche le word-stripped entry de "include" en partition 0x02, le content ordinal retourné a des postings dans TOUS les documents avec un chunk "inclu", y compris ceux de "inclusive".

**Cause** : le word-stripped entry réutilise l'ordinal du premier chunk. Deux mots différents partageant le même premier chunk partagent le même ordinal → les postings sont mélangées.

---

## 3. Fix proposé : ordinals séparés pour word-stripped

### Principe
Les word-stripped entries doivent avoir leurs **propres ordinals**, distincts des ordinals de chunks. Le contenu d'un word-stripped entry est le mot complet (ex: "include" 7 bytes), pas le premier chunk (ex: "inclu" 5 bytes). Deux mots différents "include" et "inclusive" auront des ordinals distincts en partition 0x02.

### Changement dans le collector
Au lieu de :
```rust
self.word_stripped_entries.push(WordStrippedEntry {
    first_intern_ord: first_intern,  // ← ordinal du chunk "inclu"
    ...
});
```

Créer un nouvel ordinal interné pour le mot complet :
```rust
// Interning séparé pour le word-stripped, avec ses propres postings
// copiées du premier chunk mais sous un ordinal distinct
let ws_intern = self.intern_word_stripped(word_content, first_chunk_posting);
```

### Postings du word-stripped
Les postings du word-stripped entry sont les MÊMES que celles du premier chunk (même doc, position, byte_from, byte_to). Mais elles sont sous un ordinal séparé. Cet ordinal n'est pas partagé avec d'autres mots.

Le content key pour le word-stripped ordinal serait le word_content (ex: "include"), pas le chunk text (ex: "inclu"). Comme "include" ≠ "inclusive", ils ont des content ordinals distincts.

### Ce qui reste inchangé
- Les chunks en partitions 0x00/0x01 gardent leurs content ordinals (groupés par content+sep, agrégés across overlaps)
- Le filtre `query_len ≤ own_len - sti` dans `fst_candidates` reste
- Le `cross_token_chain_v3` reste simple (pas de forking)
- Le sfxpost reste le même format

---

## 4. Résumé des 3 niveaux d'identité

| Niveau | Exemple | Utilisé pour | Ordinal type |
|--------|---------|-------------|--------------|
| Content pur | "inclu" (5 bytes) | — | — |
| Chunk complet (content+sep) | "inclu" (5b, sep=0) | Partitions 0x00/0x01 | Content ordinal (agrégé across overlaps) |
| Mot complet (word content) | "include" (7 bytes) | Partition 0x02 | Word ordinal (distinct par mot) |
| Extended (content+sep+overlap) | "include" (7b = 5+2) | Clé FST seulement | N'est plus un ordinal |

---

## 5. Bugs fixés dans cette session (avant content ordinals)

| Bug | Fix | Fichier |
|-----|-----|---------|
| Tail entry faux positifs | Seuil relevé `> 264` bytes | collector_v3.rs |
| 10 tests stripped manquants | Ajout `add_word_stripped()` dans helpers | builder_v3, fst_walk, resolve, composite, orchestrator |
| Ground truth test unfair | Deux modes (strict/relaxed) avec grep adapté | test_sfx_v3_ground_truth.rs |

## 6. État des tests

- 76 integration tests : OK (avec le filtre `own_len - sti`, 6 failures restent liées au Bug B)
- 9 pipeline tests : OK
- 12 collector tests : OK
- 8 DAG tests : OK
- Ground truth : 8/15 pass (Bug B cause les échecs restants)
