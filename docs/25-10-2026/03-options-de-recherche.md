# Les options de recherche de lucivy 4.3.1, pour le nœud plein texte de rag3weaver (25 octobre 2026)

*Pour la session « recherche » de rag3db. Relevé dans le code de lucivy 4.3.1 (`main` = `fd3fe74`,
moteur `a925d94`, celui que rag3db épingle). Chaque affirmation porte son fichier et sa ligne. Les
résultats d'essai du § 2 ont été joués le 25 octobre sur un petit index, binding Python 4.3.1, même
moteur que le Rust : ce ne sont pas des déductions.*

## 0. À retenir

1. **La démo WASM n'analyse pas la requête.** Elle envoie la chaîne entière, telle quelle, comme
   **un seul `contains` relâché** sur un champ : `{"type":"contains","field":"content","value":"Vue d'ensemble"}`
   (`playground/index.html:2114-2116`, terminal `:3401`), rien d'autre — pas de découpage en mots,
   pas de fuzzy, pas de `parse`. Pour faire exactement comme elle : la même `QueryConfig`, passée à
   `ShardedHandle::search` avec un `HighlightSink` (§ 6).
2. **C'est une recherche de sous-chaîne sur toute la valeur**, qui traverse les frontières de mots
   (`src/query/contains_query_v3.rs:270-271`). « refresh index states » cherche ces trois mots
   **à la suite**, pas trois mots quelque part dans le document.
3. **Ce que rag3weaver faisait — un OU de sous-chaînes par mot — c'est exactement `parse` sans
   syntaxe** (`lucivy_core/src/query.rs:660-697`) ou `contains_split` (`query.rs:302-341`). Sur
   « Vue d'ensemble », ça rend tous les documents qui contiennent « vue » (§ 2) : d'où le
   « n'importe quoi ». La décision de ce soir (la requête entière, fuzzy 1 au plus, aucun découpage)
   revient à faire comme la démo.

## 1. Comment la valeur d'un `contains` est comparée au texte

**Séparateurs relâchés (le défaut, `strict_separators` absent ou `false`, `query.rs:505, 522`).**
Tout caractère ASCII non alphanumérique est un séparateur ; il est **retiré de la requête et du
texte** avant la comparaison (`src/suffix_fst/briques/orchestrator.rs:118-132`,
`src/tokenizer/equal_chunk.rs:100-106`). « refresh index states » devient `refreshindexstates`, et
trouve donc `refresh index states`, `refresh_index_states`, `refreshIndexStates`,
`refresh-index-states`.

**Séparateurs stricts (`"strict_separators": true`).** La requête est comparée telle quelle,
séparateurs compris : « refresh index states » ne trouve que `refresh index states` (§ 2).

**Casse.** La comparaison est toujours insensible à la casse : l'index des suffixes est en
minuscules (`src/suffix_fst/builder_v3.rs:864-874`), la requête aussi à la vérification
(`orchestrator.rs:224-227`). « VUE D'ENSEMBLE » est trouvé.

**Accents : aucun pliage.** `é` reste `é` ; « ensemble » ne trouve pas « ensemblé » en exact (il le
trouve en fuzzy 1, § 2). Le seul code de pliage (`src/query/phrase_query/scoring_utils.rs:266-300`)
est mort (`#[allow(dead_code)]`).

