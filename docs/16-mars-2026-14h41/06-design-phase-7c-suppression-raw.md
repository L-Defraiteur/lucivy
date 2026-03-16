# Design Phase 7c — Suppression complète du ._raw

## Objectif

Supprimer le champ `._raw` du schema. Le SfxCollector run son propre RAW_TOKENIZER sur le texte brut du champ principal, indépendamment du pipeline d'indexation stemmé. Les .sfx/.sfxpost sont stockés sous le field_id du champ principal.

## Pourquoi ._raw ne peut pas être simplement supprimé

Le suffix FST indexe les **suffixes des tokens exacts** (non-stemmés). Un stemmer transforme "programming" → "programm", ce qui casse :
- **contains** : recherche de "programming" ne trouve pas "programm"
- **fuzzy** : distance calculée sur la forme stemmée
- **regex** : patterns matchent la forme stemmée

Les routes SI=0 (term, fuzzy, prefix) fonctionneraient avec des tokens stemmés, mais le contains (SI>0) a besoin du texte exact. C'est la raison d'être du ._raw.

## Architecture actuelle (avec ._raw)

```
Texte "programming en Rust"
  ├─ champ "body" (stemmed) → [programm, en, rust] → inverted index
  └─ champ "body._raw" (raw) → [programming, en, rust]
       ├─ inverted index (plus lu depuis Phases 2-5, mais encore écrit)
       ├─ .sfx (suffix FST)
       └─ .sfxpost (posting entries)

Queries contains/fuzzy/regex → ciblent body._raw
Queries term/phrase linguistiques → ciblent body (stemmé)
```

## Architecture cible (7c)

```
Texte "programming en Rust"
  └─ champ "body"
       ├─ tokenizer stemmé → [programm, en, rust] → inverted index
       └─ RAW_TOKENIZER séparé → [programming, en, rust]
            ├─ .sfx (suffix FST)    ← stocké sous body.field_id
            └─ .sfxpost (postings)  ← stocké sous body.field_id

Queries contains/fuzzy/regex → ciblent body → .sfx/.sfxpost (tokens raw)
Queries term/phrase linguistiques → ciblent body → inverted index (tokens stemmés)
```

Le .sfx/.sfxpost du champ principal contiennent des tokens **raw** (non-stemmés), pas les tokens de l'inverted index. Les deux coexistent sous le même field_id.

## Changements par fichier

### 1. segment_writer.rs — Double tokenization

**Avant :**
```rust
// Pour ._raw : interceptor capture les tokens pendant l'indexation
let mut interceptor = SfxTokenInterceptor::wrap(token_stream);
postings_writer.index_text(doc_id, &mut interceptor, field, ...);
let captured = interceptor.take_captured();
collector.add_tokens(captured);
```

**Après :**
```rust
// Pour le champ principal : deux passes sur le même texte
// Pass 1 : tokenizer stemmé → inverted index (inchangé)
postings_writer.index_text(doc_id, &mut token_stream, field, ...);

// Pass 2 : RAW_TOKENIZER → SfxCollector (pas d'inverted index)
let mut raw_stream = raw_analyzer.token_stream(text);
collector.begin_value(text, ti_before);
while raw_stream.advance() {
    let tok = raw_stream.token();
    collector.add_token(&tok.text, tok.offset_from, tok.offset_to);
}
```

**Détails :**
- `SfxCollector` est créé uniquement pour les champs texte principaux (pas ._raw, pas ngram, pas string)
- Le segment_writer stocke un `TextAnalyzer` RAW_TOKENIZER (cloné depuis l'index)
- `SfxTokenInterceptor` n'est plus nécessaire pour ce flow (peut être gardé pour d'autres usages)
- Le `sfx_collectors` HashMap est keyed par le field_id du champ principal

**Optimisation** : quand il n'y a pas de stemmer, le tokenizer principal = RAW_TOKENIZER. Détecter ce cas et utiliser l'intercepteur (une seule passe) au lieu de tokeniser deux fois.

### 2. handle.rs (lucivy_core) — Schema simplifié

**Supprimer :**
- Constant `RAW_SUFFIX`
- Champ `raw_field_pairs: Vec<(String, String)>` de `LucivyHandle`
- Création des champs ._raw dans `build_schema()`
- `build_schema()` retourne `(Schema, Vec<(String, Field)>)` au lieu de `(Schema, Vec<(String, Field)>, Vec<(String, String)>)`

**Garder :**
- `RAW_TOKENIZER` constant et `configure_tokenizers()` — le RAW_TOKENIZER est encore nécessaire pour le SfxCollector dans segment_writer
- L'enregistrement du tokenizer dans l'index

**Modifier :**
- `build_query()` : les routes contains/fuzzy/regex ciblent le champ principal au lieu de `{name}._raw`
- Tous les endroits qui itèrent `raw_field_pairs` → supprimés

### 3. lucivy_core/src/query.rs — Routing simplifié

**Avant :**
```rust
// contains route vers ._raw
let raw_field = handle.field(&raw_name)?;
SuffixContainsQuery::new(raw_field, query_text)
```

**Après :**
```rust
// contains route vers le champ principal
let field = handle.field(&field_name)?;
SuffixContainsQuery::new(field, query_text)
```

Pareil pour RegexContinuationQuery, FuzzyTermQuery, RegexQuery, TermQuery quand ils ciblent ._raw.

### 4. Bindings (6 crates) — Simplification

**Supprimer dans chaque binding :**
- L'auto-duplication vers ._raw dans add_document/add_text
- Les imports de `RAW_SUFFIX`
- Les boucles `for (user, raw_name) in &handle.raw_field_pairs`

