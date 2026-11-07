# Un passage exact d'un document n'est plus trouvé au-delà de 48 à 64 octets

*Ouvert le 7 novembre 2026 par la session lucivy, trouvé en mesurant le ticket du flou sans borne.
Antérieur à ce chantier : présent sur `main` (`fd3fe74`, 4.3.1).*

## Ce qui se passe

Un `contains` dont la valeur est un passage copié d'un document ne le trouve plus au-delà d'une
certaine longueur, en séparateurs relâchés comme stricts, sur un shard comme sur quatre. Un
utilisateur qui colle une phrase ou une ligne de code entière ne retrouve pas son document.

## Reproduire

`lucivy_core/tests/test_fuzzy_many_terms.rs`, test `probe_exact_passage_lengths` (branche
`4.3.1.1-flou-borne`) : deux documents (pages `docs/13-09-2026/07` et `09`), un passage du premier
qui commence à « Le profileur du pauvre** qui a servi : `benches/gdb_sample.sh` lance le binaire⏎␣␣sous gdb… »,
cherché exact à des longueurs croissantes :

| octets | 4 | 8 | 16 | 24 | 32 | 48 | 64 | 96 | 128 | 200 | 300 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| relâché | oui | oui | oui | oui | oui | oui | **non** | non | non | non | non |
| strict | oui | oui | oui | oui | oui | **non** | non | non | non | non | non |

Mêmes réponses à 1 et à 4 shards.

## Pistes (non vérifiées)

Le passage traverse du gras Markdown (`**`), des accents graves, une barre oblique, un `_`, puis un
retour à la ligne suivi d'une indentation. Le premier échec en strict (48 octets) tombe juste après
« `benches/ » ; en relâché (64), après « gdb_sample.sh` lance le ». À départager : la longueur
seule (une chaîne de chunks au-delà d'un nombre de jetons), ou une séquence de séparateurs
particulière (deux séparateurs de suite, l'accent grave, le saut de ligne).

## Ce que ça touche

- Tout `contains` d'un texte long : la requête entière de rag3weaver (la démo aussi) ne retrouve
  pas un passage collé.
- Le flou : une partition en pièces longues ne trouve pas ses pièces. C'est pourquoi, dans le
  correctif du flou sans borne, la distance 1 garde la coupe exhaustive (une pièce courte) ; à
  distance 2 et plus, les coupes fenêtrées d'une requête de plus de 64 octets donnent des pièces
  longues et peuvent manquer des occurrences tant que ce défaut est ouvert.

## Pour le fermer

Un témoin vert : le même passage trouvé à toutes les longueurs, exact et à distance 1, puis la
vérité terrain sur le noyau avec des aiguilles de plusieurs jetons (`test_sfx_v3_ground_truth`).
