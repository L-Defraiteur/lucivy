# Doc 10 — Issues et réflexions de la session du 25 mars 2026

## 1. CamelCaseSplitFilter : max 2 chunks merged (implémenté)

### Solution retenue

Forward merge si chars < 4, mais **max 2 raw chunks fusionnés** (`MAX_MERGED_CHUNKS=2`).
Pas de backward merge. Implémenté dans `split_and_merge()`.

```
"ag3weaver" → [ag, 3, weaver] → ag3(2 chunks, flush) + weaver → ["ag3", "weaver"] ✓
"rag3weaver" → [rag, 3, weaver] → rag3(2 chunks, flush) + weaver → ["rag3", "weaver"] ✓
"rag3wea" → [rag, 3, wea] → rag3(2 chunks, flush) + wea → ["rag3", "wea"] ✓
"getX" → [get, X] → getX(2 chunks, flush) → ["getX"] ✓
"getElementById" → [get, Element, By, Id] → ["getElement", "ById"] ✓
```

### Optimisation future : pivot-first multi-token

Actuellement le multi-token fait un SFX walk pour CHAQUE token de la query (Step 1),
puis pivot sur le plus sélectif (Step 2). Les walks de tokens courts ("3", "db") coûtent
cher car ils matchent énormément d'ordinals.

Optimisation proposée :
1. Walk SFX seulement le pivot (token le plus long/sélectif)
2. Extraire `pivot_doc_ids: HashSet<DocId>`
3. Pour les autres tokens : term dict lookup (exact, pas SFX) + filtre par `pivot_doc_ids`
4. Intersection par position (triviale car pré-filtrée)

Ça rendrait les tokens courts gratuits : pas de walk SFX, juste un lookup filtré.

## 1bis. Ancien problème (résolu)

### Problème actuel

Le forward merge (`MIN_CHUNK_CHARS=4`) absorbe les petits chunks dans le suivant.
Ça cause des incohérences entre la query tokenisée et les tokens indexés quand
la query est un substring du mot original :

```
Index : "rag3weaver" → ["rag3", "weaver"]   (rag<4, merge → rag3, ≥4 flush)
Query : "ag3weaver"  → ["ag3weaver"]         (ag<4, merge → ag3<4, merge → tout)
```

"ag3weaver" cherché comme 1 token → pas trouvé. Même en forçant le split,
le merge absorbe les petits chunks et empêche le multi-token.

### Proposition : retirer tout merge

Sans aucun merge, split à chaque boundary digit↔lettre et camelCase :

```
Index : "rag3weaver" → ["rag", "3", "weaver"]
Query : "ag3weaver"  → ["ag", "3", "weaver"]
Query : "rag3wea"    → ["rag", "3", "wea"]
Query : "rag3db"     → ["rag", "3", "db"]
Query : "getX"       → ["get", "X"]
Query : "getElementById" → ["get", "Element", "By", "Id"]
```

### Pourquoi c'est safe

Le multi-token search **pivot sur le token le plus sélectif** (le moins de postings).
Les petits tokens ("3", "X", "db") ont beaucoup de matches mais ne sont jamais
le pivot — ils sont juste vérifiés par position adjacente (binary search, O(log n)).

```
"ag3weaver" → ["ag", "3", "weaver"]
  pivot = "weaver" (le plus sélectif)
  check "3" at pos-1 → binary search → O(log n)
  check "ag" at pos-2 → binary search → O(log n)
  → match ✓
```

### Avantages

- **Plus simple** : pas de logique de merge, pas de MIN_CHUNK_CHARS
- **Plus prévisible** : le split dépend uniquement des boundaries, pas de la longueur
- **Cohérent** : même résultat à l'indexation et à la query, toujours
- **Meilleur substring matching** : les substrings cross-token marchent naturellement

### Risques

- **Index existants** : changement de tokenisation → faut reindexer
- **Plus de tokens par doc** : "getElementById" passe de 2 tokens à 4. Impact sur
  la taille du term dict et des posting lists. Probablement négligeable car les
  tokens courts sont très communs (déjà dans le dict) et les posting lists sont compressées.
- **SFX FST plus gros** : plus de tokens = plus de suffixes. À mesurer.

### À faire

1. Retirer `split_and_merge()`, remplacer par split brut + force_split_long
2. Mettre à jour les tests
3. Mesurer l'impact sur la taille de l'index et les performances (bench 90K)
4. Tester "ag3weaver", "rag3wea", "getElementById" dans le playground