**Apostrophes — le piège du français.** L'apostrophe ASCII `'` est un séparateur (retirée en
relâché) ; **l'apostrophe typographique `’` (U+2019) est non ASCII, donc du contenu**
(`equal_chunk.rs:103-105` : `!c.is_ascii() || c.is_ascii_alphanumeric()`). « Vue d'ensemble » cherche
`vuedensemble` ; un texte « Vue d’ensemble » devient `vued’ensemble` : **aucune correspondance, ni
en exact ni en fuzzy 1** (le `’` fait 3 octets, la distance se compte en octets). Les Markdown
français ont souvent `’`. Si le nœud veut les deux, c'est à l'appelant de normaliser la requête
*et* d'envoyer les deux formes (un `boolean.should` des deux `contains`), puisque le texte indexé,
lui, ne se normalise pas.

**Ce qui traverse la ponctuation.** En relâché, le texte aussi perd ses séparateurs : « refresh
index states » trouve `refresh index state (singular)`, parce que `refreshindexstate(s)ingular`
contient `refreshindexstates` — le span surligné enjambe la parenthèse (§ 2). C'est voulu (c'est ce
qui rend `refresh_index_states` trouvable), mais une requête courte peut ainsi trouver des
collages imprévus. `strict_separators: true` l'évite, au prix des variantes `_`/camelCase.

**Taille.** Une valeur de plus de 2 048 octets ne rend rien (`orchestrator.rs:22, 124`), une valeur
qui n'a que des séparateurs non plus (relâché).

## 2. Les deux requêtes de Lucie, essayées

Index d'un champ `body`, un document par variante. Entre crochets, les spans surlignés relus dans le
texte.

**« Vue d'ensemble »**

| requête | rendu |
|---|---|
| `contains` relâché (la démo) | 3 : `vue densemble`, `VUE D'ENSEMBLE`, `Vue d'ensemble` |
| `contains` strict | 2 : `VUE D'ENSEMBLE`, `Vue d'ensemble` |
| `contains` + `distance: 1` | 4 : les trois, puis `Vue d'ensembl[é]` (score **−998,98**) |
| `parse` sans syntaxe (OU par mot) | **7** : les trois, puis tout document qui contient « vue » (`La vue est vue…`, `Une vue d'un ensemble…`, `Vue d’ensemble` sur « Vue » seul) |
| `parse` entre guillemets, `phrase` | = `contains` relâché |
| `contains_split` | = `parse` sans syntaxe (7) |

Jamais trouvé : `Vue d’ensemble` (apostrophe typographique) en tant que phrase.

**« refresh index states »**

| requête | rendu |
|---|---|
| `contains` relâché (la démo) | 4 : `refresh index state (singular)` [span `refresh index state (s`], `refresh_index_states`, `refresh index states`, `refreshIndexStates` |
| `contains` strict | 1 : `refresh index states` |
| `contains` + `distance: 1` | = relâché (4) |
| `parse` sans syntaxe | 6 : en plus `the states of the index refresh lazily` et `refresh the index; states are kept` (les trois mots, dans n'importe quel ordre) |

**Ordre des résultats.** `vue densemble` passe devant `Vue d'ensemble` : le score est BM25, qui
favorise le document le plus court ; il n'y a **aucune prime à la forme exacte** ni aux séparateurs
identiques. Si le nœud veut l'exact en tête, il faut le recalculer côté appelant (comparer le span
surligné à la requête).

## 3. Les options, une à une

`QueryConfig` (`lucivy_core/src/query.rs:223-270`) se désérialise depuis du JSON ; seul `type` est
renommé (`query_type`). **Pas de `deny_unknown_fields`** : une clé inconnue (`"boost"`, `"offset"`,
`"slop"`) est ignorée en silence — c'est un piège de premier ordre.

