# Réduire encore la taille de l'index — pistes (8 septembre 2026)

Question de Lucie : « on aurait quoi comme piste pour encore réduire la taille
des index, genre optionnellement des trucs lourds, avec des branches bien
organisées dans le code de recherche ? »

## De quoi l'index est fait

Index dictionnaire du noyau rebâti le 7 au soir (`/tmp/lucivy-compare/dict`,
253 segments sans fusion, 4 shards) : **5 156 Mo pour 857 Mo de texte**.

| fichiers | Mo | part | rôle |
|---|---|---|---|
| `dict-*.sfx` | 1 013 | 19,7 % | la FST partagée : chaque suffixe de chaque jeton |
| `.sfxpost` | 809 | 15,7 % | positions des suffixes (SI>0) |
| `.word_pos_map` | 669 | 13,0 % | dérivé (rebâti à l'ouverture avec `derived_in_ram`) |
| `.word_sfxpost` | 657 | 12,7 % | positions des débuts de jeton (SI=0 : mot entier, préfixe) |
| `.posmap` | 545 | 10,6 % | dérivé |
| `.sibling_v3` | 453 | 8,8 % | dérivé |
| `store` | 343 | 6,7 % | documents stockés (optionnel par champ) |
| `dict-*.termtexts` | 318 | 6,2 % | textes des jetons |
| `.gmap` | 274 | 5,3 % | ordinaux locaux → ids globaux (fond avec les fusions) |

Les dérivés font 32 % : c'est `derived_in_ram` (noyau 3 335 Mo, ×3,9). Le
reste : la FST 20 %, les positions 28 %, le stockage et les textes 13 %.

## Piste 1 — positions optionnelles, spans par relecture (la plus grosse)

**Quoi.** Un layout « documents seulement » : par jeton, l'ensemble des
documents et la fréquence (BM25), pas les positions. Les spans se calculent en
relisant le texte stocké des candidats — la méthode de tantivy, mais avec un
ensemble de candidats **exact** venu de la FST au lieu d'un AND de trigrammes
trop large (40 741 candidats sur 93 983 pour `#include <linux/` chez lui).

**Ce qui tombe — mesuré le 8 septembre** (`postings_measure.rs`, tally
« documents only » : paires (terme, doc) distinctes, encodées en varint de
delta + tf, sur l'index du noyau) :

| postings | positions (aujourd'hui) | documents seulement | gain |
|---|---|---|---|
| `.sfxpost` | 771 Mo, 167,0 M d'entrées | 168 Mo, 76,0 M de paires (2,20 positions par paire, 2,32 o/paire) | −78 % |
| `.word_sfxpost` | 626 Mo, 136,7 M d'entrées | 128 Mo, 58,3 M de paires (2,35 par paire, 2,29 o/paire) | −80 % |

1 100 Mo de postings en moins, plus les trois dérivés (1 667 Mo) qui n'ont
plus d'objet : **5 156 → ≈ 2 400 Mo, ×2,8 le texte au lieu de ×6**, avant
tout compactage par blocs des ids.

**Ce que ça coûte.** Les comptes restent exacts et rapides (ensembles de
documents). Les spans passent de 15 ms à l'ordre de 100 ms sur `mutex_lock`
(5 145 documents à relire ; tantivy 96 ms). Une requête sans highlights ne
paie rien. `store` devient obligatoire pour le champ.

**La branche.** « D'où viennent les spans » — c'est déjà la question que
`regex_verified` se pose quand le motif n'a pas de littéral (documents
rebâtis et balayés). Un `positions: false` dans le schéma, fixé à la création ;
le lecteur de segment expose « a des positions » ; le collecteur de spans
prend la branche relecture. Le fuzzy et la regex utilisent déjà la vérification
sur le texte. Le harnais vérifie les deux layouts avec le même panel.

## Piste 2 — suffixes aux frontières de sous-mots seulement

**Quoi.** La FST indexe chaque suffixe de chaque jeton : c'est ce qui rend
`pin_loc` (qui commence au milieu de `spin`) et `de` exacts. Une variante
n'indexerait que les suffixes qui commencent à une frontière de sous-mot
(`_`, changement de casse, chiffres) plus tous les suffixes des jetons courts
(≤ 4 ?). Les requêtes qui ne tombent sur aucune clé passent dans la branche
« scan honnête » avec un avertissement dans `query_warnings`, exactement comme
une regex sans littéral aujourd'hui.

**Ce qui tombe.** La FST et `.sfxpost` : la moitié ou plus, à mesurer.

**Ce que ça coûte.** Une sous-chaîne arbitraire au milieu d'un sous-mot devient
un scan, dit. C'est le mode « je cherche du code », et il ne ment pas.

## Piste 3 — petites choses sûres

- `.gmap` 274 Mo sur 253 segments : fond avec les fusions ; compactable en
  bits (les ids globaux sont croissants par blocs).
- `store` : déjà optionnel par champ (`stored: false`) si l'application garde
  ses textes — incompatible avec la piste 1.
- `.termtexts` : remplaçable par une relecture du docstore pour la vérification
  des formes ; délicat (la casse, les formes).

## Ordre et protocole

La 1 d'abord (coupe le plus, branche la plus nette), puis la 2. Chacune :
index de référence 10 000 (`/tmp/lucivy-cmp`), A/B sur 30 000 et le noyau,
panel 10/10 **ou** la liste exacte des requêtes qui passent en scan, temps
des spans mesurés à part. Jamais le défaut : `positions: false` et
`subword_suffixes: true` sont des choix de création, comme `derived_in_ram`.
