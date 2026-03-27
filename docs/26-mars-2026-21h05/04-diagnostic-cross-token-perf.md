# Doc 04 — Diagnostic cross-token perf + pistes de simplification

Date : 26 mars 2026
Branche : `feature/cross-token-search`

## Problème constaté

"rag3w" sur le .luce (846 fichiers source lucivy) → OOM en WASM (memory allocation failed).
Le worklist multi-split explose même avec `continue` sur terminal match et `MAX_WORKLIST=256`.

La cause probable : "rag3w" génère beaucoup de falling_walk candidates (suffixes courts
qui tombent sur des frontières de tokens), et chaque remainder génère encore des candidates.
Même sans recursion profonde, le nombre de candidates × postings resolved explose la mémoire.

## Plan de diagnostic

### 1. Instrumenter le worklist

Ajouter des eprintln! temporaires dans `cross_token_search` pour mesurer :

```rust
eprintln!("[cross_token] initial candidates: {}", initial_candidates.len());
// Dans la boucle while :
eprintln!("[cross_token] depth={}, worklist={}, chains={}, remainder='{}'",
    depth, worklist.len(), chains.len(), remainder);
```

### 2. Tester en natif (pas WASM) avec le .luce

Écrire un test Rust qui charge le .luce et fait la recherche "rag3w" :

```bash
cargo test --lib -p ld-lucivy test_luce_rag3w -- --nocapture 2>&1 | head -100
```

Ou mieux : un petit programme `examples/debug_cross_token.rs` qui charge le .luce
et time la recherche.

### 3. Compter les falling_walk candidates pour "rag3w"

Le falling_walk de "rag3w" retourne TOUS les suffixes dans le FST qui :
- sont des préfixes de "rag3w"
- atteignent une frontière de token (si + prefix_len == token_len)

Pour "rag3w" (5 bytes), les candidates possibles sont :
- prefix_len=1 : "r" → fin de quel token ? Probablement aucun (token de 1 char)
- prefix_len=2 : "ra" → fin de quel token ? Probablement aucun
- prefix_len=3 : "rag" → si un token "rag" existe... (dans le code lucivy? non)
- prefix_len=4 : "rag3" → OUI! "rag3" est un token (CamelCaseSplit de "rag3db" etc.)

Mais aussi les suffixes dans la partition SI>0 :
- "ag3w" → "ag3" est un suffixe de "rag3" à SI=1, token_len=4 → si(1)+3=4 ✓
- "g3w" → "g3" suffixe de "rag3" SI=2, token_len=4 → 2+2=4 ✓
- "3w" → "3" suffixe de "rag3" SI=3, token_len=4 → 3+1=4 ✓

Donc on a probablement ~4 candidates pour "rag3w". C'est pas beaucoup.
Le remainder serait "w" (1 byte). prefix_walk_si0("w") → TOUS les tokens qui commencent
par "w". Ça pourrait être beaucoup (weaver, writer, write, with, ...) → le terminal match
est trouvé → `continue`, pas de recursion.

**Alors pourquoi ça explose ?** Peut-être que c'est la résolution de postings qui explose :
chaque ordinal résolu charge toutes les positions de ce token dans tous les documents.
Avec 846 fichiers et des tokens communs, ça fait beaucoup d'allocations.

**Hypothèse alternative** : le problème n'est pas le worklist mais le **resolve + adjacency**
avec trop de postings × trop de right walks.

### 4. Vérifier avec eprintln

Ajouter avant la boucle d'adjacency :
```rust
let total_chains = chains.len();
let total_splits: usize = chains.iter().map(|c| c.splits.len()).sum();
let total_right_walks: usize = chains.iter().map(|c| c.right_walks.len()).sum();
eprintln!("[cross_token] chains={}, total_splits={}, total_right_walks={}",
    total_chains, total_splits, total_right_walks);
```

## Pistes de simplification

### Piste A : Supprimer CamelCaseSplit, cross-token simple uniquement

**Idée** : ne plus merger les tokens courts à l'indexation. Chaque token du SimpleTokenizer
reste tel quel. Le cross_token_search ne gère que le cas basique : query traverse
exactement 2 tokens adjacents.

**Avantages** :
- Plus de chunks courts ("db", "3") qui génèrent plein de falling_walk candidates
- Pas de multi-split nécessaire (les tokens longs ne sont jamais fragmentés)
- La query "rag3weaver" → falling_walk trouve "rag3" | "weaver" directement
- Plus simple, plus prévisible

**Inconvénients** :
- Tokens très courts ("db", "3", "is") polluent l'index (beaucoup de postings)
- Le SFX a plus d'entrées (chaque petit token a ses suffixes)
- Taille index augmente

**Variante** : garder un split mais seulement pour les tokens très longs (>256 bytes).
Pas de CamelCaseSplit, pas de digit-split, juste du force-split sur les monstres.

### Piste B : Garder CamelCaseSplit mais limiter le cross-token à 1 split

**Idée** : le multi-split worklist est dangereux. On revient à 1 seul split (left|right)
mais on s'assure que c'est suffisant pour 99.9% des cas avec le bon tokenizer.

Si CamelCaseSplit produit au max 2 chunks mergés (MAX_MERGED_CHUNKS=2), alors
la plupart des queries traversent au max 2 tokens. Le cas 3+ tokens est ultra-rare
et peut être géré par RegexContinuationQuery (plus lent mais correct).

### Piste C : falling_walk amélioré avec early termination

**Idée** : le falling_walk retourne les candidates triés par prefix_len DESC.
Le premier candidate (le plus long split) est presque toujours le bon.
On peut limiter à N candidates (genre top 3) et ignorer les splits trop courts.

**Filtre** : skip les candidates où prefix_len < MIN_SPLIT_LEN (genre 2 ou 3).
Un split à 1 byte ("r" | "ag3w") n'a aucun sens — le left token serait juste "r".

### Piste D : cross-token inversé (remainder-first)

**Idée** : au lieu de falling_walk(query) → remainder,
faire prefix_walk_si0(query) d'abord pour trouver quel token commence le query.
Si aucun ne commence exactement, alors seulement essayer le falling_walk.

Ça évite de générer des candidates quand le single-token marche déjà.
(Note : c'est déjà le cas — cross_token_search est un fallback.)

### Piste E : ne pas recurser, period

Si le premier falling_walk + prefix_walk ne trouve rien → **abandonner**.
Le multi-split est rare et coûteux. Pour les cas extrêmes (3+ tokens),
la RegexContinuationQuery existe déjà (elle est juste plus lente).

On pourrait même détecter le cas "remainder ne commence aucun token ET
remainder n'a aucun falling_walk candidate" → return empty immédiatement.

## Recommandation

**Court terme** : Piste E (pas de recursion) + Piste C (limiter candidates).
Revenir à un seul falling_walk + prefix_walk, pas de worklist.
Ajouter un filtre `prefix_len >= 2` pour éviter les splits absurdes.

**Moyen terme** : Piste A ou B. Revoir la tokenisation pour que le cross-token
simple (2 tokens) couvre 99.9% des cas. Le multi-split est un problème de tokenisation,
pas de search.

**Long terme** : Si multi-split est vraiment nécessaire, l'implémenter via
RegexContinuationQuery (DFA continu) qui est déjà correct mais plus lent.
Optimiser sa performance (gap-aware DFA caching).
