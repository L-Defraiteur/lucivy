# Intuitions empiriques — pistes à tester

> 22 août 2026 — branche `v3-recovery`
> Toutes les directions ouvertes à ce jour, avec pour chacune : l'hypothèse,
> ce qui l'étaye, comment la confirmer ou la réfuter, et ce qu'on en attend.

## Convention de provenance

| Marque | Sens |
|---|---|
| `[mesuré]` | résultat d'une exécution de test de cette session |
| `[vérifié]` | lu directement dans le code de cette session |
| `[rapporté]` | issu d'une analyse non revérifiée ligne à ligne |
| `[hypothèse]` | raisonnement non confirmé |

Rien de ce document n'est un fait acquis. Les faits acquis sont dans
`02-verites-dichotomiques.md`.

---

## Contexte de mesure (22 août 2026)

Remise en route après changement de machine : toolchain Rust absent, corpus absent.
Rust 1.98.0 installé, `rag3db` recloné dans `/tmp/rag3db-bench` (5616 fichiers, 540 Mo).

| Mesure | Résultat |
|---|---|
| `cargo check --workspace` | `ld-lucivy`, `lucivy_core`, `luciole` compilent — seules erreurs : `bindings/python` (PyO3 vs Python 3.14) et un bench `bitpacker` (nightly) |
| `cargo test --lib` | 1426 passent, 3 échouent, 16 ignorés |
| Ground truth contains, 5000 docs | **15/15**, zéro FN, zéro FP |
| Baseline fuzzy/regex, 500 docs | 4/11 (fuzzy 2/6, regex 2/5) |

Le contains v3 est donc **terminé**. Tout ce document porte sur ce qui reste.

---

# A. Fuzzy

## A1 — Les FP restants s'expliquent entièrement par « filtre sans preuve »

**Hypothèse.** Le pigeonhole trigramme est une condition *nécessaire* et jamais
*suffisante*. Sans vérification finale, le taux de FP est une fonction directe de
la sélectivité des n-grammes de la query, et de rien d'autre.

**Ce qui l'étaye.** `[mesuré]` La corrélation est nette sur les 6 queries :

| Query | n-grammes | Fréquence en corpus C++ | FP |
|---|---|---|---|
| `rag3db` | `g3`, `3d` | rares | **0** |
| `strcuture` | — | — | **0** (grep 0 aussi) |
| `inclde` | `in`, `nc` | fréquents | 3 |
| `uint64` | `in`, `nt` | fréquents | 5 |
| `functin` | `fun`, `unc` | fréquents | 5 |
| `retrun` | `re`, `et` | très fréquents | 13 |

Le pipeline n'échoue que quand ses n-grammes ne sont pas sélectifs. C'est la
signature exacte d'un filtre de candidats utilisé comme preuve d'appartenance.

**Comment vérifier.** Ajouter la vérification Levenshtein (A2) et remesurer. Si
l'hypothèse tient, les FP tombent à zéro sur les six, sans toucher au seuil.

**Attendu.** 6/6 sur le fuzzy, ou l'identification d'une seconde cause.

## A2 — La vérification Levenshtein est faisable sans I/O docstore

**Hypothèse.** On peut reconstruire une fenêtre de texte autour de `(doc_id, position)`
d'un hit à partir de `posmap` (position → ordinal) et `termtexts` (ordinal → texte
étendu + `own_len`/`sep_len`), puis y faire tourner un DP semi-global. Aucune lecture
du docstore, aucune décompression.

**Ce qui l'étaye.** `[vérifié]` `posmap` et `bytemap` sont **déjà chargés** dans le
contexte fuzzy (`fuzzy_query_v3.rs:80-81`) et **inutilisés** par ce chemin — le tableau
« qui utilise quoi » de `docs/30-mai-2026/03-arsenal-index-v3.md` le confirme :
la ligne `fuzzy (trigram)` ne liste que `.sfx`, `.sfxpost`, `.word_sfxpost`.
`[vérifié]` Un DP semi-global de référence existe déjà, mais uniquement côté test :
`fuzzy_substring_exists` dans `test_sfx_v3_ground_truth.rs`.

