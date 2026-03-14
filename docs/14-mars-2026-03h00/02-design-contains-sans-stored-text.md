# Design : contains search sans stored text

Date : 14 mars 2026

## Problème

La recherche contains est lente à cause de la vérification par stored text :

```
Pour chaque candidat (500+ docs) :
  store_reader.get(doc_id)     → décompression LZ4 (~16KB par bloc)
  tokenize_raw(texte)          → re-tokenisation (5000 tokens par doc)
  edit_distance × 5000 tokens  → vérification fuzzy
```

Sur 5201 docs : contains 'rag3db' d=0 = ~300ms, d=1 = ~1400ms.

## Solution : 3 pièces qui éliminent le stored text à 100%

### Pièce 1 — Trigrams (existant)

Pré-filtre rapide. Le champ `._ngram` contient les trigrammes de chaque token.
Intersection des posting lists des trigrammes de la query → ~500 candidats.

Déjà implémenté dans `NgramContainsQuery`. Rapide, O(k) lookups. Pas touché.

### Pièce 2 — FuzzySubstringAutomaton (existant, réactivé)

Vérification exacte via le FST du raw field. **Remplace le stored text** pour confirmer
qu'un terme du document contient la query comme substring.

Fichier : `src/query/fuzzy_substring_automaton.rs` (déjà dans le code, `#[allow(dead_code)]`)

L'automate implémente `.*levenshtein(token, d).*` :
- À chaque byte du FST, démarre un nouveau walk Levenshtein (prefix `.*`)
- Une fois en état acceptant, match permanent (suffix `.*`)
- NFA simulation avec déduplication des états actifs

Existait dans la cascade main (niveaux 3-4 : Substring, FuzzySubstring) mais a été retiré
quand les trigrams ont été ajoutés. Les deux approches sont complémentaires :

```
Avant (main)     : FST substring walk seul → lent sur gros FST, pas de pré-filtre
Avant (startsWith): trigrams + stored text  → stored text = bottleneck
Maintenant       : trigrams + FST substring → rapide ET sans stored text
```

Le FST substring walk retourne les termes matchés. Pour chaque terme, on lit sa posting list
→ doc_ids, positions, byte offsets. Tout ce qu'il faut pour confirmer le match et produire
les highlights, sans jamais toucher au store.

### Pièce 3 — Fichier `.gaps` (nouveau)

Séparateurs entre tokens stockés dans un fichier binaire mmap'd par segment.
**Remplace le stored text** pour la validation des séparateurs, prefix et suffix.

Voir doc 01 pour le format détaillé. En résumé :

```
┌───────────────────────────────────────────────┐
│ Header : magic "GAPS" + version + num_docs    │
│ Offset table : [u64 × (num_docs + 1)]         │
│ Data par doc :                                │
│   [num_tokens: u16]                           │
│   [gap_0: len+bytes]  // avant token 0        │
│   [gap_1: len+bytes]  // entre token 0 et 1   │
│   ...                                         │
│   [gap_N: len+bytes]  // après dernier token  │
└───────────────────────────────────────────────┘
```

Accès O(1) par doc_id via la table d'offsets (mmap'd). Taille ~6-8KB par doc
de code source (vs ~50KB pour le stored text).

## Flow complet — exemple

### Document (doc_id = 42)

```
import rag3db from 'rag3db_core';
```

### À l'indexation — ce qui est stocké

```
RAW FIELD FST :  {"core", "from", "import", "rag3db"}

RAW POSTING LIST de "rag3db" :
  doc=42 : freq=2, positions=[1, 3], offsets=[(7,13), (20,26)]

NGRAM POSTING LISTS :
  "rag" → doc=42    "ag3" → doc=42    "g3d" → doc=42
  "3db" → doc=42    "imp" → doc=42    "cor" → doc=42  ...

.GAPS (doc=42) :
  num_tokens=5
  gap[0] = ""       (avant "import")
  gap[1] = " "      (entre "import" et "rag3db")
  gap[2] = " "      (entre "rag3db" et "from")
  gap[3] = " '"     (entre "from" et "rag3db")
  gap[4] = "_"      (entre "rag3db" et "core")
  gap[5] = "';"     (après "core")

STORE :  "import rag3db from 'rag3db_core';"  ← PLUS UTILISÉ pour la recherche
```