**Résultat :** chaque texte est ajouté une seule fois au champ principal. Le segment_writer gère les deux tokenizations en interne.

### 5. merger.rs — Filtre par .sfx au lieu de ._raw

**Avant :**
```rust
let raw_fields: Vec<Field> = self.schema.fields()
    .filter(|(_, entry)| entry.name().ends_with("._raw"))
    .map(|(field, _)| field)
    .collect();
```

**Après :**
```rust
// Trouver les champs qui ont .sfx dans au moins un segment source
let sfx_fields: Vec<Field> = self.schema.fields()
    .filter(|(field, _)| self.readers.iter().any(|r| r.sfx_file(*field).is_some()))
    .map(|(field, _)| field)
    .collect();
```

Aussi : collecter les tokens depuis les .sfx source (SI=0 stream via SfxFileReader) au lieu de `inverted_index.terms()`. Ceci élimine la dernière dépendance du merger sur l'inverted index de ._raw.

### 6. SfxTermDictionary — termdict optionnel

Les méthodes ordinales ne utilisent PAS le termdict :
- `search_automaton_ordinals()` → sfx_reader.fst() seulement
- `get_ordinal()` → sfx_reader.resolve_suffix() seulement
- `range_scan_ordinals()` → sfx_reader.fst() seulement

**Option A** : Rendre `termdict` `Option<&'a TermDictionary>` dans le constructeur. Les méthodes TermInfo paniquent si None.

**Option B** : Créer un `SfxOrdinalDict<'a>` qui ne wrappe que le `SfxFileReader` (plus propre, pas de Option).

**Recommandation** : Option B — struct séparé `SfxOrdinalDict` pour le path ordinal, garder `SfxTermDictionary` pour le fallback backward compat.

### 7. BM25 total_num_tokens

Actuellement : `inverted_index.total_num_tokens()` du champ ._raw.

Sans ._raw, options :
- **A** : Utiliser `inverted_index.total_num_tokens()` du champ **principal** (stemmé). Approximation acceptable — le nombre de tokens est similaire (stemming ne change pas le nombre de tokens, juste leur forme).
- **B** : Stocker `total_num_tokens` dans le header .sfxpost.
- **C** : Calculer depuis les fieldnorms : `sum(fieldnorm_reader.fieldnorm(doc) for doc in 0..max_doc)`. Exact mais O(max_doc).

**Recommandation** : Option A pour commencer (simple, approximation très proche). Option B si on veut être exact.

### 8. Suppression du code mort

Après que 7c fonctionne :
- Supprimer `InvertedIndexResolver` de posting_resolver.rs
- Supprimer `scorer_from_term_infos` de automaton_weight.rs
- Supprimer les guards `.sfxpost_file().is_some()` (toujours vrai)
- Supprimer les paths TermInfo dans term_weight.rs et automaton_phrase_weight.rs
- Supprimer `cascade_term_infos` / `prefix_term_infos` de automaton_phrase_weight.rs
- Supprimer les méthodes TermInfo de SfxTermDictionary (get, search_automaton, range_scan, term_info_from_ord)

## Points de risque

### Backward compatibilité
- Anciens index : .sfx/.sfxpost sur ._raw fields
- Nouveaux index : .sfx/.sfxpost sur champ principal
- **Option 1** : Détecter et supporter les deux (complexe)
- **Option 2** : Forcer un reindex (simple, recommandé pour une alpha)
- **Option 3** : Migration tool qui réindexe les .sfx/.sfxpost

### Double tokenization quand pas de stemmer
Quand le champ principal utilise "default" (LowerCaser) = même que RAW_TOKENIZER :
- Détecter `tokenizer_name == RAW_TOKENIZER || tokenizer_name == "default"` (sans CamelCaseSplit)
- Hmm, en fait RAW_TOKENIZER = SimpleTokenizer + CamelCaseSplitFilter + LowerCaser, et "default" = SimpleTokenizer + LowerCaser
- Ils sont différents (CamelCaseSplit) → double tokenization nécessaire même sans stemmer
- À moins qu'on unifie RAW_TOKENIZER comme le tokenizer par défaut pour tous les champs

### GapMap token index
Le `begin_value(raw_text, ti_before)` reçoit le token_index de départ dans le document. Avec deux tokenizers, le ti_before pour le SfxCollector est basé sur le RAW_TOKENIZER (pas le stemmer). C'est correct : le .sfxpost stocke les positions raw.

Il faut maintenir un compteur de tokens raw séparé du compteur de tokens stemmés (qui est géré par le postings_writer).

### Highlight offsets
Les highlights retournent des byte offsets (byte_from, byte_to) depuis le .sfxpost. Ces offsets sont relatifs au texte original — ils sont corrects quel que soit le tokenizer utilisé pour les capturer (c'est le texte brut qui fait foi).

## Ordre d'implémentation

1. **SfxOrdinalDict** — nouveau struct sans termdict, utilisé par les query weights
2. **segment_writer** — double tokenization, SfxCollector sur champ principal
3. **handle.rs** — supprimer ._raw de build_schema, supprimer raw_field_pairs
4. **query routing** — router vers champ principal
5. **bindings** — supprimer auto-duplication
6. **merger** — filtrer par .sfx au lieu de ._raw, tokens depuis .sfx SI=0
7. **cleanup** — supprimer fallbacks, code mort, guards

Chaque étape doit compiler et passer les tests. Tester avec reindex complet.

## Estimation

- ~300 lignes modifiées/ajoutées (segment_writer, handle, query routing, SfxOrdinalDict)
- ~400 lignes supprimées (._raw creation, auto-duplication, fallbacks, code mort)
- 6 bindings à simplifier (~20-30 lignes chacun)
- Bilan net : ~100 lignes en moins
