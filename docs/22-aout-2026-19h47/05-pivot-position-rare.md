# Pivot sur la position la plus rare — note pour la prochaine session

État : **non implémenté**. Conclusions tirées des mesures du 23 août, à reprendre
après le merge parallèle.

## Le constat qui motive l'idée

`__init` strict, 50 000 fichiers kernel, 800 segments, après les correctifs de
`5425490` (résolution par posmap, listes partagées) :

- 976 ms, contre ~200-270 ms pour `spin_lock`, `net_device`, `mutex_unlock`
- 3 443 957 chaînes construites
- 27 845 659 lookups posmap pour 9 013 061 survivants, 124 678 matchs émis

Le coût restant n'est plus une redondance ni une copie : c'est la **forme de la
requête**. Elle commence par `__`, donc le walk FST ancre une chaîne sur chaque
token qui finit par un souligné — tout le vocabulaire du kernel — puis chacune
cherche `init` à la position suivante.

## Pourquoi c'est à l'envers

Une chaîne se résout toujours **depuis la position 0**. Pour `__init` :

| position | ordinaux candidats | postings |
|---|---|---|
| 0 (`…_`, `…__`) | ~3,4 millions de parents distincts | énormes |
| 1 (`init…`) | une poignée | bien moins |

On part du côté large pour arriver au côté étroit. Or `posmap` est
**bidirectionnel gratuitement** : `ordinal_at(doc, pos - 1)` coûte exactement
`ordinal_at(doc, pos + 1)` — trois lectures mémoire.

## L'idée

1. Pour une chaîne (ou un groupe de chaînes partageant leur queue), estimer la
   sélectivité de chaque position : `selectivity_v3` existe déjà dans
   `resolve.rs` (`doc_freq` par ordinal).
2. Résoudre les postings de la position **la plus rare** seulement.
3. Depuis chacun, marcher vers l'avant ET vers l'arrière avec `posmap`
   (`word_pos_map` pour le pipeline word), en vérifiant à chaque pas que
   l'ordinal trouvé appartient à l'ensemble admis à cette position.
4. L'ensemble admis à la position 0 peut être grand (3,4 M) : un **bitset sur
   `num_terms` bits par segment** suffit — quelques dizaines de Ko — plutôt
   qu'une recherche dichotomique dans une liste.

Coût attendu : |postings de `init`| lookups au lieu de 27,8 M. `__init` devrait
rejoindre les ~200 ms des autres requêtes.

## Ce qui existe déjà et qu'il faut réutiliser

- `find_multi_token_v3` (`composite.rs`) fait déjà « pivot = position la plus
  sélective » pour le multi-token. La résolution de chaînes ne le fait pas.
- `AdjacencyMode::StrictPosmap` (`resolve.rs`) est la marche avant. La marche
  arrière est symétrique.
- `resolve_word_chains_v3_wordmap` (`resolve.rs`) est l'équivalent word, via
  `word_pos_map` (format `WMP2`, ordinal | span << 24).
- Les compteurs `n_posmap_*` / `n_wordmap_*` dans `briques/profile.rs` disent
  immédiatement si ça marche : lookups, survivants, et surtout **mismatches,
  qui doivent rester à 0**.

## Pièges

- **Ne pas supposer que la position 0 est la plus large.** Pour `spin_lock`
  c'est l'inverse. La sélectivité doit être mesurée par chaîne, pas déduite de
  la forme de la requête.
- **Les chaînes ne sont pas dupliquées** (3 443 957 brutes → 3 431 334
  distinctes). Les dédupliquer ne gagnerait rien ; je l'ai mesuré.
- **Regrouper les chaînes par queue commune** est l'autre moitié de l'idée :
  3,4 M de chaînes `[X_, init]` devraient devenir un seul problème
  « `init` précédé d'un X admis », pas 3,4 M problèmes indépendants. Sans ce
  regroupement, le pivot refait |postings de init| lookups **par chaîne**.
- Le compteur de mismatch est la seule preuve d'exactitude. S'il bouge, c'est
  faux, quel que soit le gain.

## Ce qui reste de coût ailleurs, mesuré, non corrigé

- `resolve_doc` → `read_ordinal_header` alloue trois `Vec` de la taille du
  nombre de docs de l'ordinal, par appel. Appelé une fois par match émis.
- L'émission fait `first_entries.iter().find(doc_id)` — linéaire — par match.
- Hygiène, pas structure. À faire après, si les chiffres le justifient.
