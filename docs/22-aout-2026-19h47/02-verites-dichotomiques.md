# Vérités dichotomiques — contrats de données violés

> 22 août 2026 — branche `v3-recovery`
> Les mécanismes qui sont *écrits faux* : un contrat annoncé, un code qui fait autre
> chose. Ce ne sont pas des optimisations ni des paris. Ce sont des endroits où la
> donnée ment sur elle-même, et où la correction est nécessaire ne serait-ce que pour
> que le code redevienne lisible.

Les paris et les directions à explorer sont dans `01-intuitions-empiriques.md`.
Ce document-ci ne contient que du constaté.

## Convention de provenance

| Marque | Sens |
|---|---|
| `[vérifié]` | lu directement dans le code de cette session |
| `[mesuré]` | résultat d'une exécution de test de cette session |
| `[rapporté]` | issu d'une analyse non revérifiée ligne à ligne |

---

# 1. Le merge corrompt les index v3, aujourd'hui, en silence

C'est le seul point de ce document qui soit **urgent**. Tous les autres sont de la dette.

## 1.1 Le chemin de merge ne consulte jamais la version

`[vérifié]` Le `sfx_version` des settings n'est lu qu'à **deux endroits** dans tout le
repo : `segment_writer.rs:96` et `:146`, pour choisir `SfxCollectorSlot::V3` vs `V2`.
Partout ailleurs, c'est `detect_sfx_version` sur la magic du fichier — donc côté lecture
uniquement.

`[vérifié]` `merge_dag.rs:268` appelle `sfx_dag::build_sfx_dag` — le DAG **v2** — sans
aucune condition, gardé seulement par `any_has_sfx`.

**Un index v3 qui merge aujourd'hui passe donc par le DAG v2.** Ce n'est pas un risque
futur conditionné à un branchement : c'est l'état courant, et il suffit que la merge
policy se déclenche.

## 1.2 Le DAG v2 lit un alphabet différent

`[vérifié]` `sfx_merge.rs:100` : `collect_tokens` lit
`reader.inverted_index(field).terms()` — le dictionnaire de l'index inversé, **pas**
`.termtexts`.

Or en v3 l'index inversé est alimenté par l'analyzer standard, séparément du collector
SFX. Son dictionnaire contient `mutex`, `lock` — pas les textes étendus, pas les entrées
word-stripped. Ce n'est donc pas une déduplication qui perd la partition : c'est **un
alphabet différent**.

`[rapporté]` Conséquence en chaîne :
- `merge_sfxpost` indexe le `.sfxpost` source avec les ordinaux du dictionnaire de termes,
  alors que le `.sfxpost` v3 est clé sur les ordinaux finaux v3 → désalignement total ;
- `copy_gapmap` ouvre le `.sfx` v3 avec le reader v1/v2, qui exige la magic `SFX1` →
  `None` → gapmap intégralement vide ;
- `or_merge_indexes` cherche `.sibling` et `.sepmap`, que v3 n'écrit jamais → vide ;
- `WriteSfxNode` écrit la magic `SFX1` et un `.termtexts` en `TTXT` (pas `TTX3`) ;
- `.word_sfxpost`, `.chunk_word_map`, `.next_word_map`, `.word_pos_map`, `.sibling_v3`
  ne sont pas écrits du tout.

## 1.3 Le résultat est un v2 bien formé qui ment

`[rapporté]` Le segment produit est un **v2 parfaitement cohérent avec lui-même** : magic
`SFX1`, `.termtexts` en `TTXT`, `.posmap`/`.bytemap` cohérents avec son propre `.sfxpost`.
Aucune exception nulle part. Mais les postings pointent sur les **mauvais termes**, avec
des doc_ids et des offsets d'octets réels.

Et comme les deux alphabets sont tous deux triés sur le même corpus, l'ordinal *k* v3 et
l'ordinal *k* du dictionnaire tombent dans un voisinage lexical proche : les résultats
seront **presque plausibles**. C'est le pire mode d'échec possible pour du diagnostic.