### Recherche contains "rag3" d=0

```
"rag3"
  │
  ▼
① TRIGRAMS (pré-filtre)
  Trigrammes de "rag3" : ["rag", "ag3"]
  Intersection posting lists ngram : "rag" ∩ "ag3"
  → candidats : doc 42, doc 87, doc 201, ... (~500 docs)
  │
  ▼
② FST SUBSTRING WALK (vérification)
  FuzzySubstringAutomaton(.*rag3.*, d=0)
  Walk le FST du raw field :
    "core"    → aucun état acceptant → skip
    "from"    → aucun état acceptant → skip
    "import"  → aucun état acceptant → skip
    "rag3db"  → état acceptant après "rag3" ✓ → MATCH
  │
  ▼
  Terme trouvé : "rag3db"
  Posting list de "rag3db" → doc=42 : positions=[1,3], offsets=[(7,13),(20,26)]
  │
  ▼
③ INTERSECTION candidats ∩ posting list
  doc 42 est dans les trigrams ET dans le posting de "rag3db" → CONFIRMÉ
  │
  ▼
④ Résultat :
  ✓ doc_id = 42
  ✓ positions = [1, 3]
  ✓ highlights = [(7,13), (20,26)]  (byte offsets du posting list)
  ✓ BM25 = score(fieldnorm, term_freq=2)

  Zéro stored text. Zéro décompression. Zéro re-tokenisation.
```

### Recherche contains "rag3db core" d=0 (multi-token, avec séparateur)

```
Tokens query : ["rag3db", "core"], séparateur : " "

① TRIGRAMS : intersection pour chaque token → candidats
② FST : lookup exact "rag3db" et "core" dans le FST → trouvés
③ POSTING LISTS :
   "rag3db" doc=42 : positions=[1, 3]
   "core"   doc=42 : positions=[4]
④ POSITION MATCH : pos 3 suivi de pos 4 → consécutifs ✓
⑤ .GAPS VALIDATION :
   gaps_reader.get(doc=42) → gap[4] = "_"
   Séparateur attendu : " "
   Séparateur réel : "_"
   edit_distance(" ", "_") = 1
   Budget = 0 → REJETÉ (le séparateur ne matche pas)

   (Si budget ≥ 1 → accepté en fuzzy)
```

### Recherche contains "rag3" d=1 (fuzzy substring)

```
"rag3" d=1
  │
  ▼
① TRIGRAMS : trigrammes de "rag3" avec threshold réduit → candidats
② FST FUZZY SUBSTRING :
  FuzzySubstringAutomaton(.*levenshtein("rag3", 1).*, d=1)
  Walk le FST :
    "core"   → skip
    "from"   → skip
    "import" → skip
    "rag3db" → "rag3" exact substring (distance 0 ≤ 1) ✓
  │
  ▼
  Résultat identique, mais le DFA est plus large (plus d'états actifs).
  Le coût est dans le walk FST, pas dans la décompression de 500 docs.
```

## Interaction entre les 3 pièces

```
                    TRIGRAMS                FST SUBSTRING           .GAPS
                    (._ngram field)         (._raw FST)             (.gaps file)
                    ─────────────           ───────────             ──────────
Rôle :              Pré-filtre candidats    Vérification match      Validation séparateurs
Données :           Posting lists trigrams  Dictionnaire termes     Chars entre tokens
Accès :             O(k) lookups            O(FST × DFA states)     O(1) mmap
Quand :             Toujours                Toujours                Si séparateurs/prefix/suffix
Remplace :          —                       stored text (match)     stored text (séparateurs)
```

