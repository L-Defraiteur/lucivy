# Un test de corpus qui ne trouve pas son corpus passe vert

*Ouvert le 8 novembre 2026, à la demande de c7, après l'effacement de `~/lucivy_bench` à luciepc.*

## Ce qui se passe

Les tests de vérité terrain et de parité de lucivy lisent un corpus réel (le noyau Linux sous
`/tmp/lucivy-cmp`, ou un arbre donné par `V3_CORPUS`). Quand il manque, ils ne rougissent pas :

| test | sans corpus |
|---|---|
| `test_sfx_v3_ground_truth.rs` (`collect_files`) | imprime « Skipping: corpus not found at … » et rend une liste vide : le test passe sans rien vérifier |
| `test_dictionary_index.rs`, `test_derived_in_ram.rs`, `test_positions_off.rs` (`corpus`) | sous 50 fichiers, prennent un corpus synthétique de onze mots, sans le dire |

Une mesure ou une vérité terrain lancée sans le noyau ressort donc verte sur un synthétique ou sur
rien. Le 8 novembre, `~/lucivy_bench` (le noyau `linux-7.2` vers lequel pointent les liens
`/tmp/lucivy-cmp*`) a été effacé : tout ce qui s'y appuyait est dans ce cas.

C'est contraire à la règle « un test qui se saute n'est pas vert ».

## Ce qu'il faut

1. Un corpus manquant fait rougir le test, en nommant le chemin cherché, le nombre de fichiers
   trouvés, et les commandes qui le recréent :
   - le noyau : `git clone --depth=1 --branch v7.2 https://github.com/torvalds/linux ~/lucivy_bench/linux-7.2`
     (le comparatif est épinglé sur le commit `8d3ae59288f1`), puis
     `ln -sfn ~/lucivy_bench/linux-7.2 /tmp/lucivy-cmp` et `ln -sfn ~/lucivy_bench/linux-7.2 /tmp/lucivy-cmp-90k` ;
   - les corpus de la démo : `python3 playground/tools/build_corpus.py all`.
2. Le corpus synthétique reste possible, mais seulement demandé, par une variable
   (`LUCIVY_SYNTHETIC_CORPUS=1`), et il se dit dans la sortie. La CI la pose pour les trois tests
   qui n'ont pas le noyau ; `ground-truth` a déjà son corpus (`V3_CORPUS: .`).
3. Chaque test imprime le compte de fichiers de son corpus, pour qu'une mesure se lise.

## Pour le fermer

Rouge d'abord : un test qui appelle la fonction de corpus sur un chemin absent, sans la variable, et
attend l'échec ; puis les quatre fichiers passés à la fonction commune, la CI verte avec la variable.

Hors de ce ticket : `test_lock_investigation`, `test_store_fallback` et `test_luce_roundtrip` sautent
aussi faute de leur entrée, mais ce sont des outils d'enquête ou des témoins d'un fichier précis, pas
des vérités terrain.