À décharge : l'index inversé BM25 du segment mergé, lui, reste correct. Seule la couche
SFX est corrompue.

## 1.4 Le seul garde existant ne se déclenche jamais

`[rapporté]` `validate_sfxpost` compare le fichier **qu'il vient d'écrire** à
`tokens.len()` — alors qu'il a été construit par `SfxPostWriterV2::new(tokens.len())`.
Auto-cohérent par construction : il ne peut pas échouer.

`[vérifié]` Le seul vrai garde de version est celui de `merge_segments_v3`
(`sfx_dag_v3.rs:293-297`, `detect_termtexts_version` → `"reindex required"`) — sur une
fonction que **rien n'appelle en production** (`[vérifié]` seuls ses propres tests
l'appellent, lignes 596, 625, 650, 676).

## 1.5 `has_word_pipeline()` transforme la panne en silence

`[vérifié]` `context.rs:75` :

```rust
pub fn has_word_pipeline(&self) -> bool {
    self.posmap.is_some() && self.bytemap.is_some() && self.word_sfxpost.is_some()
}
```

Test de présence, pas de contenu. `[rapporté]` Un `.word_sfxpost` vide s'ouvre
correctement avec `num_ordinals = 0` → la fonction renvoie `true` → toutes les
résolutions 0x02 renvoient vide, **sans erreur ni log**. C'est le maillon qui transforme
la corruption du §1.2 en dégradation invisible.

**Correction minimale : exiger `num_ordinals > 0`.** Trois lignes.

## 1.6 `merge_segments_v3` a l'air d'un merge et n'en est pas un

`[vérifié]` `sfx_dag_v3.rs:423-443` — cinq structures écrites **vides**, avec des `// TODO`
explicites :

```rust
// TODO: rebuild word sfxpost from merged token data
word_sfxpost: WordSfxPostWriter::new(0).finish(),
// TODO: rebuild word maps from merged token data
chunk_word_map: ChunkWordMapWriter::new(final_ord as usize).serialize(),
next_word_map:  NextWordMapWriter::new(0).serialize(),
word_pos_map:   WordPosMapWriter::new().serialize(),
// TODO: rebuild sibling table from merged token data
sibling_v3:     SiblingTableWriter::new(0).serialize(),
```

Plus deux champs **faussés en dur** (`[vérifié]`, ligne ~334) :

```rust
word_id: 0,               // word_id is segment-local, not meaningful across merge
is_word_stripped: false,  // (aucun commentaire)
```

`[mesuré]` Et pourtant `test_merge_two_segments` **passe** — il ne valide que la partition
chunk, jamais 0x02. La fonction a donc toutes les apparences d'un merge fonctionnel.

Perdre `sibling_v3` ne casse d'ailleurs pas que le relax : la sibling table est utilisée
par les chaînes chunk, donc par le contains cross-token **strict**.

**Correction : rendre la fonction honnête.** `unimplemented!()` sur le chemin word,
suppression, ou renommage en `merge_segments_v3_chunks_only`. Ne pas la laisser ressembler
à ce qu'elle n'est pas.

## 1.7 Le merge v3 reproduit exactement l'invariant que le projet s'était donné

`[rapporté]` `merge_segments_v3` construit `global_intern: HashMap<String, u32>`
(`sfx_dag_v3.rs:301, 323`) puis `ord_map: BTreeMap<String, ...>` (`:373, 377`), **clés sur
le texte seul**.

C'est mot pour mot la structure que `docs/30-mai-2026/01-findings-partition-v3-fn-fp.md`
interdit :

> Toute structure qui mappe texte → donnée dans le collector doit être partitionnée par
> type (chunk vs word-stripped). Un BTreeSet qui déduplique par texte détruit cette
> partition → régression silencieuse.

Un merge fusionnerait donc un ordinal dont les postings sont en coordonnées chunk avec un
ordinal dont les postings sont en coordonnées word.

## 1.8 La partition n'est pas dans le format

