# Audit perf : chemin de recherche contains

Date : 14 mars 2026

## Contexte

Benchmark sur 5201 docs (clone rag3db). Régression ~10% mesurée entre binaire main et feature/startsWith sur les queries contains. Le fuzzy d=1 est 4-5x plus lent que d=0, ce qui est le vrai bottleneck utilisateur (~1.9s pour "rag3db main" en contains_split d=1).

## Baseline

| Query | d=0 | d=1 | Ratio |
|-------|-----|-----|-------|
| contains 'rag3db' | 286ms | 1347ms | 4.7x |
| contains 'main' | 125ms | 543ms | 4.3x |
| split 'rag3db main' | 415ms | 1887ms | 4.5x |
| contains 'database' | 69ms | 493ms | 7.1x |

## 3 optimisations identifiées

### 1. UTF-8 boundary clamping — déplacer à l'indexation

**Fichier** : `src/query/phrase_query/ngram_contains_query.rs`

**Problème** : `floor_char_boundary()` / `ceil_char_boundary()` sont appelés 15 fois dans le hot path de vérification fuzzy. Chaque appel est un while-loop qui itère sur les bytes pour trouver une frontière UTF-8 valide. Sur 5201 docs × N candidats × M tokens = des dizaines de milliers d'appels.

**Fix proposé** : Pré-calculer les byte offsets valides à l'indexation, pas à la recherche.

- À l'indexation : les offsets stockés dans le posting list (WithFreqsAndPositionsAndOffsets) sont déjà des byte positions. S'assurer qu'ils tombent toujours sur des char boundaries au moment de l'indexation (dans le tokenizer ou dans `segment_writer`).
- À la recherche : les offsets lus du posting list sont déjà safe → plus besoin de clamping.
- Pour le stored text (vérification fuzzy) : le texte est découpé via `&stored_text[from..to]`. Si `from` et `to` sont calculés à partir d'offsets du posting list (déjà safe), pas de clamping nécessaire. Si calculés autrement (prefix/suffix), valider une seule fois la query au moment de la construction, pas à chaque doc.

**Impact estimé** : -5-15% sur le path fuzzy (supprime les while-loops du tight loop).

**Risque** : Faible — les tokenizers standard produisent déjà des offsets alignés. Le clamping serait un safety net au build_query, pas dans le scorer.

### 2. BM25 séparé du highlight path — ne pas scorer dans le collecteur d'offsets

**Fichier** : `src/query/automaton_weight.rs`

**Problème** : Le path highlight (`highlight_sink` présent) calcule le BM25 **en même temps** que la collecte des byte offsets. C'est fait "opportunistiquement" parce que les freqs sont disponibles, mais ça ajoute :
- `Bm25Weight::for_one_term_without_explain()` par terme
- `bm25.score(fieldnorm_id, term_freq)` par document × par terme
- Lecture des fieldnorms

Avant la branche startsWith, le highlight path ne faisait PAS de BM25 — il utilisait ConstScorer. Le scoring était fait séparément par le collector quand `order_by_score()` était demandé.

**Fix proposé** : Séparer les deux chemins.

```
Highlight path actuel :
  ReadPostings(WithFreqsAndPositionsAndOffsets) → BM25 + offsets → AutomatonScorer

Proposé :
  - Si highlight SANS scoring : ReadPostings(WithPositionsAndOffsets) → offsets only → ConstScorer
  - Si highlight AVEC scoring : garder le path actuel (opportuniste)
  - Si scoring SANS highlight : ReadPostings(WithFreqs) → BM25 → AutomatonScorer (déjà fait)
```

Concrètement : checker `self.scoring_enabled` dans le highlight path. Si false, skip le BM25 et retourner ConstScorer avec les offsets collectés.

**Impact estimé** : -10-20% sur le highlight path (supprime le BM25 quand pas demandé).

**Risque** : Aucun si le scoring et les highlights sont bien découplés dans le collector. Le `order_by_score()` du collector active `scoring_enabled` via `EnableScoring`.

### 3. AutomatonScorer — éviter l'allocation `Vec<Score>` de taille max_doc

**Fichier** : `src/query/automaton_weight.rs`

