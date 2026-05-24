# Design : Sibling Table v3 — Chain building sans dépendance FST

## Contexte

Le falling walk v3 dépend de la **finalité des noeuds FST** pour détecter
les splits. Quand l'index grandit, des noeuds finaux disparaissent → splits
perdus → FN. Les tentatives de fix query-time (DFS look-ahead, split table,
content-only keys) ont toutes échoué (explosion de temps ou complexité).

La v2 avait résolu ce problème proprement avec la **sibling table** :
une table index-time qui encode directement "quel ordinal suit quel ordinal".
Le chain builder n'a pas besoin de re-walker le FST pour chaque step —
il suit les liens de la sibling table.

## Principe

Reprendre le format exact de la sibling table v2 (`ordinal → [next_ordinals]`)
et le peupler pour les deux niveaux :

- **Chunk siblings** : chunk_ord → [next_chunk_ord] (contiguïté physique)
- **Word siblings** : word_ord → [next_word_ord] (adjacence mot-à-mot)

**UNE seule table** indexée par ordinal. Les chunk ordinals et word ordinals
cohabitent (ils ont des ranges d'ordinals disjoints grâce au tri alphabétique
du BTreeMap dans into_data).

## Format

Identique à la v2 `sibling_table.rs` :

```
Header:
  num_ordinals: u32

Offset table:
  offsets: [u32; num_ordinals + 1]  // byte offset dans entries_data

Entries (variable length par ordinal):
  Séquence de SiblingEntry:
    next_ordinal: u32
    gap_len: u16    // 0 = contiguous (cross-token/cross-word viable)
```

Taille : `4 + (N+1)*4 + M*6` bytes où N = ordinals, M = total paires.
Pour 100K ordinals avec ~2 siblings/ord moyen : ~1.6 MB. Acceptable.

## Construction (index-time)

### Chunk siblings

Dans `collector_v3.rs`, pendant `add_value()`, les chunks sont produits
en séquence. Pour chaque paire de chunks consécutifs (i, i+1) dans la
même value :

```rust
sibling_writer.add(
    intern_to_final[chunk_i_intern],
    intern_to_final[chunk_i1_intern],
    0, // gap_len = 0 (contiguous, overlap covers the boundary)
);
```

C'est exactement ce que fait le collector v2 pour sa sibling table.

### Word siblings

Pour chaque paire de mots adjacents dans la même value, on ajoute un
lien entre le word-stripped ordinal du mot courant et celui du mot suivant :

```rust
sibling_writer.add(
    intern_to_final[word_a_ws_intern],
    intern_to_final[word_b_ws_intern],
    0, // gap_len = 0 (séparateur ignoré en mode relaxed)
);
```

Les word-stripped ordinals sont ceux de la partition 0x02. Le lien
word sibling encode : "le mot A peut être suivi du mot B dans le corpus".

## Utilisation (query-time)

### Algorithme (identique à v2)

```
1. falling_walk(query) → premier split (ordinal, split_byte)
   OU fst_candidates(query) → ordinals single-token

2. Pour chaque ordinal trouvé :
   remainder = query[split_byte..]
   
3. DFS via sibling table :
   stack = [(ordinal, remainder, [ordinal])]
   
   while stack non vide :
     (cur_ord, rem, chain) = stack.pop()
     
     if rem.is_empty() :
       → chain complète, émettre
     
     for next_ord in sibling_table[cur_ord] :
       next_text = term_dict[next_ord]  // O(log N)
       next_content = next_text[..content_len(next_ord)]
       
       if rem starts_with next_content :
         → consommation partielle, push (next_ord, rem[content_len..], chain+[next_ord])
       elif next_content starts_with rem :
         → le sibling couvre tout le remainder, émettre chain
```

### Partition chunk (0x00/0x01) — strict adjacency

Le chain builder chunk utilise les chunk siblings :
- Le falling walk trouve le premier split dans 0x00/0x01
- La sibling table donne les chunks suivants possibles
- Comparaison textuelle du remainder avec le texte du sibling

**Élimine le besoin de** : markers, best_consumed, fst_candidates pour la continuation.

### Partition word (0x02) — relaxed adjacency

Le chain builder word utilise les word siblings :
- Le falling walk trouve le premier split dans 0x02
- La sibling table donne les mots suivants possibles
- Comparaison textuelle du remainder avec le content du sibling

