# Le flou à distance 1 perd des documents trouvés à distance 0, selon la forme des segments

*Ouvert le 13 novembre 2026 par la session lucivy, trouvé en qualifiant la 4.3.4 à luciepc.
Antérieur à ce chantier : présent dans la 4.3.3 publiée (`663975a`).*

**État** : **corrigé** sur la branche `4.3.5-fusion-garde-les-mots-neufs` (pas encore fusionné ni publié).
La cause était une perte de mots du dictionnaire partagé, par une fusion de segments lancée entre deux
commits ; la forme des segments n'y était pour rien.

## Ce qui se passe

Un `contains` à distance 1 doit rendre au moins les documents que rend le même `contains` à
distance 0. Sur le clone de rag3db indexé par `test_playground_repro`, « rag3weaver » trouve
1 235 documents à distance 0 et **1 229 à distance 1** : six documents manquent, groupés dans un
ou deux segments (`seg=4 doc=7,8,9,10`, `seg=13 doc=7` lors d'une passe).

Le défaut est intermittent : il dépend de la forme des segments, que les fusions de fond laissent
différente d'une passe à l'autre.

## Comment le reproduire

```bash
git clone https://github.com/L-Defraiteur/rag3db /tmp/test_rag3db_clone   # 207d334 le 13 novembre
cargo test -p lucivy-core --test test_playground_repro
```

Le 13 novembre à luciepc, même corpus (`207d334`), passes alternées :

| Source | Passes rouges |
|---|---|
| A, 4.3.3 (`663975a`) | 3 sur 5 |
| 4.3.4 en préparation (`d3a3810`) | 1 sur 6 |

À chaque fois, c'est le même manque : 1 235 documents contre 1 229. Le test s'arrête au premier
terme rouge, donc les autres termes de sa vérification de monotonie n'ont pas été jugés lors de
ces passes.

## La cause (trouvée le 13 novembre au soir)

Une fusion qui démarre entre deux commits faisait passer en « commités » les segments que l'indexation
venait d'écrire. Deux portes y menaient : la fusion lancée par la politique
(`handle_start_merges`, qui recopiait la préparation du commit) et la fusion explicite
(`handle_merge`, qui déroulait tout le DAG du commit, `PrepareNode` compris). Le repli de leurs textes
dans le dictionnaire partagé (`fold_new_texts`) n'a lieu qu'au début d'un vrai commit, et il ne
regarde que les segments **non** commités : le commit suivant les sautait, et
`delete_consumed_pairs` effaçait leur paire de mots comme consommée. Leurs mots n'étaient plus dans
aucune génération du dictionnaire, alors que les segments, puis leur fusion, y renvoyaient toujours.

- La recherche exacte trouve encore ces documents par les mots connus d'ailleurs, et garde un résultat
  dont elle ne peut pas reconstruire le texte (`orchestrator.rs`, « cannot rebuild — keep »).
- Le flou a besoin de ce texte pour vérifier la faute (`rebuild_window_opts`, `PosMapReader::byte_at`) :
  il écarte le document (« NO WINDOW »), quel que soit le mot cherché.

Preuves, sur l'index rouge sauvé à luciepc (`~/.cache/rag3weaver-build/lucivy-flou/index-rouge-60f7063`,
un seul segment de 5 723 documents après les fusions de fond) :
- rouvert, il rend 1 229 fichiers au lieu de 1 235 à chaque fois, et les six mêmes manquent ;
- aucune recherche floue ne trouve `tools/build-images/README.md` ni `build.sh`, pas même « paquet » ou
  « docker », que la recherche exacte y trouve ;
- `V3_DIAG_FUZZY=1` montre ces documents écartés en NO WINDOW ;
- 31 862 des 2 693 974 ids globaux du gmap du segment n'ont ni texte ni méta dans le dictionnaire.

Le témoin `lucivy_core/tests/test_merge_keeps_new_words.rs` reproduit la perte en 0,1 s : deux commits,
un lot écrit mais pas commité, une fusion explicite, un commit. Sans le correctif, même la recherche
exacte ne trouve aucun document du lot, dont les mots n'existent nulle part ; son contrôle sans fusion
est vert.

## Le correctif

- `PrepareNode` ne promeut les segments que dans un commit (`build_commit_dag(…, commit)`).
- Les deux portes d'une fusion préparent leurs segments par `merge_entries_outside_commit` : aucun
  segment ne change de registre, et les suppressions ne s'appliquent qu'aux segments fusionnés.
- En format 4, fusionner des segments pas encore commités est refusé
  (`InvalidArgument` : « commit before merging it »). Un segment fusionné n'a pas de paire de mots à
  lui, donc les mots de ses sources se perdraient au commit suivant. La politique de fusion ne propose
  que des segments commités ; ce cas passait jusqu'ici par la promotion fautive.
- Effet de bord corrigé du même coup : une fusion explicite rendait durables des documents que
  l'utilisateur n'avait pas commités.

**Ce que le correctif ne répare pas** : un index déjà écrit avec ce défaut garde ses mots perdus.
Seul un réindexage le répare.

## Les tests rendus utiles

`test_playground_repro` comparait les deux distances par adresse interne et ne disait rien d'utile en
échec. Il compare désormais par chemin, et dit en échec les comptes, la forme des segments autour de
chaque recherche et le contexte de la requête dans chaque fichier manquant. C'est ce qui a attrapé le
rouge. Même traitement pour les autres tests qui comparent deux recherches : `test_fuzzy_monotonicity`
(chemin, ou première ligne du contenu), `test_highlight_cap` (`_node_id`) et `test_fuzzy_ground_truth`
(le lecteur pris après la recherche).

## Où chercher

Le chemin du flou à distance 1 par segment (`contains` + `distance`, pigeonhole par trigrammes),
pour les documents d'un segment où l'exact les trouve : un candidat écarté avant la vérification,
ou une coupe par segment qui ne voit pas toutes ses occurrences. Il faudra relever d'abord, sur une
passe rouge, les documents manquants (chemins, octets du mot) et la forme du segment (nombre de
documents, fusionné ou non).

## Ce qui reste à décider

Rien n'est décidé. Le défaut ne vient pas du chantier des barrières d'arrêt : sa fréquence sur A
le montre.