C'est la racine du §1.7, et elle mérite d'être énoncée seule.

`[rapporté]` `intern_extended` sépare les namespaces avec le préfixe `\x00ws:`, et
`into_data` avec `C:` / `W:` — précisément pour que `"functional"`-chunk et
`"functional"`-word-stripped n'entrent pas en collision. **Ces préfixes n'existent qu'en
mémoire.** `TermMetaV3` sérialise 6 octets (`own_len`, `sep_len`, `overlap_len`,
`is_word_start`, `reserved`) — ni la partition, ni `is_word_stripped`.

Donc l'invariant central du projet est respecté à l'indexation et **perdu à l'écriture**.
Toute relecture doit le redériver, et le merge ne le redérive pas.

`[rapporté]` Correction : l'octet `reserved` de `TTX3` est **déjà libre**. Coût zéro
octet, changement rétro-compatible. La faire maintenant, pendant que le format bouge,
plutôt que d'imposer une réindexation plus tard.

## 1.9 La clé FST 0x02 pointe sur un ordinal de chunk

`[rapporté]` `build_word_stripped_pub` renseigne `first_intern_ord = first_idx`,
c'est-à-dire l'index d'un **chunk**. `BuildFstV3Node` fait ensuite
`data.intern_to_final[ws.first_intern_ord]` : la clé FST de partition 0x02 pointe donc
sur l'ordinal d'un chunk.