---

## 2. Query cancellation

### Problème

Le playground fait du search-as-you-type. Le worker WASM traite les messages
séquentiellement. Si la première query est lente (33s), toutes les suivantes
attendent derrière.

Le JS a un `searchGeneration` qui jette les résultats périmés, mais le Rust
tourne quand même jusqu'à la fin.

### Proposition : CancellationToken

```rust
// luciole/src/cancellation.rs
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self { Self(Arc::new(AtomicBool::new(false))) }
    pub fn cancel(&self) { self.0.store(true, Ordering::Relaxed); }
    pub fn is_cancelled(&self) -> bool { self.0.load(Ordering::Relaxed) }
}
```

Intégration dans luciole :
- `NodeContext` porte un `Option<CancellationToken>`
- Le runtime check `is_cancelled()` entre chaque noeud
- Les noeuds longs (SfxWalk, search) checkent périodiquement

Intégration dans le SFX walk :
```rust
// Dans prefix_walk_with_byte, tous les N entries :
while let Some((key, val)) = stream.next() {
    if cancel.is_cancelled() { break; }
    // ...
}
```

Intégration côté JS :
```js
// Avant chaque nouvelle recherche :
worker.postMessage({ type: 'cancel' });
worker.postMessage({ type: 'search', ... });
```

Côté Rust (emscripten), un `static CANCEL_TOKEN: AtomicBool` que `lucivy_cancel()`
set à true. Le search le check et return early.

### À faire

1. Ajouter `CancellationToken` à luciole
2. Intégrer dans le DAG runtime
3. Ajouter check dans le SFX walk
4. Exposer `lucivy_cancel()` dans le binding emscripten
5. Câbler côté JS dans le worker

---

## 3. Playground : default fuzzy distance = 0

### Problème

Le playground démarre avec `distance = 1` (fuzzy). Ça veut dire que même un
search simple comme "mutex" retourne des faux positifs (mots à distance 1).

Pour du code search, le mode exact (`distance = 0`) est plus pertinent par défaut.
Le fuzzy devrait être opt-in.

### Fix

Changer la valeur par défaut du champ distance de 1 à 0 dans le HTML.

---

## 4. Playground : mode par défaut = contains, pas contains_split

### Problème

Le mode `contains_split` sépare la query par espaces et fait un boolean should
pour chaque mot. C'est bien pour du langage naturel mais pour du code search,
on veut souvent chercher une expression exacte avec espaces :

- `"struct device"` → contains_split cherche "struct" OU "device" (trop large)
- `"struct device"` → contains cherche "struct device" comme phrase (exact)

### Fix

Changer le mode par défaut de `contains_split` à `contains` dans le HTML.
L'utilisateur peut toujours switcher manuellement.

---

## 5. Résumé de la session 25 mars

### Ce qui a été fait

- **lucistore** : crate extraite (BlobStore, BlobCache, ShardStorage, LUCE/LUCID/LUCIDS, SyncServer, DeltaExporter)
- **LUCID/LUCIDS** : incremental delta sync single + sharded, 25 tests lucistore + 3 E2E
- **SyncServer** : historique versions, delta dispatch
- **DeltaExporter** : trait générique pour delta export
- **DiagBus** : 4 events restants câblés (SuffixAdded, SfxWalk, SfxResolve, MergeDocRemapped)
- **Bindings** : sfx flag exposé (emscripten + python), SchemaConfig complet dans lucivy_create
- **Playground** : modes nettoyés, strict_separators exposé, batch indexing
- **tokenize_query** : CamelCaseSplitFilter ajouté (même tokenizer query/index)
- **Backward merge retiré** du CamelCaseSplitFilter
- **strict_separators** : false par défaut, skip GapMap en multi-token
- **Bug fix** : continuation activée pour contains queries

### Ce qui reste à faire demain

1. Retirer complètement le merge du CamelCaseSplitFilter (section 1)
2. CancellationToken pour query cancel (section 2)
3. Playground defaults : distance=0, mode=contains (sections 3-4)
4. Rebuild WASM + .luce + tester
5. Investiguer pourquoi certains premiers search sont lents (warm-up?)
6. Extensions de fichiers manquantes dans le playground (4k vs 5k fichiers)
