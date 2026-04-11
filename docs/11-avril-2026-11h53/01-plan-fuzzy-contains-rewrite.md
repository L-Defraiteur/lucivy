# Plan : Réécriture fuzzy contains — chemin dédié

Date : 11 avril 2026

## Contexte

Le pipeline fuzzy actuel est greffé sur le pipeline regex (RegexContinuationQuery).
Il partage la mécanique DFA, concat bytes, token spans, validate_path, etc.
Chaque correction ajoute une couche de complexité (word_ids, cross_word_count,
content_gap_count, tolerance heuristique, normalize_gaps, dual-anchor window...).

Résultat : code difficile à raisonner, 877ms sur "3db_val" d=1 à cause de
21000+ faux candidats qui passent l'intersect heuristique et déclenchent un DFA
walk chacun. Les tentatives de filtrage post-intersect (siblings, checkpoints)
soit trop lentes soit incorrectes.

## Décision

Écrire `fuzzy_contains` comme un chemin **indépendant** de regex contains.
Ne pas gérer le mode séparateurs stricts dans un premier temps.

## Principe fondamental

**Concaténer la query** en retirant tous les séparateurs. Traiter la query
comme une suite de bytes alphanumériques, point. Les trigrams ne savent pas
qu'il y avait des espaces ou des underscores.

Le resolve de chaque trigram utilise le falling walk avec **tous les siblings**
(pas juste contiguous). Donc un trigram comme "bva" à la frontière de
"rag3d**b**" et "**va**lue" est trouvé par le falling walk : "b" fin du token
→ sibling (avec gap) → "va" début du token suivant.

Ensuite, vérifier par les **positions** (token index dans le doc) que les
trigrams matchés sont dans une fenêtre compacte.

Pas de DFA. Pas de word_ids. Pas de cross-word logic. Pas de bridge segments.

## Pipeline

### Entrée

- `query_text: &str` — la query brute (ex: "rag3db_value_destroy")
- `distance: u8` — edit distance (1 ou 2 typiquement)
- `sfx_reader`, `resolver`, `ord_to_term` — accès à l'index

### Étape 1 : Concaténation de la query

```
"rag3db_value_destroy" → "rag3dbvaluedestroy"
"rag3db is cool"       → "rag3dbiscool"
"3db_val"              → "3dbval"
```

Retirer tout ce qui n'est pas alphanumeric. Lowercase.

### Étape 2 : Génération des trigrams

Sur la chaîne concaténée :
- Si `len >= 3*(d+1)+1` → trigrams (n=3)
- Sinon → bigrams (n=2)

Sliding window de taille n sur la chaîne concaténée.
Chaque trigram est taggé avec sa `query_position` (offset dans la chaîne
concaténée).

Exemple "3dbval" d=1 → n=2 (len=6, seuil=7) :
```
"3d" pos=0, "db" pos=1, "bv" pos=2, "va" pos=3, "al" pos=4
```

Exemple "rag3dbvaluedestroy" d=1 → n=3 (len=18, seuil=7) :
```
"rag" pos=0, "ag3" pos=1, "g3d" pos=2, "3db" pos=3,
"dbv" pos=4, "bva" pos=5, "val" pos=6, "alu" pos=7,
"lue" pos=8, "ued" pos=9, "ede" pos=10, "des" pos=11,
"est" pos=12, "str" pos=13, "tro" pos=14, "roy" pos=15
```

Note : "dbv" et "bva" sont des trigrams qui traversent la frontière
"rag3db"/"value". Le falling walk les résout via siblings.

### Étape 3 : Résolution par trigram

Pour chaque trigram, résoudre via les briques existantes :

1. `fst_candidates(sfx_reader, trigram)` — single-token matches
2. `cross_token_falling_walk(sfx_reader, trigram, 0, ord_to_term, allow_gaps=true)` — cross-token

**Modification clé** : `cross_token_falling_walk` doit utiliser
`siblings()` (tous les gaps) au lieu de `contiguous_siblings()` (gap=0).
Ajouter un paramètre `allow_gaps: bool` au falling walk.

3. `resolve_candidates` + `resolve_chains` avec filtrage sélectif par doc
   (résoudre les plus rares d'abord, comme aujourd'hui)

