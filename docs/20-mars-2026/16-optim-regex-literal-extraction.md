# Doc 16 — Optimisations continuation : literal extraction + stored text fallback

Date : 21 mars 2026

## Problème

Le mode `contains` avec `regex: true` et un pattern comme `alloc.*free` explose
en temps de calcul. Le `.*` fait que le DFA accepte TOUT — chaque continuation
step garde tous les candidats, et ça traverse 64 depth sur potentiellement 90K docs.

Ce n'est pas un deadlock, c'est juste combinatoirement explosif.

Le mode `contains` exact avec continuation (depth 8 max) n'a pas ce problème car
les walks FST sont très sélectifs. Mais sur des tokens très courts (2-3 chars)
avec beaucoup de candidats, ça pourrait aussi bénéficier du stored text fallback.

## Optimisation 1 : Literal extraction pour regex

Technique utilisée par ripgrep / le moteur `regex` de Rust.

### Principe
1. Parser la regex avec `regex_syntax::hir::literal::Extractor` (déjà en dépendance)
2. Extraire les sous-chaînes littérales obligatoires du pattern
3. Faire un SFX walk sur chaque littéral → sets de doc_ids
4. Intersecter les sets → candidats réduits
5. DFA continuation uniquement sur ces candidats

### Exemple
```
Pattern: alloc.*free
Littéraux extraits: prefixes=["alloc"], suffixes=["free"]

Étape 1: SFX walk "alloc" → 800 docs
Étape 2: SFX walk "free"  → 2000 docs
Étape 3: intersection     → 150 docs
Étape 4: DFA continuation sur 150 docs seulement (au lieu de 90K)
```

### API existante

```rust
use regex_syntax::hir::literal::Extractor;
use regex_syntax::parse;

let hir = parse("alloc.*free").unwrap();
let seq = Extractor::new().extract(&hir);
// seq contient les littéraux obligatoires du pattern
// Gère : alternations (a|b), répétitions, classes [a-z],
// ancres, négations — tout est pris en compte.
```

`regex-syntax` est déjà en dépendance de `lucivy-core`. C'est le même crate
qu'Andrew Gallant (burntsushi) utilise dans ripgrep et le moteur `regex`.
On a aussi forké son crate `fst` pour notre suffix FST (lucivy-fst).

### Point d'injection

Dans `RegexContinuationWeight::scorer()` (regex_continuation_query.rs:272),
avant `continuation_score()` :

```rust
// Extraire les littéraux du pattern
let hir = regex_syntax::parse(&pattern)?;
let literals = Extractor::new().extract(&hir);

// SFX walk sur chaque littéral → BitSet candidats
let mut candidate_docs = BitSet::with_max_value(max_doc);
candidate_docs.fill();
for lit in literals.literals() {
    if let Ok(s) = std::str::from_utf8(lit.as_bytes()) {
        let walk = sfx_reader.prefix_walk(s);
        let mut lit_docs = BitSet::with_max_value(max_doc);
        for (_, parents) in &walk {
            for p in parents {
                for e in resolver.resolve(p.raw_ordinal) {
                    lit_docs.insert(e.doc_id);
                }
            }
        }
        candidate_docs.intersect(&lit_docs);
    }
}

// Passer candidate_docs à continuation_score pour filtrer
continuation_score_filtered(&automaton, ..., &candidate_docs)?
```

`continuation_score` devrait prendre un `Option<&BitSet>` en paramètre.
Si présent, skip les entries dont le doc_id n'est pas dans le bitset.

### Cas où ça n'aide pas
- Regex sans littéraux : `[a-z]+` → pas de littéral extractible, fallback DFA brut
- Littéraux très courts : `a.*b` → "a" et "b" matchent presque tout
- L'extracteur gère correctement les patterns négatifs (`[^x]`)

## Optimisation 2 : Stored text fallback (depth 3+)

### Principe

Quand la continuation loop atteint depth ≥ 3, au lieu de faire encore un walk FST
(coûteux, surtout pour regex), on lit le stored text et on vérifie directement.

Le `byte_from` est connu dès le walk initial (dans le sfxpost entry). Il suffit
de lire `text[byte_from..]` et vérifier si le query/regex matche.

### Pourquoi c'est efficace

À depth 3+, on a déjà fait 2-3 walks FST très sélectifs. Les candidats restants
sont peu nombreux (quelques dizaines). Lire le stored text pour chacun c'est :
- 1 lecture mmap (le store est déjà chargé en mémoire)
- 1 comparaison string (contains exact) ou regex match
- O(1) par candidat, pas de walk FST supplémentaire

### Deux boucles à modifier

**1. suffix_contains.rs — continuation loop (contains/startsWith exact)**

Fichier : `src/query/phrase_query/suffix_contains.rs:177`
```rust
for _depth in 0..8 {
```

Actuellement : fait un `prefix_walk_si0(remaining)` à chaque depth.
Changement : à partir de depth 3, lire le stored text.

