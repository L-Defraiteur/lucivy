# Le fuzzy relâché perdait des documents — enquête, cause, correctif

Nuit du 27 au 28 août 2026. Trouvé en voulant produire des chiffres publiables
pour la page de présentation. Corrigé en 3.0.7.

## Ce qui s'est passé, dans l'ordre

L'objectif était un bench « impressionnant » sur les 90 000 fichiers du noyau
Linux. Le bench existant, `bench_sharding`, a rendu de très bons temps.

Une remarque de Lucie a tout changé : **il ne compare rien**. Ses onze lignes
annoncent toutes « 20 hits », parce que 20 est le plafond de résultats. Il
chronométrait une réponse que personne n'avait vérifiée.

D'où un second panel, `v3_ground_truth_demo`, qui réutilise le harnais de
vérité terrain : chaque ligne compare les documents **et les spans d'octets**
du moteur à une lecture naïve des fichiers sur disque. Au premier passage sur
le corpus complet, une ligne a échoué.

## Le symptôme

```
schdule   fz1   5206  5206  FAIL  spans gt=18843 v3=18839 miss=4 extra=0
    missing  doc=52034 [24754..24768] "u_to_le16(duration_m>>s);\n\tcmd->u.le<<d_action_req…"
```

Quatre spans manquants sur 18 843, tous dans `kvaser_usb_leaf.c`. L'occurrence
manquée est `s);\n\tcmd->u.le`, qui aplatie donne `scmdule` — à une
substitution de `schdule`, et **à cheval sur quatre tokens**.

## L'enquête, et deux hypothèses fausses

| étape | résultat |
|---|---|
| Le fichier seul | 5 spans, **exacts**. Donc pas un bug par document. |
| `drivers/net` (6 661 fichiers) | mêmes 4 manquants — reproduction en 10 s |
| `drivers/net/can/usb` (**31 fichiers**) | mêmes 4 manquants, en 0,7 ms |
| Plafonds `LUCIVY_MAX_MATCHES_PER_SEGMENT=0` et `LUCIVY_HIGHLIGHT_SPAN_CAP=0` | **identique** — ce n'est pas un plafond |
| `V3_DIAG_FUZZY_MAX=0`, recherche de l'occurrence parmi les 166 rejets | **zéro** — le candidat n'est jamais produit |

Deux hypothèses ont été écartées par la mesure, et il faut le noter parce que
les deux étaient plausibles :

1. **« C'est le plafond de matches »** — testé en le désactivant : résultat
   identique. Neuf spans attendus, un plafond à quatre millions.
2. **« C'est la frontière de token, seule »** — un fichier de deux lignes
   contenant la même occurrence à cheval **passait**. L'explication était donc
   incomplète : c'est la conjonction de la frontière *et* du générateur choisi.

## La cause

La génération de candidats fuzzy a deux implémentations, et `auto` choisit
d'après une estimation de coût (`composite.rs`, commit `9866bc1` du 23 août) :

```
[fz] auto: pivot (pieces cost 91 x2 > pivot 135)
[fz] gram "sch" … "chd" … "hdu" … "dul"        ← "ule" jamais sondé
[fz] cand=171 kept=4 rejected=166 spans=5
```

`pivot` tire ses candidats des **postings de trigrammes**, qui n'existent qu'à
l'intérieur des chunks d'un token. Or `scmdule` ne partage avec `schdule` que
`dul` et `ule`, et dans la source **les deux enjambent des séparateurs** : le
`d` vient de `cmd`, le `u` de `u`, le `l` de `le`. Aucun posting, donc aucun
candidat. Garder davantage de trigrammes n'y changerait rien : ceux qu'on
garderait sont à cheval aussi.

La dichotomie, vérifiée par construction :

| occurrence | `pieces` | `pivot` |
|---|---|---|
| dans un seul token | OK | OK |
| à cheval sur des tokens | OK | **FAIL — 0 document** |