Résultat : `all_matches[i] = Vec<LiteralMatch>` par trigram.
Chaque LiteralMatch : `doc_id, position, byte_from, byte_to, si, ordinal`.

### Étape 4 : Construction du dictionnaire de matches par doc

Structure intermédiaire centrale — le "dico de matches" :

```rust
/// Un match de trigram à une position donnée dans un document.
struct TrigramHit {
    tri_idx: usize,       // quel trigram de la query (0, 1, 2, ...)
    position: u32,        // token index dans le doc (premier token du match)
    byte_from: u32,       // byte offset dans le contenu
    byte_to: u32,         // byte offset fin
    si: u16,              // suffix index dans le parent token
    /// Décomposition du trigram par token.
    /// Single-token : ["ag3"] (trigram entier dans un seul token)
    /// Cross-token  : ["b", "va"] (fin d'un token + début du suivant)
    /// Permet de savoir comment le trigram se distribue sur les tokens.
    token_parts: Vec<String>,
}

/// Tous les matches pour un doc, groupés par position.
type DocHits = HashMap<u32, Vec<TrigramHit>>;  // position → hits

/// Le dico complet.
type HitsByDoc = HashMap<DocId, DocHits>;
```

Construction :

```
Pour chaque trigram i :
  Pour chaque LiteralMatch m dans all_matches[i] :
    // Single-token : token_parts = [trigram_text]
    // Cross-token : token_parts vient du CrossTokenChain
    //   prefix_len bytes du premier token + remainder du second
    //   ex: "bva" avec prefix_len=1 → ["b", "va"]
    hits_by_doc[m.doc_id][m.position].push(TrigramHit {
        tri_idx: i,
        position: m.position,
        byte_from: m.byte_from,
        byte_to: m.byte_to,
        si: m.si,
        token_parts: ...,
    })
```

Pour les cross-token matches issus de `resolve_chains`, le `LiteralMatch`
actuel ne porte pas la décomposition. Il faut soit :
- Enrichir `LiteralMatch` avec un champ `token_parts`
- Soit séparer la résolution single-token et cross-token pour construire
  le `token_parts` au moment du resolve

Exemple concret pour "3db_val" dans un doc où "rag3db_value" est aux
positions 5-6 :

```
doc[42] = {
    pos 5: [
        { tri: "3d", tri_idx: 0, si: 3, bf: 103, parts: ["3d"] },
        { tri: "db", tri_idx: 1, si: 4, bf: 104, parts: ["db"] },
    ],
    pos 5: [  // cross-token, position = premier token
        { tri: "bv", tri_idx: 2, si: 5, bf: 105, parts: ["b", "v"] },
    ],
    pos 6: [
        { tri: "va", tri_idx: 3, si: 0, bf: 107, parts: ["va"] },
        { tri: "al", tri_idx: 4, si: 1, bf: 108, parts: ["al"] },
    ],
}
```

Note : "bv" cross-token a parts=["b", "v"] → "b" est la fin de "rag3db"
(si=5, 1 byte), "v" est le début de "value" (si=0, 1 byte). Le trigram
traverse la frontière token, le falling walk l'a trouvé via siblings(gap).

Ce dico est la seule source de vérité pour les étapes suivantes.

### Étape 5 : Filtrage par ancrage sur position

Threshold = max(total_trigrams - n*d, 2)

Nombre de tokens attendus dans le contenu :
```
"rag3db_value_destroy" → 3 mots → max_span = 3 + distance
"3db_val"              → 2 mots → max_span = 2 + distance
"rag3weaver"           → 1 mot  → max_span = 1 + distance
```

Pour chaque doc dans hits_by_doc :

1. Collecter toutes les positions qui ont des hits
2. Pour chaque position P dans le doc :
   - Regarder la zone `[P, P + max_span]`
   - Compter combien de `tri_idx` **distincts** apparaissent dans cette zone
   - Si ≥ threshold → **match trouvé**. Collecter les byte_from/byte_to
     des hits dans la zone pour le highlight.

Les matches chevauchants sont trouvés naturellement : si la même séquence
de tokens apparaît à position 10-12 et à position 50-52, les deux zones
produisent des matches indépendants.