**Problème** : `AutomatonScorer` pré-alloue `vec![0.0f32; max_doc as usize]` pour stocker les scores par doc_id. Sur un segment de 5000 docs, c'est 20KB (négligeable). Mais sur un gros segment (100K docs = 400KB, 1M docs = 4MB), c'est une allocation significative par query × par terme automaton.

**Fix proposé** : Utiliser un `HashMap<DocId, Score>` ou un `Vec` sparse quand le nombre de candidats est petit par rapport à max_doc.

Heuristique : si le BitSet a < 10% de fill rate, utiliser un HashMap. Sinon garder le Vec dense.

Alternativement : calculer le score inline pendant le BitSet scan au lieu de pré-allouer, en stockant le score dans le BitSetDocSet lui-même (un `ScoredBitSetDocSet` qui porte le score courant).

**Impact estimé** : -2-5% (principalement sur la pression mémoire/cache, pas sur le compute).

**Risque** : Moyen — le HashMap a un overhead par-lookup. À benchmarker avant/après.

## Optimisation bonus : le vrai bottleneck fuzzy d=1

Le fuzzy d=1 est 4-5x plus lent que d=0. Ce n'est pas une régression — c'est le coût structurel de la vérification fuzzy sur le stored text. Le chemin est :

```
contains 'rag3db' d=1 :
  1. Trigram candidates via ._ngram (rapide, ~10ms)
  2. Pour chaque candidat : lire le stored text du document
  3. Calculer edit_distance(query_token, stored_text_slice) pour chaque position
  4. Si distance ≤ budget → match
```

L'étape 2 (lecture stored text) est le vrai coût. Le `StoreReader::get(doc_id)` décompresse le bloc de stockage pour chaque document candidat.

**Pistes d'optimisation du fuzzy** :

- **Pré-filtrage FST** : avant de lire le stored text, vérifier si le terme existe dans le dictionnaire à distance ≤ d via le Levenshtein DFA. Si aucun terme du dictionnaire ne matche, skip le document sans lire le store.
- **Batch store reads** : grouper les lectures de stored text par bloc au lieu d'une par document.
- **Cache de décompression** : garder les blocs décompressés en LRU cache entre les candidats (le store utilise des blocs de ~16KB, souvent plusieurs docs par bloc).

## Optimisation 5 — Cascade stem → trigram → FST fuzzy → stored text

**Idée** : utiliser le champ stemmed comme premier filtre (O(1) lookup exacte), puis trigrams, puis FST fuzzy sur raw, puis stored text en dernier recours. Chaque niveau élimine des candidats avant le suivant.

```
Niveau 1 : stem exact (gratuit) → 5000 → 200 docs
Niveau 2 : trigram intersection → 200 → 100 docs
Niveau 3 : FST fuzzy sur ._raw → 100 → 30 docs
Niveau 4 : stored text verification → 30 docs (seul coût réel)
```

**Limite** : marche pour les tokens entiers. Les substrings purs ("prog" dans "programming") fallback directement sur trigrams.

**Impact estimé** : potentiellement -80% sur d=1 (le stem élimine la majorité des candidats avant toute vérification coûteuse).

## Optimisation 6 — Morpheme/syllable tokenizer (exploratoire)

**Idée** : un champ intermédiaire entre ngram (3 chars) et raw (token entier), découpant les mots en sous-unités morphologiques : "programming" → "pro", "gram", "ming". Résolution croisée entre les champs ngram, morpheme, raw pour trouver les candidats.

**Avantage** : plus sélectif que les trigrams, moins strict que le raw. Capturerait "program" dans "programming" sans fuzzy.

**Risque** : design de recherche avancé, nécessite un tokenizer morphologique (ou heuristique syllabique). Impact sur la taille de l'index. Prédictibilité du contains à valider.

**Statut** : idée à explorer après les optimisations 4-5. Nécessite un prototype + benchmark comparatif.

## Priorités

1. ~~**BM25 séparé du highlight**~~ (fait — commit 2cd65c2)
2. ~~**UTF-8 clamping supprimé du hot path**~~ (fait — commit 2cd65c2)
3. **AutomatonScorer sparse** (impact faible, surtout mémoire — P3)
4. **Fuzzy pré-filtrage FST** sur ._raw (gros impact, design modéré — P1)
5. **Cascade stem → trigram → FST** (très gros impact potentiel, design significatif — P2)
6. **Morpheme tokenizer** (exploratoire, post-release — P4)