**Comment vérifier.** Prototyper la reconstruction de fenêtre sur un doc connu et
comparer au texte réel avant même de brancher le DP.

**Attendu.** Élimination de 100 % des FP par construction, sans introduire de FN.
Point d'attention : en mode relax la vérification doit se faire sur le **texte strippé**,
pour coller à la sémantique de la ground truth qui strippe des deux côtés.

## A3 — Le seuil doit être *baissé*, pas monté

**Hypothèse.** Une fois A2 en place, le seuil ne sert plus qu'au rappel et au coût.
Le monter ne fait que créer des FN.

**Ce qui l'étaye.** `[vérifié]` `composite.rs:470` : `threshold = (T - n·d).max(2)`.
`[hypothèse]` Pour `functin` (7 octets, n=3, 5 trigrammes, d=1) le seuil vaut
`max(5-3, 2) = 2` : deux trigrammes consécutifs suffisent, donc tout document
contenant le token nu `func` produit `fun`@p + `unc`@p+1 et passe — alors que
`distance("func", "functin") ≥ 3`. `[mesuré]` 5 FP observés sur `functin`, cohérent.

**Comment vérifier.** Instrumenter les chaînes acceptées sur les 5 docs FP de
`functin` et regarder si elles sont bien de longueur 2 ancrées sur `func`.

**Attendu.** Confirmation que le seuil produit des FP *structurels*, et qu'après A2
on peut redescendre à `max(T - n·d, 1)` sans risque.

## A4 — Le vrai décalage de coordonnées est query-strippée vs index-brut

**Hypothèse.** Ce n'est pas un mélange chunk/word (hypothèse initiale, **réfutée**),
c'est que `query_positions` est en espace strippé alors que `byte_from` est en espace
brut, séparateurs inclus.

**Ce qui l'étaye.** `[vérifié]` `orchestrator.rs:106-110` strippe les séparateurs de
la query en relax. `[vérifié]` `resolve_single_word_v3` (`resolve.rs:112`) ajoute bien
`cand.sti` à `byte_from` — les hits 0x02 portent donc l'offset du premier octet
apparié, **pas** le début du mot. Les deux chemins produisent des offsets absolus dans
le même espace brut. `[vérifié]` `build_trigram_chains` (`composite.rs:386-392`) compare
`expected_gap` (strippé) à `actual_gap` (brut) avec tolérance `±d`.

`[rapporté]` À chaque franchissement de séparateur l'écart vaut `sep_len`. Sur
`mutex_lock` le pas passe mais consomme **tout** le budget d'erreur ; sur `::`, `->`,
`\r\n`, `);` il est rejeté. Et quand un pas est rejeté, `prev_bf`/`prev_qp` ne bougent
pas : la dérive devient permanente et tue toute la queue de la chaîne.

**Comment vérifier.** Construire un index sur `mutex::lock::init`, chercher
`mutexlockinit` en relax d=1, et observer la longueur de la meilleure chaîne. Prédiction :
5 au lieu de 11.

**Attendu.** Récupération de toute une classe de FN cross-séparateur.

## A5 — Le fix est une normalisation, pas un routage

**Hypothèse.** Porter un `stripped_from` sur chaque `TrigramHit` et chaîner là-dessus,
plutôt que de router les partitions selon `strict_separators`.

**Ce qui l'étaye.** `[vérifié]` `builder_v3.rs:333-335` :

```rust
// No sep → nothing to strip, chunks in 0x00/0x01 already cover this word.
if first_sep_len == 0 { return; }
```

**Tout mot non suivi d'un séparateur n'a aucune entrée en partition 0x02** — dernier
mot de chaque valeur, fin de segment. Router le relax vers 0x02 seul les rendrait
invisibles. Le routage proposé dans `docs/30-mai-2026/04-fuzzy-partition-routing.md`
est donc **à abandonner** : il avait raison sur la direction (en relax la bonne métrique
est l'espace strippé) et tort sur le moyen (supprimer une source de rappel au lieu de
normaliser la métrique).

