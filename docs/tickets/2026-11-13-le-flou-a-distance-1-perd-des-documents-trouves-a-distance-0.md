# Le flou à distance 1 perd des documents trouvés à distance 0, selon la forme des segments

*Ouvert le 13 novembre 2026 par la session lucivy, trouvé en qualifiant la 4.3.4 à luciepc.
Antérieur à ce chantier : présent dans la 4.3.3 publiée (`663975a`).*

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

## Où chercher

Le chemin du flou à distance 1 par segment (`contains` + `distance`, pigeonhole par trigrammes),
pour les documents d'un segment où l'exact les trouve : un candidat écarté avant la vérification,
ou une coupe par segment qui ne voit pas toutes ses occurrences. Il faudra relever d'abord, sur une
passe rouge, les documents manquants (chemins, octets du mot) et la forme du segment (nombre de
documents, fusionné ou non).

## Ce qui reste à décider

Rien n'est décidé. Le défaut ne vient pas du chantier des barrières d'arrêt : sa fréquence sur A
le montre.