Le posting list du raw field fournit les **positions** et **byte offsets** des matches.
Les 3 pièces ensemble reconstituent toute l'information sans jamais toucher au store.

## Cas où le stored text reste nécessaire

1. **Affichage du document complet** — quand l'utilisateur veut lire le contenu du doc,
   pas juste les highlights. Pas dans le hot path de recherche.

2. **Regex sur texte brut** — le mode `VerificationMode::Regex` exécute un regex compilé
   sur le texte entier. Le FST substring ne peut pas émuler un regex arbitraire sur le
   contenu. Pourrait être optimisé plus tard avec un regex-on-FST mais complexe.

## Fichiers impactés

### Réactivation FuzzySubstringAutomaton

- `src/query/fuzzy_substring_automaton.rs` — retirer `#[allow(dead_code)]`
- `src/query/phrase_query/ngram_contains_query.rs` — dans `NgramContainsWeight::scorer()`,
  après les candidats trigrams, utiliser FuzzySubstringAutomaton pour la vérification au lieu
  de `store_reader.get()` + `tokenize_raw()`

### Nouveau fichier .gaps

- `src/index/segment_component.rs` — ajouter `Gaps` à l'enum
- `src/index/index_meta.rs` — ajouter `".gaps"` dans `relative_path()`
- `src/indexer/segment_serializer.rs` — ouvrir/écrire/fermer le `.gaps`
- `src/indexer/segment_writer.rs` — capturer les gaps pendant la tokenisation
- `src/indexer/merger.rs` — concaténer les `.gaps` au merge
- Nouveau : `src/store/gaps_writer.rs` et `src/store/gaps_reader.rs`

### Modification du scorer

- `src/query/phrase_query/ngram_contains_query.rs` — `NgramContainsScorer::verify()` :
  au lieu de `store_reader.get()`, utiliser FST substring + `.gaps`
- `src/query/phrase_query/contains_scorer.rs` — `validate_separators()` :
  lire les gaps depuis le `.gaps` reader au lieu du stored text

## Ordre d'implémentation

### Phase 1 — FST substring (gros gain, zéro changement de format)

Réactiver FuzzySubstringAutomaton dans NgramContainsWeight. Les candidats trigrams sont
vérifiés via FST walk au lieu de stored text. Pas besoin du .gaps pour cette phase —
on skip la validation séparateurs (mode relaxed : positions consécutives = OK).

**Gain estimé** : -90% sur contains d=0, -80% sur contains d=1.
Le seul coût restant est le FST walk (~5ms) au lieu des 500 décompressions LZ4 (~300ms).

### Phase 2 — .gaps writer + reader

Implémenter l'écriture et la lecture du fichier .gaps. Les index créés après cette phase
auront les gaps. Compatibilité ascendante : si pas de .gaps, fallback stored text.

### Phase 3 — Validation séparateurs via .gaps

Brancher le .gaps reader dans ContainsScorer et NgramContainsScorer pour la validation
strict des séparateurs. Élimine le dernier usage du stored text dans la recherche.

### Phase 4 — Merger

Implémenter la concaténation des .gaps au merge de segments.

## Benchmarks attendus (phase 1 seule)

| Query | Avant | Après (estimé) | Gain |
|-------|-------|----------------|------|
| contains 'rag3db' d=0 | 300ms | <5ms | 60x |
| contains 'rag3db' d=1 | 1400ms | <20ms | 70x |
| contains 'main' d=0 | 125ms | <5ms | 25x |
| split 'rag3db main' d=1 | 1900ms | <30ms | 60x |
| contains 'atafl' d=0 (mid-word) | ~200ms | <10ms | 20x |

Le gain sur d=1 est le plus spectaculaire : au lieu de 500 docs × 5000 tokens × edit_distance,
c'est un seul FST walk avec le DFA Levenshtein qui fait tout le travail.