`[rapporté]` Recette de calcul, sans nouvel index :
- hit 0x02 → `stripped_offset(word_start_pos) + sti`
- hit chunk avec `sti < content_len` → `stripped_offset(chunk_pos) + sti`
- hit chunk avec `sti ≥ own_len` (zone overlap) → `stripped_offset(pos+1) + (sti - own_len)`
- hit chunk avec `content_len ≤ sti < own_len` → le trigramme **commence dans le
  séparateur**, il ne peut jamais matcher une query strippée → **à jeter en relax**

`stripped_offset` se reconstruit par sommes préfixes sur `posmap` + `termtexts`, et
comme une chaîne ne s'étend que sur ~`query_len` octets, seul le **delta** entre deux
hits est nécessaire.

**Comment vérifier.** Implémenter et remesurer A4.

## A6 — Le O(n²) est algorithmique, pas du debug

**Hypothèse.** Les 35 s sur `uint64` / 500 docs viennent de l'asymptotique, pas du
profil debug. Le facteur debug pèse ~10-30×, pas trois ordres de grandeur.

**Ce qui l'étaye.** `[vérifié]` `build_trigram_chains` (`composite.rs:373-392`) : double
boucle `start` / `j` par document. `[rapporté]` Sur `uint64` (n=2, bigrammes `ui`, `in`,
`nt`, `t6`, `64` — `in` et `nt` parmi les plus fréquents d'un corpus C++), un fichier de
quelques Ko donne 10³-10⁴ hits ; `Σ_doc H²` sur 500 docs monte à 10⁹-10¹⁰ itérations.
Et **ça grandit en O(taille_doc²)** — donc intenable en production, pas juste lent en debug.

**Comment vérifier.** Remesurer en profil release avant toute optimisation, pour isoler
le facteur debug. C'est la première chose à faire.

## A7 — Trois gaspillages purs, gains gratuits

`[vérifié]` **Double scan FST par n-gramme.** `resolve_all_trigrams` appelle
`fst_candidates_v3` une fois pour la sélectivité (`composite.rs:301`) puis une seconde
fois à l'identique dans la boucle (`composite.rs:310`). Range-scan + décodage des parents
payés deux fois. Gain attendu : ×2 sur la phase FST.

`[vérifié]` **Doublons chunk/word.** `composite.rs:316-321` empile les hits chunk et word
dans la même `Vec`. Pour tout mot à `sep_len > 0`, le même trigramme au même octet est
émis deux fois. Inoffensif pour la longueur de chaîne, mais **double `n`, donc quadruple
le O(n²)**. Dédupliquer sur `(tri_idx, byte_from)` : gain ×4, gratuit.

`[vérifié]` **Le tri par sélectivité est mort.** Il n'y a plus de `doc_filter` —
`resolve_single_v3(&cands, ctx.resolver, None)` (`composite.rs:311`). L'ordre rarest-first
n'influence plus rien, et `build_trigram_chains` re-trie par `byte_from` de toute façon.
Coût pur.

## A8 — Une DP à balayage unique remplace le greedy

**Hypothèse.** Le problème est un plus long sous-ensemble croissant sous contrainte de
gap. Une DP le résout en `O(H·T)` et elle est *optimale*, donc elle supprime aussi le FN
dû au premier-arrivé.

**Ce qui l'étaye.** `[vérifié]` `composite.rs:388-392` : le premier candidat satisfaisant
la contrainte est engagé, `prev_bf` avancé, sans backtracking. Si un meilleur successeur
existait plus loin, il est perdu.

**Recette.** `[rapporté]` Les hits sont déjà triés par offset. Pour chaque hit, les
prédécesseurs admissibles vivent dans une fenêtre bornée `[h.off - (query_len + d), h.off]` :
pointeur gauche glissant + tableau `best[tri_idx]` indexé par les ≤ `T` trigrammes de la query.

**Attendu.** `uint64` de ~35 s à l'ordre de la centaine de millisecondes en debug, et
fin du comportement quadratique en taille de document.

## A9 — Réintroduire un vrai pré-filtre documentaire

**Hypothèse.** Intersecter les doc-sets des `d+1` n-grammes les plus sélectifs *avant*
de résoudre les autres réduit `H` d'un ordre de grandeur avant même le chaînage.

**Ce qui l'étaye.** `[rapporté]` C'était l'intention originale du `doc_filter` supprimé :
le code de `81dd4a6` faisait `filter.insert(...)`, donc il *grossissait* au lieu de
restreindre — d'où le Finding 5 de `docs/30-mai-2026/02-findings-fuzzy-v3-baseline.md`.
La bonne conclusion était de le *rendre* utile, pas de garder le tri sans le filtre.
`resolve_filtered` est déjà supporté par `PostingResolver`.

## A10 — Ancrer les chaînes sur les n-grammes les plus rares

**Hypothèse.** Ne démarrer les chaînes que depuis les hits du n-gramme le plus sélectif
donne `O(rare_hits · T)`.

**Réserve.** `[rapporté]` Ce n'est correct que si le n-gramme rare survit à l'édition.
Avec `d ≥ 1` il peut être détruit — il faut donc ancrer sur les `d+1` plus rares et unir.
À combiner avec A8, pas à la place.

## A11 — Le BM25 fuzzy n'a plus de composante TF

**Hypothèse.** Émettre toutes les chaînes vérifiées par document, au lieu de la meilleure,
répare la pertinence et les highlights du même coup.

**Ce qui l'étaye.** `[rapporté]` `fuzzy_query_v3.rs:108-113` calcule la fréquence de terme
en comptant les entrées de `highlights` par doc. Une seule chaîne par doc ⇒ `doc_tf ≡ 1`
pour tous les documents.

**Note.** `[rapporté]` Garder une seule chaîne par doc est *correct pour le bitset*
(`filter_by_chain_threshold` ne fait qu'un `insert`) — ce n'est donc pas une cause de
FN/FP de premier ordre, seulement de pertinence.

---

# B. Regex

## B1 — Le regex n'avait jamais été mesuré

**Fait.** `[mesuré]` 2/5 sur le baseline du jour, avec 3 FN. Aucune session après le
17 mai ne valide `regex_v3` sur ground truth : c'est la première mesure de son existence.

| Query | Grep | V3 | FN |
|---|---|---|---|
| `function\s*\(` | 1 | 0 | 1 |
| `uint\d+_t` | 1 | 0 | 1 |
| `std::\w+` | 3 | 3 | 0 |
| `#include\s*[<"]` | 1 | 0 | 1 |
| `Table\w+Function` | 0 | 0 | 0 |

## B2 — Les FN regex pourraient être le bug de classes de caractères de mai

**Hypothèse.** `docs/9-mai-2026-11h14/08-rapport-final-session-9-mai.md` signalait
« bug regex character classes — `program[a-z]+` → 0 résultats, `program\w+` idem, alors
que `program.+` fonctionne », annoncé comme premier point de la session suivante et
jamais repris. Deux des trois FN du jour portent des classes de caractères
(`#include\s*[<"]`, `uint\d+_t`).

**Réserve forte.** L'échantillon est de **1 document par query** sur 500 — c'est
anecdotique. Et `std::\w+` passe, ce qui affaiblit l'hypothèse « toute classe échoue ».

**Comment vérifier.** Rejouer les cas exacts du doc de mai (`program[a-z]+`,
`program\w+`, `program.+`) sur le corpus courant, puis élargir le baseline regex à des
queries à plusieurs dizaines de hits pour sortir de l'anecdote.

## B3 — Le regex est la vraie cible du sidecar byte-n-gram

**Hypothèse.** Un index de trigrammes d'octets bruts (design csearch / Zoekt) sert
correctement la recherche littérale de ponctuation pure et l'extraction de littéraux
regex, là où le SFX est structurellement mal placé.

**Ce qui l'étaye.** `[mesuré]` Les queries à séparateurs passent déjà : `uint64_t`,
`std::unique_ptr`, `ku_dynamic_cast` sont à 15/15 en strict **et** en relax. Le trou
n'est donc pas « les séparateurs » en général, mais le sous-cas plus étroit de la
**ponctuation pure sans ancre token sélective** (`->`, `};`, `);`) — et c'est exactement
la famille des 3 FN regex.

**Réserve décisive.** `[vérifié]` C'est un chantier **additif**. Il ne permet ni de
supprimer la partition 0x02, ni de supprimer le conditionnel `strict_separators` :
0x02 sert le matching *agnostique* aux séparateurs (`mutexlock` → `mutex_lock`,
`TableFunction` → `table function`), qu'un index d'octets bruts ne peut pas faire par
construction — les octets diffèrent. `[mesuré]` Et 0x02 est aujourd'hui à zéro erreur.
Toute présentation du sidecar comme un « déblocage de v3 » ou une simplification est
donc à écarter : à cadrer sur le regex, en parallèle, sans toucher au SFX.

---

# C. Merge

## C1 — Persister plutôt que reconstruire

**Hypothèse.** Des quatre options de merge, celle qui tient est de **persister
davantage** pour que le merge devienne un remap pur, sans jamais recalculer une identité
de mot.

**Ce qui l'étaye.** `[rapporté]` Ce qui manque est identifiable précisément :
1. un octet de partition dans `TTX3` — **la place est déjà libre** (champ `reserved`),
   coût 0 octet ;
2. le `content_overlap` par entrée 0x02 — ~2 octets/mot ;
3. la composition des mots (`word → [ordinaux chunk ordonnés]`) — `.chunk_word_map`
   couvre déjà partiellement.

Avec ces trois, chaque fichier devient un remap sans jugement. L'alternative
« reconstruire depuis les artefacts » produirait une **seconde implémentation** des
invariants du collector, non couplée par le type — précisément le mécanisme de régression
que le projet documente depuis mai.

**Coût du report.** Les ajouter aujourd'hui pendant que le format bouge est gratuit ;
les ajouter plus tard impose une réindexation à tout utilisateur v3.

## C2 — La ré-indexation depuis le docstore comme repli

**Hypothèse.** ~150 lignes, un seul chemin de code, invariants respectés par construction
puisque le collector reste l'unique source de vérité.

**Ce qui l'étaye.** `[rapporté]` Le chemin pre-tokenized alimente déjà v3 avec
`c.add_value(&pre_tok.text)` (`segment_writer.rs:349`) : le texte brut est le seul intrant
de v3, donc la ré-indexation est **exactement reproductible**.

**Ce qu'il faut mesurer avant de s'engager.** Le collector garde tout en RAM
(`token_intern` + `token_postings` + `word_postings`). En WASM, `WRITER_HEAP_SIZE = 15 MB` :
re-collecter un gros segment mergé est un risque de heap direct. Deux autres coûts :
dépendance au champ `STORED`, et perte du fast-path `store_writer.stack(store_reader)`.

## C3 — Le `content_overlap` n'est dérivable qu'en sur-ensemble

**Hypothèse.** À partir de `.next_word_map` on peut produire une entrée 0x02 par paire
`(mot, successeur observé dans le segment)`. Sans FN, mais avec des FP.

**Ce qui l'étaye.** `[rapporté]` `content_overlap` est « les 2 premiers octets de contenu
du mot **suivant dans le texte source** ». La seule information persistée qui encode la
succession est `.next_word_map`, qui donne les successeurs *possibles dans le segment*,
pas le successeur *de l'occurrence*. Une reconstruction exacte exige soit de re-tokeniser,
soit de persister le champ.

**Conséquence.** C'est l'argument le plus fort en faveur de C1 sur toute reconstruction :
le sur-ensemble rouvre une classe de FP dans la partition qu'on vient de rendre exacte.

---

# D. Méthodologie de mesure

## D1 — Le corpus doit être épinglé

**Constat.** `[mesuré]` Le corpus est un `clone --depth=1` du `rag3db` courant, qui a
évolué depuis mai. Les comptes grep ont beaucoup bougé : `functin` 62 → 5, `uint64`
243 → 34, `retrun` 1 → 22.

**Conséquence.** Aucune comparaison mai ↔ août n'est à périmètre constant. Dire
« 0/6 → 2/6 » serait une erreur de méthode : ce ne sont pas les mêmes mesures.

**Direction.** Épingler un commit de `rag3db` dans le harnais de test, et versionner le
cache de ground truth avec ce SHA. Sans ça, aucune série temporelle n'est interprétable.

## D2 — La baseline 0/6 ne décrivait pas le code qu'on croyait

**Constat.** `[rapporté]` `build_trigram_chains` n'existait pas quand la baseline 0/6 a
été mesurée : les briques `resolve_all_trigrams` / `build_trigram_chains` /
`filter_by_chain_threshold` sont **ajoutées** par le WIP `af2db07`. Les six chiffres de
mai décrivent le prédécesseur (seuil `.max(1)`, fenêtre glissante, partition 0x02 jetée).

**Direction.** Toute mesure doit être horodatée **par commit**, pas par date de session.

## D3 — Les docs de mai se contredisent, seule la mesure tranche

**Constat.** Le rapport de session 7 annonce 13/15, le findings du 30 mai décrit 2 fails
restants, et le doc fuzzy du 1er juin affirme au passage « `contains_v3` validée à 15/15 ».
Les trois ne peuvent pas être vrais ensemble. `[mesuré]` La mesure tranche : **15/15**.

**Direction.** Ne plus jamais planifier sur un score cité dans un doc. Remesurer d'abord.
C'est la leçon la plus rentable de cette session : trois mois de raisonnement ont porté
sur un score périmé.

## D4 — L'instrumentation ne décrit plus la production

**Constat.** `[rapporté]` `resolve_trigrams_v3_explained` (`composite.rs:531-647`) n'a
**aucun appelant** et utilise encore la fenêtre glissante `max_window`, pas les briques.

**Direction.** Le migrer ou le supprimer **avant** toute nouvelle campagne de mesure —
sinon on diagnostiquera un pipeline qui n'existe plus.

---

# E. Calibrations jamais faites

## E1 — `MAX_TOKEN = 8` n'a jamais été mesuré

`docs/24-mai-2026-15h04/avis_claude.md` pose l'invariant explicitement : sans les trois
courbes, « le 8 est une superstition héritée de la v2 ». Les courbes à sortir :
1. distribution des longueurs de tokens sur corpus réel,
2. nombre de sauts siblings par query,
3. temps par saut en WASM.

## E2 — Aucun budget temps par phase

Tout le tuning v3 a été fait en natif. Le diag de budget (descente FST / intersection /
sauts siblings / collecte), compilé en WASM dès le départ, n'a jamais été construit —
alors que WASM est la cible qui contraint le design.

## E3 — La régression d'indexation n'a jamais été traitée

68 s → 103 s sur 500 docs à l'introduction de la sibling table (24 mai), assumée sur le
moment, jamais optimisée. Piste notée à l'époque : dédupliquer les paires siblings en
batch au lieu d'un `Vec::contains`.

## E4 — Perf à remesurer en release

`[mesuré]` En profil debug : `include` strict 14,5 s, `uint64_t` relax 17,4 s, regex
`std::\w+` 18,4 s. Aucune conclusion perf n'est tirable avant une mesure en release.

---

# F. Dette d'outillage et de code mort

## F1 — Code mort annoncé, jamais retiré

`docs/24-mai-2026-15h04/08-recap-session-6.md` §5 annonce le nettoyage de
`cross_chunk_chain_v3`, `cross_word_chain_v3`, `build_chains_from_splits` et
`best_consumed` « une fois la sibling table confirmée stable ». Les quatre sont toujours là.

## F2 — `best_consumed` : question ouverte depuis le 19 mai

`docs/19-mai-2026/05-recap-session-5-complete.md` §5 demande si le filtre fait perdre des
vrais positifs intra-partition. Question jamais tranchée, filtre toujours actif.

## F3 — Les collisions multi-parent n'ont jamais été éliminées

Le diag builder mesurait jusqu'à 11 157 parents sous une seule clé FST, et 32 251 clés
multi-parent sur 183 k. Contournées par `best_consumed`, jamais traitées à la racine.

## F4 — Les tests rouges sont des fixtures périmées, pas des bugs

**Hypothèse.** `[mesuré]` 3 tests rouges : `test_into_data_sorted` (casse connue et
documentée du WIP, `tokens` passé de `BTreeSet` à `Vec`), plus `diag_false_positive_uint64t`
et `test_resolve_chain_sep_skip`. `[vérifié]` Ces deux derniers sont documentés rouges
depuis le **19 mai** (`docs/19-mai-2026/05-recap-session-5-complete.md:46-48`), avec leur
cause : ils appellent le pipeline avec `None` pour les maps, ou `resolve_chains_v3` en
direct sans le pipeline word. Ce sont donc des fixtures qui ne reflètent plus le modèle,
pas des bugs de code — ce que corrobore le 15/15 sur corpus réel.

**Note.** Le troisième de la liste de mai, `fz10_long_cross_token_d1_strict_false`, passe
désormais.

**Comment vérifier.** Les migrer vers de vraies maps et constater qu'ils passent.

---

# G. Environnement

## G1 — `bindings/python` ne compile pas sur cette machine

`[mesuré]` PyO3 0.24.2 plafonne à Python 3.13, la machine a 3.14. Deux sorties : bumper
PyO3, ou `PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1`. Sans effet sur v3, mais bloque tout
`--workspace` non filtré.

## G2 — Un bench `bitpacker` exige nightly

`[mesuré]` `#![feature]` sur le canal stable. À exclure ou à mettre derrière une feature.

## G3 — 177 warnings sur la lib `ld-lucivy`

`[mesuré]` À trier — le projet avait atteint clippy zéro en mai, donc c'est de la dérive
récente.

---

# H. Chantiers annoncés, jamais entamés

Repris de la synthèse rétro-chronologique des 20 dernières sessions. Ce sont des
directions décidées puis abandonnées sans décision explicite — à re-trancher, pas à
exécuter par défaut.

| Chantier | Origine | État |
|---|---|---|
| Term dict `.terms` (FST de mots entiers → postings, fast path `term`/`prefix`/`range`) | roadmap v3, `16-mai/08-roadmap-v3-implementation.md` §2.4, annoncé « ajout confirmé » | aucun fichier |
| Sep dict `.seps` (FST de séparateurs pour le lookup regex) | idem §2.5 | aucun fichier |
| Table `next_word` sérialisée en section 0x04 du `.sfx` | idem §2.6 | section déclarée dans l'en-tête, jamais écrite |
| Auto doc_id (allocateur BTree de ranges libres) | `10-mai/05-design-auto-doc-id.md`, jalon v2.1 | design complet, zéro ligne |
| Distribué « 1 shard par machine » + LUCIDS sur HTTP/gRPC | `10-mai/06-roadmap-post-v2.md`, jalon v3 | non entamé (`ExportableStats` existe) |
| Normalisation agentique | idem, jalon v3.1 | non entamé |
| Regex multi-mot (l'espace déclenche un split) | `25-avril/01-known-issues-v2.md` §2 | aucun suivi |
| Multi-token d=0 sur `.luce` | `29-mars/07-rapport` | traité partiellement, jamais reconfirmé |
| Playground : exposer `anchor_start` / `exact_match` / `distance` | `9-mai/08-rapport-final` §4 | non fait |
| Merger `feature/sibling-table-v3` dans la branche v3 | `24-mai/08-recap-session-6.md` §2 | la branche courante n'a jamais été mergée |

---

# I. Ce qu'il ne faut pas retenter

Chaque ligne a déjà été payée. Source : `docs/24-mai-2026-15h04/02-techniques-essayees-et-pistes.md`
et la synthèse rétro-chronologique.

**Tokenisation.** Chain flexible (same-position matching) ; `use_si0 = false` pour tous
les tokens (24 s sur 5 k docs) ; réglages de `MIN_CHUNK_CHARS` / backward merge (chaque
réglage fixe un cas et en casse un autre).

**Cross-token.** Graphe multi-split par worklist (8,8 s, OOM en WASM) ; graphe dédupliqué
par remainders (~500 ms WASM) ; DP avec `ord_to_term` (trop de walks FST).

**Perf fuzzy.** Cache `ord→text` dans le DFA walk (*plus lent* — `TermTextsReader` est déjà
O(1)) ; groupement des candidats par doc (50 ms → 10 000 ms ; piste de rattrapage jamais
explorée : grouper par *région*).

**Monotonie fuzzy.** Ngram checkpoint + sibling adjacency (17 s) ; sibling filter
post-intersect (soit trop strict soit trop laxe) ; position filter seul ;
`cross_token_falling_walk_any_gap` (déclaré inutilisable).

**SFX v3 (liste canonique A→P).** Content-prefix ordinals (crée des FP d'overlap mixing) ;
forking des chains par `consumed` (explosion exponentielle, une variante a fait crasher
la machine) ; exiger `overlap_consumed > 0` dans le falling walk ; supprimer le falling
walk de 0x02 (casse tout le relaxed) ; adjacency stricte `pos+1` (casse `mutex____lock`) ;
prefix markers dans 0x02 ; `ChunkWordMap`/`NextWordMap` global (0 rejets) ; `WordPosMap`
per-doc (ne filtre rien) ; vérification par `TermTexts` (casse `std::unique_ptr`) ;
`MAX_CHAIN_VARIANTS` hardcodé (qualifié de scotch dans les docs de l'époque).

**Session 6.** Content-only keys dans le FST (le DFS look-ahead explose) ; DFS look-ahead
dans le falling walk (99 % CPU pendant 6+ minutes sur 500 docs) ; split table `.wordsplits`
(implémentée puis retirée au profit de la sibling table).

**Deux revirements coûteux à ne pas rejouer.** La sibling table supprimée le 16 mai puis
réintroduite le 24 mai — le falling walk dépend de la finalité des nœuds FST, qui se perd
quand l'index grandit, d'où des FN *scale-dependent*. Et le fuzzy réécrit intégralement le
11 avril puis abandonné à la refonte v3.

**Un choix de sémantique déjà acté.** 6 FN sont acceptés *by design* : le grep relaxed
concatène tout le fichier en une chaîne et matche des mots sans rapport, alors que v3
matche par mots adjacents. La ground truth a été ajustée pour refléter la sémantique v3.

---

# Ordre recommandé

1. **Garde de merge** — le seul point urgent, voir `02-verites-dichotomiques.md` §1.
2. **Remesure en release** (E4) et épinglage du corpus (D1) — sans ça, aucune décision
   perf n'est fondée.
3. **Fuzzy** : A2 (vérification) puis A4/A5 (normalisation), puis A3 (baisser le seuil),
   puis A7/A8 (perf). Dans cet ordre : la vérification rend tous les autres réglages sûrs.
4. **F3 + `exact_match`** en un seul commit, avec test négatif (voir `02` §2).
5. **Regex** : B2 d'abord (rejouer les cas de mai), puis élargir le baseline avant toute
   conclusion.
6. **Format de merge** (C1) tant que le format bouge encore.