**Élimine le besoin de** : DFS look-ahead, content-only keys, split table.

**Résout le bug uint64_t** : le falling walk trouve "uint64" au noeud
final (SI=0, key `\x02uint64...`). Même si le noeud n'est pas final
(overlap masque le split), les word siblings de "uint64" incluent "to",
"type", etc. Le remainder "t" matche le début de "to" → chain trouvée.

Attendons — le falling walk doit QUAND MÊME trouver le premier split.
Si le noeud au content boundary n'est pas final, le falling walk ne
trouve pas l'ordinal. Comment la sibling table aide-t-elle ?

### Le problème du premier split

La sibling table aide pour les CONTINUATIONS (steps 2+), pas pour le
premier split. Le premier split vient du falling walk qui dépend de la
finalité FST.

**Solutions pour le premier split** :

**Option A** : fst_candidates comme source du premier split.
fst_candidates(query) trouve toutes les clés dont le query est préfixe.
Pour "uint64t", il trouve "uint64to" (ordinal X). On sait que le
content_len de X est 6 (via parent entry). query_len=7 > 6 → split.
Puis la sibling table prend le relais pour la continuation.

Coût : O(log N + K) pour UN appel fst_candidates (déjà fait pour les
single-token matches). Zéro overhead supplémentaire.

**Option B** : la sibling table elle-même comme source du premier split.
Au lieu du falling walk, on utilise fst_candidates pour trouver les
ordinals candidats, puis pour chaque ordinal on vérifie via
content_len si c'est un split, et on suit les siblings.

→ Option A est la plus simple : fst_candidates est DÉJÀ appelé dans
find_literal_v3. Il suffit de checker les candidats pour des splits.

## Flow complet

```
find_literal_v3(query):

  1. candidates = fst_candidates(query)          // déjà fait
  2. single_token_matches = resolve(candidates)   // déjà fait

  3. // Chunk chains (0x00/0x01)
     chunk_splits = falling_walk_chunks(query)     // comme avant
     // NOUVEAU : splits supplémentaires depuis fst_candidates
     for cand in candidates where partition == 0x00/0x01 :
       if query.len() > content_len(cand) - sti :
         chunk_splits.add(cand)
     chunk_chains = sibling_chain_dfs(chunk_splits, chunk_siblings)
     resolve_chains(chunk_chains)

  4. // Word chains (0x02)
     word_splits = falling_walk_words(query)       // comme avant
     // NOUVEAU : splits supplémentaires depuis fst_candidates
     for cand in candidates where partition == 0x02 :
       if query.len() > content_len(cand) - sti :
         word_splits.add(cand)
     word_chains = sibling_chain_dfs(word_splits, word_siblings)
     resolve_word_chains(word_chains)
```

## Ce qui change vs l'actuel

| Composant | Avant | Après |
|-----------|-------|-------|
| Continuation chains | falling_walk + fst_candidates par step | sibling table DFS (O(1) lookup) |
| Premier split (raté) | perdu si FST noeud non-final | rattrapé par fst_candidates + content_len check |
| best_consumed | filtre glouton query-time | inutile (siblings sont pré-filtrés) |
| markers FST | nécessaires pour splits chunk | toujours utiles pour le falling walk, mais plus critiques |
| Postings resolve | inchangé | inchangé |

## Fichiers à modifier

| Fichier | Changement |
|---------|------------|
| `collector_v3.rs` | Construire la sibling table dans `into_data()` (chunk + word pairs) |
| `sfx_dag_v3.rs` | Ajouter "sibling_v3" aux registry_files |
| `index_registry.rs` | Enregistrer SiblingV3Index |
| `briques/composite.rs` | `find_literal_v3` : ajouter splits depuis fst_candidates + sibling DFS |
| `briques/fst_walk.rs` | Nouveau : `sibling_chain_dfs()` |
| `contains_query_v3.rs` | Charger sibling table + term dict |
| `fuzzy_query_v3.rs` | Idem |

## Résultat attendu

- Ground truth : **15/15** (uint64_t relaxed résolu par fst_candidates split + word siblings)
- Aucune dépendance à la finalité des noeuds FST pour les continuations
- Le falling walk reste utile comme "fast path" pour le premier split
- fst_candidates rattrape les premiers splits ratés
- Performance query : plus rapide (O(1) sibling lookup vs O(log N) fst_candidates par step)
- Performance index : ~1-2 MB de plus, construction O(N) triviale