À la lecture, `is_word_stripped()` est vrai (dérivé de l'octet de partition de la clé),
donc `resolve_single_v3` skippe, et `resolve_single_word_v3` cherche dans un
`.word_sfxpost` vide. Double FN, sans la moindre erreur.

`[mesuré]` Ce chemin est **spécifique au merge** : le chemin d'indexation normal regroupe
sur la valeur locale fraîche, ce qui explique que le ground truth soit à 15/15.

## 1.10 `word_id` est un champ qui ment

`[rapporté]` `TokenMetaV3.word_id` vient d'un compteur **local** remis à zéro à chaque
`add_value`, et `intern_extended` ne conserve que la valeur de la **première occurrence**.
Un token `"lock"` peut être `word_id=1` dans une valeur et `word_id=5` dans une autre :
la valeur stockée est arbitraire.

Le persister ne réparerait donc rien — le champ est déjà sémantiquement faux dans le
collector. Il n'est simplement jamais lu sur le chemin normal. L'identité de mot correcte
est ailleurs : `word_intern` / `.chunk_word_map`.

`[rapporté]` Conséquence concrète au merge : `merge_segments_v3` appelle
`build_word_stripped_pub`, qui groupe par `meta.word_id`. Avec tous les `word_id` à 0,
**tous les tokens du corpus mergé tombent dans un seul groupe** → une seule
`WordStrippedEntry` dont le contenu est la concaténation de tout l'index. La partition
0x02 du segment mergé devient un mot unique absurde.

**Correction minimale : ne pas laisser un champ nommé `word_id` porter une valeur qui
n'identifie pas un mot.** Le retirer de `TokenMetaV3`, ou le renommer en
`first_seen_local_word_id` avec la mise en garde.

---

# 2. `exact_match` : une sémantique portée par une coïncidence d'octets

C'est le piège le plus dangereux du chantier F3, et aucun document ne l'avait relevé.

## 2.1 Le contrat réel

`[vérifié]` `orchestrator.rs:72` :

```rust
if exact_match {
    matches.retain(|m| m.byte_to.saturating_sub(m.byte_from) == query_content_len);
}
```

Avec `byte_to` = fin du **contenu du token conteneur**, cette égalité signifie « le match
va jusqu'au bout du token et fait la bonne longueur », c'est-à-dire **whole-token match**.
Combiné à `anchor_start` (`sti = 0`), c'est l'équivalent v3 du `si + qlen >= token_len` de
la v2.

`[rapporté]` Et `term` est routé sur `contains + anchor_start + exact_match`
(`lucivy_core/src/query.rs:186-194`) — c'est le seul consommateur.

**Donc la sémantique de `term` tout entière repose sur une propriété accidentelle de
`byte_to`.** Rien ne la nomme, rien ne la teste, rien n'empêche de la casser.

## 2.2 Corriger `byte_to` détruit `term` silencieusement

`[rapporté]` Si `byte_to` devient `byte_from + query_len` (le fix attendu), alors
`byte_to - byte_from` vaut **toujours** `query_len` → le `retain` devient toujours vrai →
**`term` dégénère en `contains`**. `term "mut"` matcherait `"mutex_lock"`.

Et le test existant `handle.rs:1069-1070` (`term "mutex"` > 0) **passerait quand même** :
il ne teste que le sens positif.

**Correction obligatoire, dans le même commit que tout fix de `byte_to`** : réimplémenter
`exact_match` sur un signal explicite porté par `MatchV3` (par exemple `covers_token_end`,
posé à la résolution où `cand.content_len()` et `cand.sti` sont disponibles), jamais sur
un span d'octets. Plus un **test négatif** `term "mut"` → 0, sans lequel la régression est
invisible.

## 2.3 `exact_match` est déjà cassé aujourd'hui

`[vérifié]` `orchestrator.rs:53` :

```rust
let query_content_len = query_ref.chars().filter(|c| is_content_char(*c)).count() as u32;
```

C'est un **compte de caractères**, comparé lignes 66 et 72 à un **span en octets**.

Conséquences immédiates, indépendantes de tout fix futur :
- toute requête non-ASCII est cassée : `term "café"` → 5 octets vs 4 chars → zéro résultat ;
- toute requête stricte contenant un séparateur est cassée : `term "mutex_lock"` → span 10
  vs 9 chars de contenu → zéro résultat.

`[rapporté]` Aucun test ne couvre ces cas. Le `>=` du filtre `content_len` (ligne 66)
masque l'écart ; le `==` d'`exact_match` non.

**Correction : passer `query_content_len` en octets.** Indépendante du reste.

---

# 3. `byte_to` a quatre sémantiques différentes selon le chemin

## 3.1 Le contrat annoncé

`[vérifié]` `src/query/posting_resolver.rs:21` :

```rust
/// End byte offset (exclusive) of the term in the original text.
```

## 3.2 Ce qui est réellement écrit

`[vérifié]` `collector_v3.rs:267` :

```rust
let byte_to = (offset + meta.content_len + meta.sep_len) as u32;
```

**Le séparateur est inclus.** Le doc-comment est donc faux : ce n'est pas la fin du terme.

## 3.3 Ce que la résolution en fait

`[rapporté]` sauf mention contraire :

| Chemin | `byte_to` | Réf |
|---|---|---|
| `resolve_single_v3` (chunk) | `e.byte_from + own_len - sep_len` = fin du **contenu du chunk** | `resolve.rs:71` |
| `resolve_single_word_v3` (0x02) | `e.byte_from + content_len` = fin du **contenu du mot** `[vérifié]` | `resolve.rs:117` |
| chaîne word, longueur 1 | encore la fin du contenu du mot | `resolve.rs:219-223` |
| chaîne word, multi | `e.byte_to` du dernier posting = fin du dernier chunk, **séparateur inclus** | `resolve.rs:235, 289` |
| chaîne chunk | `e.byte_to` du dernier chunk, **séparateur inclus** | `resolve.rs:353, 367` |

Quatre conventions différentes, aucune n'étant « fin du match », aucune n'étant celle du
doc-comment.

`[rapporté]` `MatchV3.byte_to` est documenté « End byte offset (exclusive) in the original
text » sans dire **de quoi**. Et un `grep byte_from` sur `docs/` ne donne **aucun résultat** :
la sémantique n'est spécifiée nulle part.

## 3.4 Le commentaire de `resolve_single_word_v3` ment

`[vérifié]` `resolve.rs:105-117` annonce :

```rust
// Adjust byte_from by sti (suffix offset within the word) and
// compute byte_to to cover exactly the query match, not the whole word.
let content_len = cand.content_len() as u32;
let query_byte_len = content_len - cand.sti as u32;
byte_from: e.byte_from + cand.sti as u32,
byte_to:   e.byte_from + cand.sti as u32 + query_byte_len,
```

L'arithmétique donne `byte_to = e.byte_from + content_len` : le `sti` s'annule. C'est
**exactement `the whole word`**, ce que le commentaire dit ne pas faire. Et
`query_byte_len` ne porte pas la longueur de la query : c'est la longueur de contenu
restante depuis `sti`.

Quelqu'un a cru corriger le Finding 3 sans le corriger. Le commentaire est plus dangereux
que le bug : il fait croire le contraire de ce qui se passe, et il m'a moi-même induit en
erreur pendant cette session.

## 3.5 C'est une régression par rapport à la v2

`[rapporté]` En v2, `byte_to = byte_from + query_len` — exactement les octets du match
(`suffix_contains.rs:178, 280, 730`), avec des assertions explicites
(`suffix_contains.rs:1183` : « 7 + len("rag")=3 »). Et v2 implémentait `exact_match`
**sans toucher aux octets** : `si + qlen >= token_len` (`suffix_contains_query.rs:326`).

Le fix ne casse donc pas une sémantique établie : il **restaure** celle de la v2.

## 3.6 Exemple concret non couvert par un test

`[rapporté]` Sur `"hello_world_test"`, requête stricte `"ello_world"` → chaîne de 2 chunks,
`byte_from = 1`, `byte_to = e.byte_to` du chunk `"world_"` = 12 → highlight `[1, 12)` =
`"ello_world_"`, **underscore final inclus**. Le test `integration_tests.rs:609-611` ne
vérifie que `bf < bt <= text.len()`.

## 3.7 Le fix

`[rapporté]` L'option « faire porter à `WordPostingEntry` les octets du match » est
**impossible par construction** : un posting est indexé une fois, indépendamment de la
requête ; `sti` et la longueur du match sont des propriétés de la requête. Et l'information
de fin de contenu est **déjà dérivable côté requête** via
`FstCandidateV3::content_len() = own_len - sep_len` — le champ `byte_to` de
`WordPostingEntry` est redondant. Changer le format serait payer une réindexation pour une
information qu'on a déjà.

La correction est donc côté résolution, sans changement de format :
- `byte_to = byte_from + query_len`, clampé à la fin de contenu pour 0x02 (au-delà, le
  match franchit un séparateur dont on ne connaît pas l'offset ; débord borné à
  `DEFAULT_OVERLAP = 2` octets) ;
- ajouter la consommation de la dernière position à `TokenChainV3` (**struct en mémoire,
  aucun format sur disque**) pour les chaînes multi ;
- en relax, `query_len` doit être la longueur de la query **déjà strippée**.

`[rapporté]` Sites concernés : 6 constructions de `MatchV3` dans `resolve.rs` (69-71,
113-117, 219-223, 235/289/305-310, 350-355, 367/411/427-431), une 7ᵉ dans `composite.rs`
(233-240), plus les signatures de `resolve_single_v3` et `resolve_single_word_v3`.

## 3.8 Le filtre `content_len` devient tautologique — et il génère des FN aujourd'hui

`[vérifié]` `orchestrator.rs:66` :

```rust
matches.retain(|m| m.span > 1 || m.byte_to.saturating_sub(m.byte_from) >= query_content_len);
```

`[rapporté]` Ce qu'il attrape réellement : il rejette les matches single-token dont la
requête déborde du contenu du token. Exemple : `"ex_l"` sur `"mutex_lock"` en strict →
candidat 0x01, `sti=3`, `content_len=5` → span 2 < 3 → **droppé alors que le match est
réel**. Il n'est rattrapé que parce que le pipeline chaîne le récupère avec `span=2`.

`[rapporté]` Attrape-t-il de vrais FP ? Non : les ordinaux 0x00/0x01 sont *extended* (un
ordinal par texte étendu unique), donc le texte de la clé FST **est** le texte réellement
présent à chaque occurrence. « La requête est préfixe de la clé » **prouve** l'occurrence.
Le commentaire « Filter false positives from content ordinals » (`orchestrator.rs:52`) est
**périmé** : il date du design où plusieurs textes partageaient un content ordinal.

Après le fix, le `retain` devient tautologique. **À supprimer, pas à conserver « au cas où ».**

---

# 4. `span` a deux unités différentes

`[vérifié]` Dans `resolve.rs` :

| Ligne | Valeur | Unité |
|---|---|---|
| 113, 219 | `last_position - first_position + 1` | nombre de **chunks** |
| 307, 429 | `chain.ordinals.len()` | nombre de **mots** (chaîne word) / d'ordinaux (chaîne chunk) |

`[vérifié]` Et le consommateur `contains_query_v3.rs:147-150` fait :

```rust
if m.span <= 1 { return true; }
let first_pos = m.position;
let last_pos = m.position + m.span - 1;
```

puis interroge les maps position par position — il traite donc `span` comme un **compte de
tokens (chunks)**. Les matches issus d'une chaîne word sont donc post-filtrés sur une plage
de positions **fausse**.

C'est la même famille de bug que §3, sur un autre champ, et aucun document ne la mentionne.

**Correction : une seule unité, nommée.** Soit `span_tokens`, soit deux champs distincts.
Pas un `u32` dont l'unité dépend du chemin qui l'a produit.

---

# 5. Asymétries entre chemins qui devraient être équivalents

## 5.1 `find_literal_v3` n'appelle pas `resolve_single_word_v3`

`[vérifié]` `composite.rs:43-44` :

```rust
let candidates = fst_walk::fst_candidates_v3(ctx.reader, query, anchor_start, strict_separators);
let single = resolve::resolve_single_v3(&candidates, ctx.resolver, ctx.filter_docs);
```

`[vérifié]` Alors que le DAG (`dag_builder.rs:105-106`, `dag_nodes.rs:223`) **et** le
pipeline fuzzy (`composite.rs:313, 573`) câblent bien `resolve_single_word_v3`.

Le chemin de production `ContainsQueryV3` passe par `find_literal_v3`. Il n'a donc **aucun
chemin de résolution directe** pour la partition 0x02 : un mot word-stripped exact dépend
entièrement des chaînes pour être rattrapé.

`[mesuré]` **Nuance importante** : ce n'est pas un bug actif. Le ground truth est à 15/15,
`TableFunction` relax à 172/172 — les chaînes word rattrapent bien le cas. C'est une
asymétrie latente, à corriger pour la cohérence, pas une urgence.

## 5.2 Le DAG n'a aucun appelant en production

`[rapporté]` `find_literal_v3_dag` n'est appelé que par ses tests de parité. Les deux
implémentations peuvent donc diverger — comme en §5.1 — sans qu'aucun test de production
ne le voie. Un test de parité qui compare deux chemins dont un seul est utilisé ne protège
que la moitié du contrat.

---

# 6. Instrumentation et code mort qui mentent

## 6.1 `resolve_trigrams_v3_explained` décrit un pipeline qui n'existe plus

`[rapporté]` `composite.rs:531-647` n'a **aucun appelant** (`src/`, `lucivy_core/`,
`bindings/`) et utilise encore la fenêtre glissante `max_window` (`composite.rs:608-632`),
alors que la production est passée aux briques `resolve_all_trigrams` /
`build_trigram_chains` / `filter_by_chain_threshold`.