Problème : `suffix_contains_single_token_inner` ne reçoit pas le `SegmentReader`.
Il reçoit un `SfxFileReader` + closure `resolver`.

Solution : ajouter un paramètre optionnel `store_verifier`:
```rust
/// Callback pour vérifier un candidat via le stored text.
/// (doc_id, byte_from, remaining_query) → bool
type StoreVerifier = dyn Fn(u32, usize, &str) -> bool;

fn suffix_contains_single_token_inner<F>(
    sfx_reader: &SfxFileReader<'_>,
    query: &str,
    raw_term_resolver: F,
    prefix_only: bool,
    continuation: bool,
    store_verifier: Option<&StoreVerifier>,  // NEW
) -> Vec<SuffixContainsMatch>
```

Le scorer dans `suffix_contains_query.rs` construit le verifier :
```rust
let store_reader = reader.get_store_reader(0)?;
let field = self.raw_field;
let verifier = move |doc_id: u32, byte_from: usize, remaining: &str| -> bool {
    let Ok(doc) = store_reader.get::<LucivyDocument>(doc_id) else { return false };
    for (f, val) in doc.field_values() {
        if f == field {
            if let Some(text) = val.as_value().as_str() {
                let text_lower = text.to_lowercase();
                if byte_from < text_lower.len() {
                    return text_lower[byte_from..].starts_with(remaining);
                }
            }
        }
    }
    false
};
```

Dans la continuation loop :
```rust
for depth in 0..8 {
    if depth >= 3 {
        if let Some(verify) = &store_verifier {
            // Vérification directe sur le stored text
            for (&consumed, entries) in &depth_candidates {
                let remaining = &query_lower[consumed..];
                for &(doc_id, _ti, byte_from) in entries {
                    if verify(doc_id, byte_from, remaining) {
                        matches.push(SuffixContainsMatch { doc_id, byte_from, ... });
                    }
                }
            }
            break;  // plus besoin de continuer les walks
        }
    }
    // ... walk FST normal pour depth < 3
}
```

**2. regex_continuation_query.rs — continuation loop (regex/fuzzy)**

Fichier : `src/query/phrase_query/regex_continuation_query.rs:192`
```rust
for _depth in 0..MAX_CONTINUATION_DEPTH {
```

Même pattern, mais avec un regex match au lieu d'un starts_with :
```rust
type StoreDfaVerifier<A> = dyn Fn(u32, usize, &A, &A::State) -> bool;
```

Ou plus simplement, passer un `Option<&StoreReader>` + field à `continuation_score`,
et à depth ≥ 3, lire le doc et faire `automaton.is_match()` sur le texte restant.

```rust
for depth in 0..MAX_CONTINUATION_DEPTH {
    if depth >= 3 && store_reader.is_some() {
        let store = store_reader.unwrap();
        for (&(doc, pos), cand_states) in &candidates {
            for cs in cand_states {
                // Lire le texte à partir de byte_from
                let text = read_field_text(store, doc, field);
                // Matcher le DFA sur le texte restant
                let mut state = cs.dfa_state.clone();
                for &byte in text[cs.byte_from as usize..].as_bytes() {
                    state = automaton.accept(&state, byte);
                    if automaton.is_match(&state) {
                        doc_bitset.insert(doc);
                        break;
                    }
                    if !automaton.can_match(&state) { break; }
                }
            }
        }
        break;
    }
    // ... walk FST normal pour depth < 3
}
```

### Optionalité

Le store_verifier / store_reader est `Option`. Si `None`, la boucle fait le walk
FST classique sur toute la profondeur (comportement actuel).

Cas où on passe `None` :
- Index sans stored fields (stockage désactivé)
- Appels internes de test qui n'ont pas de reader

Cas où on passe `Some` :
- Recherche normale via le scorer (a accès au SegmentReader)

### Impact sur les performances

- **Depth 1-2** : inchangé, walk FST (rapide et sélectif)
- **Depth 3+** : une lecture mmap + comparaison string au lieu de N walks FST
- **Regex `alloc.*free`** : de "infini" à ~1s (literal extraction filtre, puis
  stored text vérifie les quelques candidats restants)

## Résumé des changements

| Fichier | Changement |
|---------|-----------|
| `regex_continuation_query.rs` | Literal extraction dans `scorer()`, `store_reader` param dans `continuation_score()` |
| `suffix_contains.rs` | `store_verifier` param dans `suffix_contains_single_token_inner()` |
| `suffix_contains_query.rs` | Construire le `store_verifier` closure dans `scorer()` |

Les deux optimisations sont indépendantes et complémentaires :
- Literal extraction = pré-filtrage (réduit le nombre de candidats)
- Stored text fallback = accélération en profondeur (évite les walks FST coûteux)

## Priorité

- **Stored text fallback** : facile à implémenter, bénéficie à contains + startsWith + regex + fuzzy
- **Literal extraction** : spécifique regex, mais critique pour les patterns type `alloc.*free`

Faire le stored text fallback en premier — c'est le plus impactant et le plus simple.
