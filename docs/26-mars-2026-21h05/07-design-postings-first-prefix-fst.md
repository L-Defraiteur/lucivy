# Doc 07 — Postings-first + idée prefix FST à query time

Date : 27 mars 2026
Branche : `feature/cross-token-search`

## Problèmes structurels identifiés

### Bug: filtered resolve cache coherence

Le cache `ordinal_cache` est partagé entre branches du graph walk.
Si branche A resolve ordinal X filtré par doc_ids {1,2} et le cache,
branche B qui veut X avec doc_ids {3,4} obtient le résultat de A → perte de résultats.

Solution : ne pas filtrer au cache. Filtrer au moment du join.

### Bottleneck : resolve postings dans la boucle récursive

Le walk récursif fait :
```
pour chaque noeud × chaque candidate → resolve ordinal → join → recurse
```

C'est "graph-first, postings-lazy". Le graph est petit (5-7 noeuds, ~50 ordinals)
mais chaque resolve est un I/O (lecture sfxpost). En WASM c'est cher.

## Design A : Postings-first

### Principe

Inverser l'ordre : résoudre TOUT d'abord, valider ensuite.

```
1. Phase 1 : build graph (inchangé, FST walks only) → collect all ordinals
2. Phase 2 : resolve ALL ordinals in one batch
3. Phase 3 : index par doc_id → [(ordinal, position, byte_from, byte_to)]
4. Phase 4 : pour chaque doc_id, scanner si une chaîne valide couvre la query
```

### Phase 1 : graph build (inchangé)

```rust
graph: HashMap<String, SplitNode>
// Chaque SplitNode a candidates: Vec<SplitCandidate> et terminal: Option<...>
```

En plus, on collecte tous les ordinals :
```rust
let mut all_ordinals: HashSet<u64> = HashSet::new();
for node in graph.values() {
    for cand in &node.candidates { all_ordinals.insert(cand.parent.raw_ordinal); }
    if let Some(terminal) = &node.terminal {
        for (_suffix, parents) in terminal {
            for p in parents { all_ordinals.insert(p.raw_ordinal); }
        }
    }
}
```

### Phase 2 : batch resolve

```rust
let mut postings_by_ordinal: HashMap<u64, Vec<RawPostingEntry>> = HashMap::new();
for &ord in &all_ordinals {
    postings_by_ordinal.insert(ord, raw_term_resolver(ord));
}
```

Un seul pass, pas de cache, pas de filtre.

### Phase 3 : index par doc_id

```rust
struct DocPosting {
    ordinal: u64,
    position: u32,
    byte_from: u32,
    byte_to: u32,
}

let mut doc_index: HashMap<u32, Vec<DocPosting>> = HashMap::new();
for (ord, postings) in &postings_by_ordinal {
    for p in postings {
        doc_index.entry(p.doc_id).or_default().push(DocPosting {
            ordinal: *ord, position: p.token_index,
            byte_from: p.byte_from, byte_to: p.byte_to,
        });
    }
}
// Trier chaque doc par position
for entries in doc_index.values_mut() {
    entries.sort_by_key(|e| (e.position, e.byte_from));
}
```

### Phase 4 : scan par doc_id

Pour chaque doc_id, on a une liste triée de postings (position, byte_from, byte_to, ordinal).
On cherche une séquence de positions consécutives qui :
1. Couvre la query (la somme des token_len == query.len())
2. Est byte-contiguë (byte_to[i] == byte_from[i+1])
3. Chaque posting correspond à un split valide du graph

C'est un scan linéaire O(N) par doc, où N = nombre de postings dans le doc.
Pas de HashMap, pas de recursion.

### Avantages

- **Un seul resolve** par ordinal (pas de cache coherence issues)
- **Pas de HashMap dans la boucle chaude** — l'index par doc est construit une fois
- **Pas de recursion** — le scan est linéaire
- **Cache-friendly** — accès séquentiel au Vec trié

### Inconvénients

- On resolve TOUS les ordinals même ceux qui ne matcheront aucun doc.
  Avec ~50 ordinals pour "getElementById", c'est acceptable.
- L'index par doc_id utilise de la mémoire temporaire.
  Avec 846 docs et ~50 ordinals, chaque ordinal a ~50-100 entries
  → ~5000 DocPostings max. Négligeable.

### Complexité

- Phase 1 : O(unique_remainders × L) — inchangé
- Phase 2 : O(|all_ordinals| × avg_posting_size) — un seul pass
- Phase 3 : O(total_postings × log) — build + sort
- Phase 4 : O(|docs| × avg_postings_per_doc) — scan linéaire
- Total : O(total_postings) dominé par le resolve, comme avant,
  mais sans overhead HashMap/recursion

## Design B : Prefix FST à query time

### L'idée

Le SFX FST indexe les **suffixes** : étant donné une substring, on trouve
quel token la contient. C'est le "suffix → parent" lookup.

Pour le cross-token, on a besoin du lookup inverse : étant donné un **prefix**
de la query, trouver quel token **commence** par ce prefix. C'est le
"prefix → next token" lookup — exactement ce que `prefix_walk_si0` fait.

Mais `prefix_walk_si0` ne fait qu'un walk à la fois. Si on construisait un
**mini-FST à query time** qui contient tous les tokens candidats avec leurs
positions, on pourrait faire le matching en une seule traversée.

### Comment ça marcherait

1. Phase 1 (graph build) → on connaît tous les ordinals candidats
2. Batch resolve → on connaît tous les (ordinal, doc_id, position, byte_from, byte_to)
3. Pour chaque token candidate, on connaît son texte (via l'ordinal → term dict lookup)
4. **Construire un mini-FST** : clé = token text, valeur = liste de (doc_id, position, byte_from, byte_to)
5. **Walk le mini-FST** avec la query : le FST matche naturellement les préfixes

### Problème

On n'a pas accès au texte des tokens depuis l'ordinal dans le SFX.
L'ordinal SFX n'est pas l'ordinal du term dict standard — c'est un ordinal
de tri BTreeSet des suffixes. Pour retrouver le texte, il faudrait un reverse
lookup ordinal → term, qui n'existe pas dans le format actuel.

De plus, le FST qu'on construirait à query time serait minuscule (~50 entrées)
— un Vec trié + binary search serait aussi rapide et beaucoup plus simple.

### Variante : pas un FST mais un trie léger

Au lieu d'un FST complet, construire un simple **trie** (arbre de préfixes)
des tokens candidats. Chaque noeud du trie porte les postings associés.

Traverser la query byte par byte dans le trie :
- Quand on atteint une feuille (fin de token candidat) : vérifier byte continuity
  puis continuer dans le trie depuis la racine pour le token suivant.
- C'est essentiellement ce que fait le cross-token search mais en une seule
  structure au lieu de falling_walk + graph + walk récursif.

C'est élégant mais revient au même que le design A (postings-first)
avec une structure de données différente pour la validation.

### Verdict

L'idée du prefix FST est bonne conceptuellement — c'est l'inverse du SFX.
Mais pour le cross-token à query time avec ~50 candidats, le design A
(postings-first avec index par doc_id) est plus simple et aussi performant.

Si un jour on avait besoin d'un prefix index PERSISTANT (pas à query time),
ce serait un vrai gain — mais ça nécessiterait de modifier le format d'indexation.

## Recommandation

1. **Fixer le bug** de cache coherence (retirer le filtre du or_insert_with)
2. **Implémenter le design A** (postings-first) — plus simple, plus rapide, plus correct
3. Garder l'idée du prefix FST/trie pour plus tard si le design A ne suffit pas