L'outil censé expliquer le fuzzy explique donc son prédécesseur. À migrer ou supprimer
**avant** toute campagne de diagnostic — c'est un piège qui coûtera une session entière à
qui s'y fiera.

## 6.2 Le tri par sélectivité est du coût pur

`[vérifié]` `resolve_all_trigrams` (`composite.rs:294-320`) trie les n-grammes par
sélectivité, mais résout ensuite avec `filter_docs = None` (`composite.rs:311`) et
`build_trigram_chains` re-trie par `byte_from` de toute façon. L'ordre n'influence plus
rien.

`[vérifié]` Pire, `fst_candidates_v3` est appelé **deux fois à l'identique** par n-gramme :
une fois pour mesurer la sélectivité (`composite.rs:301`), une fois pour résoudre
(`composite.rs:310`).

Une phase entière du code annonce un rôle qu'elle ne joue plus.

## 6.3 `best_bt` ne correspond pas à la chaîne

`[rapporté]` `composite.rs:400-404` :
`sorted.iter().filter(|h| h.tri_idx == last_tri && h.byte_from >= best_bf).next()` prend
la **première occurrence globale** du dernier trigramme après `best_bf` — pas celle qui
appartient à la chaîne retenue. Le highlight est donc arbitraire.

## 6.4 Code mort annoncé et jamais retiré