| option | JSON | effet | piège |
|---|---|---|---|
| sous-chaîne | `{"type":"contains","field":F,"value":V}` | toute la valeur, d'un seul tenant, à travers les mots | — |
| séparateurs stricts | `"strict_separators": true` | les séparateurs de la requête doivent être ceux du texte | ignoré par `parse`, `contains_split` et `regex` (`query.rs:306-341, 662-697, 537+`) |
| fuzzy | `"distance": 1` (ou 2, 3) | distance d'édition **de toute la valeur** contre le texte, en octets, minuscules, séparateurs retirés en relâché (`src/suffix_fst/briques/fuzzy_spans.rs:10-16`) | **> 3 rend vide sans erreur** (`orchestrator.rs:420-422`) ; valeur de ≤ 3·d+1 caractères : bruit (`init` à 1 trouve `int`, `unit` — `warnings.rs:140-149`) ; score des hits fuzzy **négatif** (`miss_penalty·1000 + bm25`, `src/query/phrase_query/sfx_scoring.rs:247-257`) : ne pas filtrer `score > 0` |
| fuzzy Jaro-Winkler | `"fuzzy_metric":"jaro_winkler","min_similarity":0.9` | similarité sur caractères ; `distance` ne sert qu'à dimensionner les candidats | défaut de `distance` 2 dans ce cas (`query.rs:496`) |
| début de mot | `{"type":"startsWith",…}` | `anchor_start` (`query.rs:396-401`) | ignoré en fuzzy |
| mot entier | `{"type":"term",…}` | `anchor_start` + `exact_match` (`query.rs:357-365`) ; vérifié sur le texte non dépouillé | en relâché `rag3weaver` trouve `rag3_weaver` |
| expression régulière | `"regex": true` | toujours insensible à la casse et stricte (`src/query/regex_query_v3.rs:19, 121`) | `distance` et `anchor_start` ignorés (`query.rs:544-546`) |
| phrase | `{"type":"phrase",…}` | **= `contains`** sur `value` (`query.rs:375-380`) | `terms` n'est lu nulle part ; pas de `slop` |
| proximité | — | **n'existe pas** | — |
| mots dans n'importe quel ordre | `{"type":"parse","field":F,"value":"a b c"}` | **OU** de `contains` par mot (`query.rs:660-697`) | c'est le découpage à éviter ; `parse` avertit (`query_warnings`) |
| syntaxe booléenne | `parse` avec `"…"`, `AND`/`OR`/`NOT`, `+x`, `-x`, parenthèses (`query.rs:699-913`, `value_has_parser_syntax` `:650-658`) | priorité NOT > AND > OR ; mots côte à côte = **OU** ; `"…"` = un `contains` de toute la phrase ; requis présent ⇒ les mots nus ne font que du score | pas de `champ:valeur` ; le `'` ASCII n'est pas de la syntaxe, `Vue d'ensemble` reste un OU de deux mots |
| ET explicite | `{"type":"boolean","must":[…],"should":[…],"must_not":[…]}` (`query.rs:578-592`) | composition libre de feuilles | `must_not` n'a pas de surlignage |
| plusieurs champs | `parse` + `"fields":[…]` ; ou `boolean.should` d'une feuille par champ ; ou `{"type":"disjunction_max","queries":[…],"tie_breaker":0.0}` (`query.rs:601-622`) | — | `fields` n'est lu que par `parse` ; ailleurs `field` est obligatoire |
| filtres de champ | `"filters":[{"field":…,"op":"eq"\|"in"\|"between"\|"starts_with"\|"contains"\|…,"value":…}]` sur tout type (`query.rs:212-221, 455-464, 948-1094`) | ajoutés en `must` | le filtre `contains` a `distance` 0 par défaut, pas 1 comme le dit le commentaire (`:217` contre `:1084`) |
| filtre par identifiants | `search_filtered(…, allowed_ids: HashSet<u64>)` | sur les `_node_id` ; ensemble vide ⇒ rien (`sharded_handle.rs:2253-2286`) | les surlignages peuvent rester partiels si le puits déborde (`:2203-2208`) |

**Poids des champs : il n'y en a pas.** Ni `boost` par champ ni par clause dans `QueryConfig`
(`BoostQuery` existe dans `ld_lucivy`, `src/query/mod.rs:49`, mais `query.rs` ne le construit
jamais, et `build_search_dag_with_query` est `pub(crate)`, `search_dag.rs:659`). Pour pondérer
deux champs : deux recherches, et la pondération côté appelant.

**Pagination : pas d'`offset`.** Seulement `top_k`. Page *n* : `top_k = offset + taille`, puis
couper.

## 4. Le score

BM25, `K1 = 1.2`, `B = 0.75`, non réglables (`src/query/bm25.rs:8-9`). Pour un `contains`, un seul
« terme » : la fréquence documentaire est le nombre de documents qui contiennent la sous-chaîne,
les longueurs de champ viennent du champ tokenisé (`src/query/phrase_query/sfx_scoring.rs:120-153`).
`ShardedHandle::search` agrège déjà les statistiques de tous les shards (`sharded_handle.rs:2129-2135`).
Plusieurs index ou nœuds : `export_stats` sur chacun, `ExportableStats::merge`
(`lucivy_core/src/bm25_global.rs:195`), puis `search_with_global_stats`.

Depuis la 4.3.1, une statistique lue entre deux états de l'index (un terme vu dans plus de documents
que le compte) est bornée au lieu de faire paniquer la recherche (`bm25.rs`, `idf`) ; avant, la
panique tuait l'acteur de shard — si le nœud a vu des recherches qui échouent sous réindexation
avec la 4.3.0, c'était ça.

## 5. Surlignages : ce que l'API rend

Un `HighlightSink` (`src/query/phrase_query/scoring_utils.rs:63`) passé à la recherche se remplit des
spans ; on le lit par `sink.get(segment_id, doc_id) -> Option<HashMap<String, Vec<[usize; 2]>>>`
(`:122-137`) : nom du champ → intervalles **`[début, fin)` en octets UTF-8** dans la valeur stockée
du champ (`:50`). Pour un affichage en JavaScript, convertir en unités UTF-16 comme la démo
(`playground/index.html:2278-2296`, vérifié par `playground/test_highlight_mapping.mjs`) ; en Rust,
`&text[a..b]` sur la chaîne stockée.

Chaque occurrence a son span : la démo affiche une fenêtre de ±120 caractères autour de chacune,
fusionne celles qui se chevauchent, et en montre 5 (`index.html:2223-2274`). Plafond : 4 M spans en
natif (`LUCIVY_HIGHLIGHT_SPAN_CAP`) ; au-delà, la recherche se refait sur le top-k seul
(`sharded_handle.rs:2176-2227`).

## 6. Reproduire la démo en Rust

```rust
use std::sync::Arc;
use lucivy_core::query::QueryConfig;
use lucivy_core::sharded_handle::ShardedHandle;
use ld_lucivy::query::HighlightSink;