**Ce n'est donc pas un défaut de surlignage.** Un document dont l'unique
occurrence approchée est à cheval n'est pas rendu du tout. Et comme
l'estimation bascule vers `pivot` quand l'index grossit, la perte n'apparaît
qu'à l'échelle — invisible sur les petits corpus de test.

Versions concernées : **3.0.2 à 3.0.6**.

## Le correctif

Pas une détection de risque à l'exécution — elle devrait deviner l'existence de
ce qu'elle ne peut pas voir. La condition est **connue d'avance** :

```
séparateurs relâchés  →  pivot exclu   (les occurrences à cheval sont dans le périmètre)
séparateurs stricts   →  pivot permis  (l'occurrence tient dans un token par définition)
```

En pratique, dans la branche `auto` : le budget de coût n'est passé à
`resolve_pieces` que si `strict_separators`. Sans budget, `pieces` n'est jamais
écarté pour une raison de vitesse.

Le fuzzy étant toujours relâché (la requête est aplatie avant comparaison),
cela revient à dire que `pivot` ne sert plus au fuzzy — il reste légitime pour
les chemins stricts.

## Ce que ça coûte : rien

93 605 fichiers, machine au repos, trois passes :

| requête | `auto` avant (→ pivot) | `auto` après (→ pieces) |
|---|---|---|
| `schdule` d=1 | 238,1 ms — **4 spans perdus** | **223,8 ms** — 18 843 exacts |
| `regsiter` d=2 | 990,0 ms | **878,9 ms** |

L'estimation choisissait le générateur incomplet **et** le plus lent.

## Vérification

- Panel complet, 93 605 fichiers, mode `auto` : **9 vérifiées, 0 échec**
- `cargo test --lib` : 1 435 passés
- Suite vérité terrain : 10 passés
- `fuzzy_finds_an_occurrence_that_straddles_tokens` : passe en 0,03 s, et
  **échoue** sous `V3_FUZZY_MODE=pivot` avec « 0 documents returned »

## Ce qu'on en retient

**Un bench sans vérité terrain ne mesure rien d'utile.** Les temps de
`bench_sharding` étaient corrects depuis le début ; ils chronométraient une
réponse fausse. Le défaut a vécu cinq versions publiées derrière des chiffres
qui avaient l'air bons.

Et le corollaire, déjà écrit le 27 pour les benchs sparse : une explication
convaincante ne vaut que si elle survit à une nouvelle mesure. Les deux
hypothèses écartées ci-dessus étaient plausibles ; seule la mesure les a
départagées.

## Reste ouvert

- **Jaro-Winkler n'a pas de vérité terrain.** Sur le chemin Levenshtein le
  moteur rend *tous* les spans d'une fenêtre candidate, donc un balayage naïf
  est comparable span par span ; sur le chemin Jaro-Winkler il n'en rend
  **qu'un**, le meilleur de la fenêtre (`best_window`), donc ce qu'il rapporte
  dépend du découpage en fenêtres, qui est un artefact de l'index. Une règle
  comparable existe — regrouper les sous-chaînes au-dessus du seuil par
  chevauchement et garder la meilleure de chaque groupe — mais c'est une
  seconde définition à défendre. En attendant, ces lignes affichent `n/a` et
  `NOT VERIFIED`.
- **Le mode fuzzy n'est pas exposé publiquement.** `V3_FUZZY_MODE` est une
  variable d'environnement de diagnostic. Si un utilisateur veut un jour
  échanger du rappel contre de la vitesse, il faudra une option nommée par ce
  qu'elle fait, avec le défaut du côté de la justesse.
- **L'environnement `release` n'a aucun réviseur requis** — seulement une
  `branch_policy`. Le « clic d'approbation » que `release.yml` décrit en
  commentaire n'existe pas en pratique : les publications partent seules dès
  qu'un tag correspond. À réarmer si on veut vraiment le verrou.