`docs/24-mai-2026-15h04/08-recap-session-6.md` §5 annonce le retrait de
`cross_chunk_chain_v3`, `cross_word_chain_v3`, `build_chains_from_splits` et
`best_consumed` une fois la sibling table stable. Les quatre sont toujours présents.

## 6.5 Sections de format déclarées et jamais écrites

`[rapporté]` Les sections `SECTION_WORD_MAP` et `SECTION_NEXT_WORD` du `.sfx` v3 sont
déclarées dans l'en-tête (`file_v3.rs:26-27`) mais jamais écrites : le TODO
`sfx_dag_v3.rs:209` n'a jamais été levé, y compris sur le chemin d'indexation normal. Un
format qui annonce des sections vides est un format qui ment à ses lecteurs.

## 6.6 L'arsenal documente des fichiers que v3 ne produit pas

`[rapporté]` `docs/30-mai-2026/03-arsenal-index-v3.md` liste `.sepmap` et `gapmap` parmi
les 13 structures. Or v3 n'en produit **ni l'un ni l'autre** : `build_derived_indexes_v3`
filtre explicitement `sepmap`, et `gapmap` est un `ExternalDagNode` jamais construit sur le
chemin v3.

---

# 7. Approximations assumées mais non tracées

## 7.1 Les entrées tail des mots longs ont des octets approximatifs