// La requête entière, un seul contains relâché, comme la démo.
let q: QueryConfig = serde_json::from_value(serde_json::json!({
    "type": "contains",
    "field": "content",          // le champ texte du nœud
    "value": user_text,          // tel quel, sans découpage
    // "distance": 1,            // si fuzzy : 1 au plus (décision du 25 octobre)
}))?;

// Le plus simple : les documents et leurs spans d'un coup.
let hits = handle.search_with_docs(&q, 20)?;   // Vec<SearchHit { score, shard_id, doc, highlights }>

// Ou, comme le binding WASM (bindings/emscripten/src/lib.rs:1328-1374, 1575-1596) :
let sink = Arc::new(HighlightSink::new());
let results = handle.search(&q, 20, Some(sink.clone()))?;   // Vec<ShardedSearchResult>
let node_ids = handle.node_ids_of(&results)?;               // les _node_id
for r in &results {
    let searcher = handle.shard(r.shard_id).unwrap().reader.searcher();
    let seg_id = searcher.segment_reader(r.doc_address.segment_ord).segment_id();
    let spans = sink.get(seg_id, r.doc_address.doc_id);    // champ -> [[début, fin)] en octets
}
```

Signatures (`lucivy_core/src/sharded_handle.rs`) :

```rust
pub fn search(&self, q: &QueryConfig, top_k: usize, sink: Option<Arc<HighlightSink>>) -> Result<Vec<ShardedSearchResult>, String>                       // :2136
pub fn search_filtered(&self, q: &QueryConfig, top_k: usize, sink: Option<Arc<HighlightSink>>, allowed_ids: HashSet<u64>) -> Result<…, String>          // :2166
pub fn search_with_global_stats(&self, q: &QueryConfig, top_k: usize, stats: &ExportableStats, sink: Option<Arc<HighlightSink>>) -> Result<…, String>     // :2755 — stats AVANT le puits
pub fn search_with_docs(&self, q: &QueryConfig, top_k: usize) -> Result<Vec<SearchHit>, String>                                                        // :2884 — ni filtre ni stats globales
pub fn fetch_docs(&self, results: &[ShardedSearchResult]) -> Result<Vec<LucivyDocument>, String>                                                       // :2821
pub fn query_warnings(&self, q: &QueryConfig) -> Vec<String>                                                                                           // :2158
pub fn last_search_truncated(&self) -> bool                                                                                                             // :1785
```

Écarts entre la démo et un appel natif, à connaître pour comparer des comptes : en WASM, au plus
20 000 correspondances par segment (natif 4 000 000, `src/suffix_fst/briques/resolve.rs:23-33`) et
1 000 000 spans (natif 4 000 000) ; `last_search_truncated()` dit quand ça a coupé. Les résultats
sont triés par score décroissant, les ex æquo par (shard, segment, doc) (`sharded_handle.rs:1595-1606`).

## 7. Ce que je conseille pour le nœud

- `contains` relâché sur la requête entière, comme la démo ; `distance: 1` seulement si le nœud
  sait l'afficher comme « approché » (scores négatifs, § 3) — ou deux appels, exact puis fuzzy si
  l'exact est vide.
- Les apostrophes : normaliser `’` en `'` dans la requête ne suffit pas (le texte garde `’`) ; pour
  du français, envoyer les deux formes en `boolean.should`.
- Garder `parse` pour une syntaxe explicite de l'utilisateur (`"…"`, `AND`, `-x`), jamais pour une
  saisie libre ; `query_warnings` le signale.
- Ne pas compter sur `boost`, `offset`, `slop` : ignorés en silence.
- Si l'exact doit passer devant (« Vue d'ensemble » avant « vue densemble »), retrier côté nœud en
  comparant le span surligné à la requête.