Pas de "consommation" des trigrams : un même hit peut contribuer à
plusieurs matches (cas de tokens répétés dans le doc). On déduplique les
highlights identiques à la fin.

**Complexité** : O(P × max_span) par doc, avec P = nombre de positions
distinctes ayant des hits. Typiquement P < 20 et max_span < 5 → négligeable.

### Étape 6 : Calcul des highlights

Pour chaque match validé (doc_id + zone [P, P+max_span]) :

```
first_tri_idx = plus petit tri_idx dans la zone
last_tri_idx  = plus grand tri_idx dans la zone

hl_start = min(byte_from des hits dans la zone) - query_positions[first_tri_idx]
hl_end   = max(byte_from des hits dans la zone) + (concat_query_len - query_positions[last_tri_idx])
```

Le `byte_from` du premier trigram est recalé au début de la query concaténée.
Le `byte_from` du dernier trigram est prolongé jusqu'à la fin.

Pour d>0, les highlights sont approximatifs (±distance). Suffisant pour BM25
tf et affichage.

### Étape 7 : Résultat

- `BitSet` des doc_ids matchés
- `Vec<(doc_id, byte_from, byte_to)>` highlights (dédupliqués, triés)

Même signature que l'actuel `fuzzy_contains_via_trigram`.

## Modification du falling walk

`cross_token_falling_walk` dans `literal_pipeline.rs` ligne 196 utilise
`sib_table.contiguous_siblings()`. Pour le fuzzy contains, il faut aussi
considérer les siblings avec gap (séparateurs entre tokens).

Ajouter un paramètre `allow_gaps: bool` à `cross_token_falling_walk`.
Quand `true`, utilise `siblings()` et filtre uniquement sur le texte du
token suivant (comme aujourd'hui), mais accepte les gaps.

## Ce qui ne change PAS

- `fst_candidates` : inchangé
- `resolve_candidates`, `resolve_chains` : inchangé
- Le pipeline regex contains : inchangé
- `intersect_trigrams_with_threshold` : plus utilisé par le fuzzy, reste pour legacy

## Fichiers

- **Nouveau** : `src/query/phrase_query/fuzzy_contains.rs`
  - `pub fn fuzzy_contains(...)` — le pipeline complet
  - `TrigramHit`, `DocHits`, `HitsByDoc` — structures intermédiaires
  - `concat_query()`, `generate_trigrams()`, `build_hits_by_doc()`,
    `find_matches()`, `compute_highlights()`
- **Modifié** : `src/query/phrase_query/literal_pipeline.rs`
  - `cross_token_falling_walk` : paramètre `allow_gaps: bool`
- **Modifié** : `src/query/phrase_query/regex_continuation_query.rs`
  - `run_fuzzy_prescan` : appeler `fuzzy_contains` au lieu de
    `fuzzy_contains_via_trigram`

## Complexité attendue

- Étapes 1-2 : O(query_len) — négligeable
- Étape 3 : ~20ms (identique à aujourd'hui, résolution sélective)
- Étape 4 : O(total_matches) — construction du dico, linéaire
- Étape 5 : O(D × P × max_span) avec D=docs, P~20, max_span~5 → <1ms
- Étape 6 : O(résultats) → négligeable
- **Total : ~20ms** (vs 877ms pour "3db_val", vs 35ms pour "rag3weaver")

## Exemple complet : "3db_val" d=1

1. Concat : "3dbval"
2. Bigrams (n=2, len=6 < 7) : "3d" "db" "bv" "va" "al"
3. Resolve : chaque bigram via fst + cross_token(allow_gaps=true)
4. Build dico : par doc, par position, chaque hit avec tri_idx et byte_from
5. Filter : threshold=3, max_span=3 (2 mots + d=1)
   Pour chaque position P, compter tri_idx distincts dans [P, P+3]
   → 5 tri_idx dans [5, 8] → match
6. Highlight : recaler byte_from/byte_to depuis les hits extrêmes
7. Résultat : (doc_id, hl_start, hl_end)

Pas de DFA walk. Pas de 21000 candidats. Direct.