`[rapporté]` Les entrées *tail* des mots très longs réutilisent le posting du **dernier
chunk** alors que leur contenu commence à `word_content.len() - max_token`. Le collector
l'admet lui-même en commentaire : « byte ranges … may be approximate ».

Impact borné à `max_token = 8` octets, sur des mots de plus de 264 octets. Ce n'est pas
grave, mais c'est le seul endroit où `byte_from + sti` est **réellement** faux — et ce
n'est ni testé, ni signalé au consommateur.

## 7.2 `dedup_by_key` collapse des matches légitimement distincts

`[vérifié]` `orchestrator.rs:68` : `matches.dedup_by_key(|m| (m.doc_id, m.position))`.

`[rapporté]` Le tri précédent garantit que les mêmes `(doc, position)` sont adjacents, donc
le dédoublonnage fonctionne. Mais il collapse aussi deux occurrences **réellement
distinctes** à la même position de token avec des `sti` différents — par exemple `"ab"`
dans `"abab"` si les deux tombent dans le même chunk.

Et aujourd'hui, quand un même `(doc, position)` arrive de 0x00 (span jusqu'à la fin du
chunk) et de 0x02 (span jusqu'à la fin du mot), **le survivant est arbitraire** → highlights
non déterministes. Après le fix du §3, les deux chemins produisent le même `byte_to` et le
non-déterminisme disparaît par construction.

---

# 8. Ce qui n'est *pas* un contrat violé

Pour éviter que ces points soient « corrigés » par erreur dans un futur passage.

## 8.1 Les skips `sep_len == 0` sont des invariants du modèle

`[vérifié]` `builder_v3.rs:333-335` :

```rust
// No sep → nothing to strip, chunks in 0x00/0x01 already cover this word.
if first_sep_len == 0 { return; }
```

`[rapporté]` Et par construction du tokenizer, un segment n'a un séparateur vide que s'il
est le **dernier du texte**. Donc `sep_len == 0` ⇔ « aucun mot ne suit ». Les `extra`
filtrés dans `composite.rs:108-111` sont des **splits**, c'est-à-dire des amorces de chaîne
*cross-mot* : un mot sans successeur ne peut pas en amorcer une.

La thèse de `docs/30-mai-2026/01-findings-partition-v3-fn-fp.md` (« `TableFunction`,
camelCase mono-mot, `sep_len = 0`, donc éliminé ») est **fausse dans le cas général** :
dans du code réel, `TableFunction` est suivi de `<`, `(`, ` ` — donc `sep_len > 0`. Elle
n'est vraie qu'en fin de fichier.

**Ne pas toucher à ces filtres.** Ce sont des invariants, pas des pansements.

## 8.2 La partition 0x02 n'est pas supprimable

`[mesuré]` Elle sert le matching **agnostique** aux séparateurs — `mutexlock` → `mutex_lock`,
`TableFunction` → `table function` — et elle est aujourd'hui à **zéro erreur** sur les
quatre queries relax du ground truth (`uint64_t`, `std::unique_ptr`, `ku_dynamic_cast`,
`TableFunction`). Un index de trigrammes d'octets bruts ne peut pas la remplacer : les
octets diffèrent.

## 8.3 Les deux tests rouges du pipeline sont des fixtures, pas des bugs

`[vérifié]` `diag_false_positive_uint64t` et `test_resolve_chain_sep_skip` sont documentés
rouges depuis le **19 mai** (`docs/19-mai-2026/05-recap-session-5-complete.md:46-48`), avec
leur cause : ils appellent le pipeline avec `None` pour les maps, ou `resolve_chains_v3` en
direct sans le pipeline word. `[mesuré]` Le corpus réel passe à 15/15.

À migrer vers de vraies maps — pas à traiter comme des régressions.

---

# Ordre de correction

| # | Correction | Coût | Pourquoi maintenant |
|---|---|---|---|
| 1 | **Garde de merge** : refuser le merge si `sfx_version >= 3` (`merge_dag.rs:268`) | ~20 lignes | Arrête une corruption **active et silencieuse**. Coût nul pour les corpus d'évaluation actuels (build unique puis query). |
| 2 | `has_word_pipeline()` exige `num_ordinals > 0` (`context.rs:75`) | 3 lignes | Transforme une dégradation invisible en échec lisible. |
| 3 | Rendre `merge_segments_v3` honnête (§1.6) | ~10 lignes | Supprime un piège : la fonction a l'air de marcher et ses tests passent. |
| 4 | `query_content_len` en octets (`orchestrator.rs:53`) | 1 ligne | `exact_match` est **déjà** cassé sur l'unicode et sur les séparateurs. Indépendant du reste. |
| 5 | Octet de partition dans le `reserved` de `TTX3` (§1.8) | petit | Gratuit maintenant, réindexation forcée plus tard. |
| 6 | `byte_to` + `exact_match` + test négatif, **en un seul commit** (§2, §3) | moyen | Livrer `byte_to` seul transforme un FN en une classe entière de FP sur `term`, et aucun test ne le verra. |
| 7 | Unifier l'unité de `span` (§4) | petit | Même famille que §3, autant le faire dans le même passage. |
| 8 | Câbler `resolve_single_word_v3` dans `find_literal_v3` (§5.1) | 1 ligne | Cohérence prod/DAG. Latent, pas urgent. |
| 9 | Migrer ou supprimer `resolve_trigrams_v3_explained` (§6.1) | petit | **Avant** toute campagne de mesure fuzzy. |
| 10 | Corriger les doc-comments menteurs (§3.1, §3.4) | trivial | Un commentaire faux coûte plus cher qu'un commentaire absent : celui de `resolve.rs:105` a induit en erreur pendant cette session. |
