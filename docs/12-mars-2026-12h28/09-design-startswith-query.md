# Design : query type `startsWith`

## Objectif

Ajouter un query type `"startsWith"` qui exploite directement le FST pour une recherche par préfixe optimale. Le `contains` actuel peut techniquement répondre à des recherches préfixes, mais il passe par des trigrams + stored text verification — inutilement coûteux quand on sait que le pattern est en début de valeur.

`startsWith` sera la première query de lucivy qui est **strictement plus rapide** qu'un moteur full-text classique sur ce use case, car le FST est un trie natif pour les préfixes.

## Comportement attendu

```json
{
  "type": "startsWith",
  "field": "body",
  "value": "async prog",
  "distance": 1
}
```

Recherche : tous les documents dont le champ `body` contient un token qui **commence par** la valeur (cross-token, fuzzy optionnel, dernier token = préfixe).

### Sémantique précise

- `"async prog"` est tokenisé → `["async", "prog"]`
- Les **N-1 premiers tokens** (`"async"`) : match exact ou fuzzy (Levenshtein DFA complet sur le FST). Pas de substring — on cherche des termes entiers, pas des sous-chaînes.
- Le **dernier token** (`"prog"`) : traité comme **préfixe** — matche `"program"`, `"programming"`, `"programmation"`, etc. via range FST ou prefix DFA.
- **Positions** : les tokens doivent être à des positions consécutives dans le document (phrase adjacente).
- Si `distance > 0` : les N-1 premiers tokens acceptent du fuzzy (Levenshtein), le dernier est un préfixe fuzzy (prefix DFA).

### Différence avec `contains`

| | `startsWith` | `contains` |
|---|---|---|
| Tokens non-derniers | exact ou fuzzy DFA (terme complet) | exact → fuzzy → substring → fuzzy substring |
| Dernier token | préfixe (range FST) ou prefix fuzzy DFA | même cascade que les autres |
| Méthode de résolution | FST direct, pas de stored text | trigrams + stored text verification |
| Vitesse | +++ (FST natif, zéro I/O stored) | ++ (trigrams + I/O stored text) |
| Rappel | matche uniquement des termes complets ou préfixes | matche des sous-chaînes n'importe où |

## Architecture : cascade modifiée

### Cascade actuelle (`contains`) — `cascade_term_infos`

Pour chaque token, dans l'ordre :
1. **Exact** : term dict lookup direct
2. **Fuzzy** : Levenshtein DFA (terme complet, `build_dfa`)
3. **Substring** : regex `.*token.*` sur FST
4. **Fuzzy substring** : `FuzzySubstringAutomaton`

Dès qu'un niveau trouve des résultats → on s'arrête.

### Cascade `startsWith` — tokens non-derniers

Pour les N-1 premiers tokens :
1. **Exact** : term dict lookup direct
2. **Fuzzy** : Levenshtein DFA (terme complet, `build_dfa`)

Stop. Pas de substring/fuzzy substring — `startsWith` implique des termes complets, pas des sous-chaînes.

### Cascade `startsWith` — dernier token (préfixe)

Pour le dernier token :
1. **Prefix range** : range FST `[token..prefix_end(token))` — tous les termes qui commencent par `token`
2. **Prefix fuzzy** : Levenshtein prefix DFA (`build_prefix_dfa`) — termes dont le début est à distance ≤ d de `token`

C'est une cascade à 2 niveaux seulement.

## Implémentation

### Modifications dans `AutomatonPhraseWeight`

Le cœur du travail est dans `automaton_phrase_weight.rs`. On ajoute :

#### 1. Champ `last_token_is_prefix: bool`

Propagé depuis `AutomatonPhraseQuery` → `AutomatonPhraseWeight`.

#### 2. Nouvelle méthode `prefix_term_infos`

```rust
/// Cascade for a prefix token: prefix range → prefix fuzzy.
fn prefix_term_infos(
    &self,
    token: &str,
    inverted_index: &InvertedIndexReader,
) -> crate::Result<(Vec<TermInfo>, CascadeLevel)> {
    let term_dict = inverted_index.terms();

    // 1. PREFIX RANGE: all terms starting with `token`
    let prefix_bytes = token.as_bytes();
    let end_bytes = prefix_end(prefix_bytes);
    // Build a range stream [prefix..prefix_end)
    let mut builder = term_dict.range();
    builder = builder.ge(prefix_bytes);
    if let Some(ref end) = end_bytes {
        builder = builder.lt(end);
    }
    let mut stream = builder.into_stream()?;
    let mut term_infos = Vec::new();
    while stream.advance() && term_infos.len() < self.max_expansions as usize {
        term_infos.push(stream.value().clone());
    }
    if !term_infos.is_empty() {
        return Ok((term_infos, CascadeLevel::Exact)); // prefix exact
    }

    // 2. PREFIX FUZZY: Levenshtein prefix DFA
    if self.fuzzy_distance > 0 && self.fuzzy_distance <= 2 {
        let builder = get_automaton_builder(self.fuzzy_distance);
        let dfa = DfaWrapper(builder.build_prefix_dfa(token));
        let mut stream = term_dict.search(&dfa).into_stream()?;
        let mut term_infos = Vec::new();
        while stream.advance() && term_infos.len() < self.max_expansions as usize {
            term_infos.push(stream.value().clone());
        }
        if !term_infos.is_empty() {
            return Ok((term_infos, CascadeLevel::Fuzzy(self.fuzzy_distance)));
        }
    }

    Ok((Vec::new(), CascadeLevel::Exact))
}
```

