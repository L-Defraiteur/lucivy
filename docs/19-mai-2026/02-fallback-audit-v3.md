# Audit des fallbacks dans le pipeline query v3

## Fallbacks identifiés

### 1. `resolve_chains_v3_relaxed` → ByteOrdered (CRITIQUE)

**Fichier** : `src/suffix_fst/briques/resolve.rs:102-107`

```rust
if posmap.is_some() && bytemap.is_some() {
    resolve_chains_impl(chains, resolver, filter_docs,
        AdjacencyMode::Relaxed { posmap, bytemap })
} else {
    // No PosMap/ByteMap available — fallback to byte-ordered check
    resolve_chains_impl(chains, resolver, filter_docs, AdjacencyMode::ByteOrdered)
}
```

**Impact** : ByteOrdered ne vérifie PAS que les tokens intermédiaires sont pure-sep. Accepte des chains qui traversent des mots entiers de contenu. Cause directe des FP relaxed (42 FP uint64_t, 2 FP function, 1 FP TableFunction).

**Appelants sans maps** :
- `composite::find_literal_v3()` passe `None, None` (ligne 36) — appelé par :
  - `composite::find_multi_token_v3()` (lignes 107, 119)
  - tests unitaires

**Appelants avec maps** :
- `composite::find_literal_v3_full()` — passe les maps quand fournis
- `orchestrator::contains_v3_full()` — câblé dans `contains_query_v3.rs` ✓ (fixé cette session)

**Status** : `contains_query_v3` câblé ✓. `find_multi_token_v3` encore en fallback.

### 2. `find_literal_v3()` sans maps (MOYEN)

**Fichier** : `src/suffix_fst/briques/composite.rs:27-36`

```rust
pub fn find_literal_v3(...) -> Vec<MatchV3> {
    find_literal_v3_full(reader, query, resolver, anchor_start, strict_separators, filter_docs, None, None)
}
```

**Impact** : Wrapper de commodité qui passe `None, None`. Tout appelant qui utilise `find_literal_v3` au lieu de `find_literal_v3_full` n'a pas de vérification des intermédiaires.

**Appelants** :
- `find_multi_token_v3()` (lignes 107, 119) — multi-token relaxed sans vérification
- `regex_v3::resolve_trigrams_v3()` (ligne 231) — fuzzy relaxed sans vérification
- Tous les tests unitaires

**Action** : soit supprimer `find_literal_v3` et forcer `_full`, soit ajouter posmap/bytemap en paramètre à `find_multi_token_v3` et `resolve_trigrams_v3`.

### 3. `regex_v3::resolve_trigrams_v3` — trigrams sans maps (MOYEN)

**Fichier** : `src/suffix_fst/briques/regex_v3.rs:231`

```rust
let matches = composite::find_literal_v3(...);
```

Le fuzzy/trigram pipeline utilise `find_literal_v3` pour résoudre chaque trigramme. En relaxed, les chains trigrammes pourraient produire les mêmes FP que contains.

**Impact** : FP potentiels en fuzzy relaxed. Pas testé dans le ground truth.

### 4. `find_multi_token_v3` — sub-tokens sans maps (MOYEN)

**Fichier** : `src/suffix_fst/briques/composite.rs:107,119`

Les sous-tokens dans une query multi-mots sont résolus individuellement via `find_literal_v3` (sans maps). Si un sous-token utilise une chain relaxed, ByteOrdered est utilisé.

**Impact** : FP en multi-token relaxed.

### 5. v2 compat layer — fallbacks divers (BAS)

- `suffix_contains.rs:266` : "Depth 3+: fallback to stored text verification if available"
- `regex_continuation_query.rs:1557` : idem
- `suffix_contains_query.rs:84` : "cross_token_search fallback which uses falling_walk + token_len"

Ce sont des fallbacks du pipeline v2, pas v3. Pas directement liés à nos FP v3.

## Implications sur le fix strict (best_consumed filter)

### Est-ce que posmap/bytemap auraient permis un meilleur fix strict ?

**Non.** Les FP strict venaient du chain builder qui mélangeait des ordinals de consumed différents. Ce bug est AVANT le resolve — les maps n'auraient pas aidé car :

1. Le chain builder produit des chains avec des ordinals incorrects à certaines positions
2. Le resolve (strict ou relaxed) résout ces ordinals et trouve des postings adjacents
3. Même avec posmap/bytemap en Relaxed mode, les tokens adjacents sont valides (pos+1 = strict adjacency, pas besoin de vérifier les intermédiaires)

Les FP strict avaient pos+1 directement (chunks adjacents). Le problème n'était pas les intermédiaires mais les ORDINALS eux-mêmes. Le fix best_consumed est le bon fix pour strict.

### Est-ce que posmap/bytemap fixent les FP relaxed ?

**Oui, en grande partie.** Les FP relaxed viennent de chains qui traversent des tokens de contenu (pas pure-sep). Avec posmap/bytemap, `intermediates_are_pure_sep()` rejette ces chains.

**Mais attention** : le fix ne fonctionne que si les maps sont DISPONIBLES. Sans maps → fallback ByteOrdered → FP. Il faut s'assurer que les maps sont toujours passées.

### Fix best_consumed : pourrait-il causer des FN relaxed ?

**Oui, potentiellement.** En relaxed, un split en partition 0x02 (word-stripped) peut avoir un consumed différent de celui en 0x00/0x01 pour le même point de la query. Filtrer par best_consumed pourrait écarter des ordinals 0x02 valides.

**Mais** : les sub_splits sont triés par consumed DESC. Le best = le plus de bytes consommés. Les ordinals 0x02 (word-stripped, souvent plus long que les chunks) ont typiquement un consumed PLUS GRAND. Donc le filtre garde les 0x02 et écarte les 0x00/0x01 courts. C'est le bon sens.

**Cas à surveiller** : un split 0x00 avec consumed=5 (best) écarte un split 0x02 avec consumed=3. Si le split 0x02 est le seul chemin valide dans un doc, c'est un FN. Mais l'overlap du split 0x00 valide 5 bytes contre la query → le match 0x00 est plus fiable.

## Actions recommandées

1. **[FAIT]** Câbler posmap/bytemap dans `contains_query_v3` → `contains_v3_full`
2. **[TODO]** Propager posmap/bytemap à `find_multi_token_v3`
3. **[TODO]** Propager posmap/bytemap à `regex_v3::resolve_trigrams_v3`
4. **[TODO]** Considérer supprimer `find_literal_v3` (wrapper sans maps) pour forcer l'usage de `_full`
5. **[TODO]** Vérifier que les tests d'intégration briques utilisent aussi les maps quand strict_sep=false
