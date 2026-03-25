# Doc 02 — Plan d'expérimentation : cross-token substring search

Date : 25 mars 2026

## Setup expérimental

- **Pas de CamelCaseSplit** — tokenizer = `SimpleTokenizer + LowerCaser` uniquement
- **MAX_TOKEN_LEN = 5** — force des splits fréquents pour tester les cas limites
- Petit index de test (10-20 docs avec des mots qui chevauchent les boundaries)
- Le force-split à 5 chars est arbitraire — en prod ce serait 256 (ou configurable)

### Exemple d'index

```
Doc 0: "getElementById"   → tokens ["getel", "ement", "byid"]  (split at 5)
Doc 1: "rag3weaver"       → tokens ["rag3w", "eaver"]
Doc 2: "transformQuery"   → tokens ["trans", "formq", "uery"]
Doc 3: "hello world"      → tokens ["hello", "world"]
```

### Queries à tester

| Query | Split cross-token | Attendu |
|-------|-------------------|---------|
| "getel" | non (dans token 0) | doc 0 |
| "ement" | non (token 1 entier) | doc 0 |
| "element" | oui : "el" fin token 0 + "ement" token 1 | doc 0 |
| "getElement" | oui : "getel" token 0 + "ement" début token 1 | doc 0 |
| "mentBy" | oui : "ment" fin token 1 + "by" début token 2 | doc 0 |
| "rag3weaver" | oui : "rag3w" + "eaver" | doc 1 |
| "3weav" | oui : "3w" fin token 0 + "eav" début token 1 | doc 1 |
| "hello" | non | doc 3 |
| "lo wor" | multi-token (espace) : "lo" suffix token 0 + "wor" prefix token 1 | doc 3 |

## Approche 1 : Split exhaustif + ordinals seulement

### Principe

Pour une query Q de longueur L, essayer tous les splits en 2 parties :
```
Pour i = 1..L-1 :
  left  = Q[0..i]    — doit être un SUFFIXE d'un token à position N
  right = Q[i..L]    — doit être un PRÉFIXE d'un token à position N+1
```

Pour chaque split, on fait deux walks SFX qui retournent seulement des **ordinals** (pas de posting lookup) :
- `resolve_suffix(left)` → Vec<(ordinal, si)> pour les tokens qui se TERMINENT par `left`
- `prefix_walk_si0(right)` → Vec<(ordinal, si=0)> pour les tokens qui COMMENCENT par `right`