#### 3. Nouvelle méthode `starts_with_term_infos`

Pour les tokens non-derniers de `startsWith` : exact + fuzzy seulement (pas de substring).

```rust
/// Cascade for a non-last startsWith token: exact → fuzzy only.
fn starts_with_term_infos(
    &self,
    token: &str,
    inverted_index: &InvertedIndexReader,
) -> crate::Result<(Vec<TermInfo>, CascadeLevel)> {
    // 1. EXACT
    let term = Term::from_field_text(self.field, token);
    if let Some(term_info) = inverted_index.get_term_info(&term)? {
        return Ok((vec![term_info], CascadeLevel::Exact));
    }

    // 2. FUZZY (no substring, no fuzzy substring)
    if self.fuzzy_distance > 0 && self.fuzzy_distance <= 2 {
        let builder = get_automaton_builder(self.fuzzy_distance);
        let dfa = DfaWrapper(builder.build_dfa(token));
        let mut stream = inverted_index.terms().search(&dfa).into_stream()?;
        let mut term_infos = Vec::new();
        while stream.advance() {
            term_infos.push(stream.value().clone());
        }
        if !term_infos.is_empty() {
            return Ok((term_infos, CascadeLevel::Fuzzy(self.fuzzy_distance)));
        }
    }

    Ok((Vec::new(), CascadeLevel::Exact))
}
```

#### 4. Modification de `phrase_scorer`

Dans la boucle sur `phrase_terms`, dispatcher selon le mode :

```rust
for (i, &(offset, ref token)) in self.phrase_terms.iter().enumerate() {
    let is_last = i == self.phrase_terms.len() - 1;
    let (term_infos, level) = if self.last_token_is_prefix {
        if is_last {
            self.prefix_term_infos(token, &inverted_index)?
        } else {
            self.starts_with_term_infos(token, &inverted_index)?
        }
    } else {
        self.cascade_term_infos(token, &inverted_index)?
    };
    // ... rest unchanged
}
```

#### 5. Modification de `single_token_scorer`

Si `last_token_is_prefix && phrase_terms.len() == 1` : utiliser `prefix_term_infos` au lieu de `cascade_term_infos`.

### Modifications dans `AutomatonPhraseQuery`

- Ajouter `last_token_is_prefix: bool` au struct
- Propager dans les constructeurs (nouveau constructeur `new_starts_with` ou paramètre additionnel)
- Propager vers `AutomatonPhraseWeight::new`

### Modifications dans `lucivy_core/src/query.rs`

Nouveau case dans `build_query` :
```rust
"startsWith" => build_starts_with_query(config, schema, index, raw_pairs, highlight_sink),
```

`build_starts_with_query` :
- Tokenise la value sur le champ raw
- Construit un `AutomatonPhraseQuery` avec `last_token_is_prefix: true`
- Pas besoin de ngram field (pas de trigrams)
- Pas besoin de stored field (pas de verification sur stored text)
- Pas de separators/prefix/suffix validation (on match des termes complets, pas des sous-chaînes)

```rust
fn build_starts_with_query(
    config: &QueryConfig,
    schema: &Schema,
    index: &Index,
    raw_pairs: &[(String, String)],
    highlight_sink: Option<Arc<HighlightSink>>,
) -> Result<Box<dyn Query>, String> {
    let field = resolve_field(config, schema, raw_pairs, true)?;
    let value = config.value.as_deref().ok_or("startsWith query requires 'value'")?;
    let fuzzy_distance = config.distance.unwrap_or(0);

    let tokens = tokenize_for_field(index, field, schema, value);
    if tokens.is_empty() {
        return Err("startsWith query produced no tokens".into());
    }

    let phrase_terms: Vec<(usize, String)> = tokens
        .into_iter()
        .enumerate()
        .collect();

    let mut query = AutomatonPhraseQuery::new_starts_with(
        field,
        phrase_terms,
        50,  // max_expansions
        fuzzy_distance,
    );
    if let Some(sink) = highlight_sink {
        query = query.with_highlight_sink(sink, config.field.clone().unwrap_or_default());
    }
    Ok(Box::new(query))
}
```

### Pas de changement dans les bindings

Les bindings Node.js, Python, Emscripten passent le JSON brut → `build_query`. Le nouveau query type `"startsWith"` est disponible automatiquement.

## Ce qu'on ne fait PAS

- **endsWith** : nécessite un reverse FST à l'indexation. Chantier séparé.
- **startsWith cross-field** : on reste mono-champ comme `contains`.
- **Separator validation** : pas pertinente pour `startsWith` (on match des termes complets via FST, pas des sous-chaînes dans du stored text).
- **NgramContainsQuery variant** : pas besoin — le FST est plus rapide que les trigrams pour les préfixes.

## Risques

- **`max_expansions`** : un préfixe très court (ex: `"a"`) peut matcher des milliers de termes. Default à 50 (comme `PhrasePrefixQuery`). Configurable si besoin.
- **Champ raw** : `startsWith` doit utiliser le champ raw (lowercased, pas stemmé). Le préfixe `"prog"` doit matcher `"programming"`, pas la forme stemmée `"program"`.

## Fichiers à modifier

```
lucivy_core/src/query.rs                           # routing + build_starts_with_query
src/query/phrase_query/automaton_phrase_query.rs    # last_token_is_prefix, new_starts_with()
src/query/phrase_query/automaton_phrase_weight.rs   # prefix_term_infos, starts_with_term_infos, dispatch dans phrase_scorer/single_token_scorer
```

~100-150 lignes de code nouveau, principalement dans `automaton_phrase_weight.rs`.