On ne resolve les posting lists QUE pour le meilleur split (celui qui a le moins d'ordinals combinés). L'adjacence (position N, N+1) est vérifiée sur les postings, pas sur les ordinals.

### Algorithme détaillé

```
1. Single-token SFX walk(Q) → si résultats, return (pas de cross-token)

2. Pour chaque split i = 1..L-1 :
   a. left_ordinals  = resolve_suffix(Q[0..i])     → Vec<ParentEntry>
   b. right_ordinals = prefix_walk_si0(Q[i..L])     → Vec<ParentEntry>
   c. Si left_ordinals.is_empty() || right_ordinals.is_empty() : skip
   d. score = left_ordinals.len() + right_ordinals.len()  (plus petit = plus sélectif)

3. Prendre le split avec le score minimum (pivot = le côté le plus sélectif)

4. Résoudre les postings du pivot, extraire doc_ids

5. Résoudre les postings de l'autre côté, filtré par doc_ids

6. Intersecte par (doc_id, position N, position N+1) = adjacence
```

### Coût

- L walks SFX (resolve_suffix + prefix_walk), chacun = O(log FST_size)
- Le resolve_suffix est un lookup exact (pas un scan), très rapide
- Le prefix_walk est un range scan, rapide pour des préfixes longs (peu de résultats)
- 1 seul posting lookup (sur le meilleur split)

### Avantages

- **Exhaustif** : teste toutes les positions de split possibles
- **Correct** : trouve toujours le bon split s'il existe
- **Ordinals seulement** : pas de posting explosion pendant la recherche de split

### Inconvénients

- O(L) walks — pour L=100 ça fait 100 walks. Mais chaque walk est O(log n), et
  la plupart returnent vide rapidement (le SFX n'a pas d'entrée pour des suffixes aléatoires).
- Pas de "multi-split" : ne gère pas les queries qui chevauchent 3+ tokens.
  Ex: "elementById" sur ["getel", "ement", "byid"] → chevauche token 0-1 ET token 1-2.
  Nécessiterait une extension récursive.

### Extension multi-split

Pour gérer les queries qui chevauchent 3+ tokens :
```
Après avoir trouvé le premier split (left, right) :
  Si left est encore trop long pour matcher un seul token :
    Récurser : split_search(left) pour la partie gauche
  Si right est encore trop long :
    Récurser : split_search(right) pour la partie droite
```

Ou plus simplement : après le premier split, right peut être passé au même algo (single token d'abord, split si échec). C'est naturellement récursif.

---

## Approche 2 : Walk qui "tombe" = pivot naturel

### Principe

Au lieu d'essayer tous les splits, on fait UN SEUL walk SFX progressif sur la query.
Le walk avance byte par byte dans le FST. Quand il n'y a plus de match → c'est la frontière de token. Le point de chute = le split naturel.

### Algorithme détaillé

```
1. Single-token SFX walk(Q) → si résultats, return

2. Trouver le point de chute :
   - Marcher dans le FST byte par byte pour Q
   - À chaque position i, vérifier s'il reste des entrées préfixées par Q[0..i]
   - Le plus grand i tel que le FST a des entrées = fall_point
   - Q[0..fall_point] = la partie qui matche comme suffix de token N
   - Q[fall_point..] = la partie qui doit être préfixe de token N+1

3. Le côté le plus court/sélectif = pivot
   - Résoudre ses postings → doc_ids

4. Résoudre l'autre côté filtré par doc_ids

5. Intersecte par adjacence (position N, N+1)
```

### Comment trouver le fall_point efficacement

Le FST (lucivy-fst) supporte le streaming. On peut ouvrir un stream avec le préfixe progressif :
```rust
// Pseudo-code
let mut fall_point = 0;
for i in 1..=query.len() {
    let prefix = &query[0..i];
    // Check if any FST key starts with this prefix
    let has_entries = sfx_fst.range().ge(prefix).lt(increment(prefix)).count() > 0;
    if has_entries {
        fall_point = i;
    } else {
        break;  // Le walk est tombé
    }
}
```

Mais ce pseudo-code fait L lookups. Mieux : utiliser l'automate du FST directement.
Le FST a une méthode `get()` ou `contains_key()` mais aussi un `Stream` qu'on peut
avancer node par node. Si on suit les transitions du FST byte par byte, on sait
exactement quand il n'y a plus de transition → fall_point. C'est O(fall_point) = O(L)
dans le pire cas mais avec un coût par step minuscule (1 lookup de transition).

### Avantages

- **1 walk principal** au lieu de L walks
- **Naturel** : le FST nous dit exactement où le token se termine
- **Rapide** : O(fall_point) avec un coût par step minimal

### Inconvénients

- Nécessite d'accéder au FST à bas niveau (transitions byte par byte)
  → lucivy-fst expose-t-il ça ? À vérifier.
- Trouve UN SEUL split point (le premier fall). Si la query chevauche 3+ tokens,
  il faut récurser sur la partie droite.
- Le fall_point n'est peut-être pas unique — il peut y avoir plusieurs splits valides.
  Le premier fall n'est pas forcément le bon. Exemple :
  "abcdef" avec tokens ["abc", "def"] et ["ab", "cdef"] → fall_point pourrait être à 2 ou 3.
  On aurait besoin du **dernier** fall_point (le plus long match).

### Extension : binary search sur le fall_point

Au lieu de walker byte par byte, on peut faire un binary search :
```
Si prefix_walk(Q[0..L/2]) a des résultats → chercher plus à droite
Sinon → chercher plus à gauche
```
O(log L) walks. Chaque walk est O(log FST_size). Total : O(log L × log FST_size).

---

## Comparaison

| | Approche 1 (split exhaustif) | Approche 2 (walk qui tombe) |
|---|---|---|
| **Walks SFX** | O(L) walks | O(1) walk principal + O(log L) si binary search |
| **Coût par walk** | O(log FST) | O(1) par byte (transition FST) |
| **Total** | O(L × log FST) | O(L) ou O(log L × log FST) |
| **Multi-split (3+ tokens)** | Extension récursive | Idem — récursion sur partie droite |
| **Implémentation** | Simple (resolve_suffix + prefix_walk existants) | Nécessite accès FST bas niveau |
| **Risque** | Aucun — utilise des API existantes | Dépend de l'API FST |

## Plan d'expérimentation

### Phase 0 : Setup

1. Retirer CamelCaseSplitFilter du RAW_TOKENIZER
2. Ajouter un MAX_TOKEN_LEN = 5 dans le segment_writer (force split)
3. Créer un petit test avec les docs/queries du tableau ci-dessus
4. Vérifier que le single-token SFX marche pour les queries intra-token

### Phase 1 : Approche 1 (split exhaustif)

1. Implémenter `cross_token_search(query, sfx_reader, resolver)` :
   - Single token d'abord
   - Si échec : loop sur les splits, collecter ordinals
   - Prendre le meilleur split, resolver postings filtrés
   - Intersecte par adjacence
2. Tester sur tous les cas du tableau
3. Mesurer les perfs (nombre de walks, temps)

### Phase 2 : Approche 2 (walk qui tombe)

1. Vérifier que lucivy-fst expose les transitions byte par byte
2. Implémenter le fall_point detection
3. Comparer les résultats et perfs avec l'approche 1
4. Si le FST ne l'expose pas : implémenter via binary search sur prefix_walk

### Phase 3 : Multi-split

1. Tester les queries qui chevauchent 3+ tokens ("getElementBy")
2. Implémenter la récursion si nécessaire
3. Valider sur le tableau de test

### Phase 4 : Remonter le MAX_TOKEN_LEN

1. Passer à 256 (valeur de force_split_long actuelle)
2. Tester que CamelCase-like words matchent naturellement via SFX
3. Le cross-token search devient un fallback rare (seulement pour les très longs tokens)
4. Bench 90K pour valider les perfs

---

## Amélioration clé : token_len dans ParentEntry

### Problème actuel

Le SFX ParentEntry stocke `(ordinal, si)`. Pour vérifier l'adjacence entre deux
côtés d'un split, il faut **résoudre les posting lists** pour obtenir les positions
(doc_id, token_index). C'est là que la mémoire explose.

### Solution : ajouter token_len

```rust
// Avant
pub struct ParentEntry {
    pub raw_ordinal: u64,
    pub si: u16,
}

// Après
pub struct ParentEntry {
    pub raw_ordinal: u64,
    pub si: u16,
    pub token_len: u16,  // +2 bytes par entrée
}
```

### Ce que ça permet

Avec `(ordinal, si, token_len)` on peut **classifier chaque match géométriquement**
sans toucher aux posting lists :

```
left  → ParentEntry { ordinal: X, si: 3, token_len: 8 }
right → ParentEntry { ordinal: Y, si: 0, token_len: 5 }
len(left_query) = 4, len(right_query) = 3
```

#### Cas 1 : Intra-token (les deux côtés sont dans le MÊME token)

```
left.ordinal == right.ordinal
AND left.si + len(left_query) == right.si
```

Vérification purement sur les ordinals — **zéro posting lookup**.
Le match entier est dans un seul token. On prend ses postings une fois.

#### Cas 2 : Inter-token (left = fin d'un token, right = début du suivant)

```
left.si + len(left_query) == left.token_len    // left consomme jusqu'au bout
AND right.si == 0                               // right commence au début
AND left.ordinal != right.ordinal               // tokens différents
```

Seul ce cas nécessite un posting lookup pour vérifier l'adjacence de position (N, N+1).
Mais on a déjà filtré les candidats : seuls les ordinals où left atteint la fin ET
right commence au début sont retenus. Le nombre de candidats est minimal.

#### Cas 3 : Impossible (filtre géométrique)

```
left.si + len(left_query) < left.token_len     // left ne va pas jusqu'au bout
AND right.si == 0                               // right commence au début d'un AUTRE token
→ IMPOSSIBLE : il y a un trou entre la fin du match left et la fin du token
```

```
left.si + len(left_query) == left.token_len     // left va jusqu'au bout
AND right.si > 0                                // right commence au MILIEU d'un token
→ IMPOSSIBLE : inter-token mais right ne commence pas au début
```

Ces cas sont éliminés sans aucun I/O.

### Algorithme avec token_len

```
Pour chaque split i de la query Q :
  left_parents  = resolve_suffix(Q[0..i])      → Vec<ParentEntry> avec token_len
  right_parents = prefix_walk(Q[i..])           → Vec<ParentEntry> avec token_len

  // Filtre intra-token (zéro I/O)
  Pour chaque left_p dans left_parents :
    Pour chaque right_p dans right_parents :
      Si left_p.ordinal == right_p.ordinal
         AND left_p.si + i == right_p.si :
        → MATCH intra-token ! Collecter (left_p.ordinal)

  // Filtre inter-token (géométrique, zéro I/O)
  left_candidates  = left_parents.filter(|p| p.si + i == p.token_len)
  right_candidates = right_parents.filter(|p| p.si == 0)

  Si left_candidates.is_empty() || right_candidates.is_empty() : skip

  // Seulement maintenant : posting lookup (filtré)
  left_postings  = resolve_postings(left_candidates, pivot_doc_ids)
  right_postings = resolve_postings(right_candidates, pivot_doc_ids)
  intersect by (doc_id, position N, position N+1)
```

### Coût

- Le filtre géométrique élimine 90%+ des candidats avant posting lookup
- L'intra-token est résolu sans AUCUN I/O (juste comparaison d'ordinals)
- Le posting lookup ne se fait que sur les candidats inter-token survivants
- +2 bytes par entrée SFX (token_len u16) = ~0.5% overhead sur l'index

### Impact sur les deux approches

**Approche 1 (split exhaustif)** : le filtre géométrique rend chaque split quasi-gratuit.
On peut tester les L splits sans coût. Seul le split gagnant fait un posting lookup.

**Approche 2 (walk qui tombe)** : le token_len aide à valider le fall_point.
Quand le walk tombe à position i, on vérifie que `si + i == token_len` (le match
atteint bien la fin du token). Si non, le fall_point est invalide.

### Implémentation

1. Modifier `ParentEntry` dans `suffix_fst/builder.rs` : ajouter `token_len: u16`
2. Modifier `SuffixFstBuilder::add_token()` : passer `token.len()` comme token_len
3. Modifier `encode_parent_entries` / `decode_parent_entries` : encoder/décoder le u16
4. Modifier `SfxFileReader::decode_parents()` : lire le token_len
5. Les consumers (suffix_contains, etc.) utilisent token_len pour le filtre géométrique

Le format SFX est interne (pas de backward compat) → on peut changer sans migration.

---

## Questions ouvertes

1. Le force-split à N chars préserve-t-il les offsets correctement pour les highlights ?
2. Le GapMap stocke-t-il le gap entre tokens issus d'un force-split ? (devrait être "" = vide)
3. Faut-il un CamelCaseSplit pour produire des tokens "sémantiques" (meilleur BM25) ?
   Ou le SFX + cross-token suffit-il pour le search, et le BM25 travaille sur les tokens bruts ?
4. Performance du SFX sur des tokens de 256 chars : combien de suffixes ? Impact mémoire ?
   Un token de 256 chars = 256 entrées SFX. 1000 tokens uniques × 256 = 256K entrées.
   Comparable à l'actuel (1000 tokens × ~10 chars × merge = ~10K entrées, mais moins de tokens).
