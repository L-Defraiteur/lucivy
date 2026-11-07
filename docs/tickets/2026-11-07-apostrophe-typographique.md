# L'apostrophe typographique « ’ » casse « Vue d'ensemble »

*Ouvert le 7 novembre 2026, à la demande de Lucie. Relevé le 25 octobre sur lucivy 4.3.1
(`docs/25-10-2026/03-options-de-recherche.md`, § 1 et § 2, branche `docs-pipeline-publication`).*

## Ce qui se passe

Une requête « Vue d'ensemble » (apostrophe droite, U+0027) ne trouve pas un texte « Vue d’ensemble »
(apostrophe typographique, U+2019), ni en exact ni en fuzzy 1 ; et l'inverse. Les Markdown en
français ont souvent `’`, les claviers tapent `'`.

## Reproduire

Un index d'un champ `body`, deux documents : `## Vue d'ensemble` et `## Vue d’ensemble`. La
requête `{"type":"contains","field":"body","value":"Vue d'ensemble"}` rend le premier seulement ;
avec `"distance": 1`, toujours seulement le premier (essayé le 25 octobre, binding Python 4.3.1).

## Cause

En séparateurs relâchés (le défaut), un caractère ASCII non alphanumérique est un séparateur, retiré
de la requête et du texte ; tout caractère non ASCII est du contenu :
`src/tokenizer/equal_chunk.rs:103-105`, `is_content_char(c) = !c.is_ascii() || c.is_ascii_alphanumeric()`.
`'` est donc retiré (« vuedensemble ») et `’` gardé (« vued’ensemble »). Le fuzzy compte en octets
(`src/suffix_fst/briques/fuzzy_spans.rs:10-16`) et `’` en fait trois : distance 3, hors de portée à 1.

## Proposition

Traiter les apostrophes et guillemets typographiques comme leurs équivalents ASCII, des deux côtés :
`’` `‘` `ʼ` → `'`, et vraisemblablement `“` `”` `«` `»` → `"`, et les tirets `–` `—` → `-`. Le plus
sûr : les compter comme séparateurs dans `is_content_char` (une seule fonction, lue à l'indexation et à
la requête). En strict, la requête et le texte devraient être normalisés de la même façon avant la
comparaison.

## À décider

- **La liste exacte** des caractères assimilés (apostrophes seules, ou guillemets et tirets aussi).
- **Le format** : changer `is_content_char` change ce que les index existants contiennent (le texte
  dépouillé indexé). Un index écrit avant ne trouverait plus `’` là où il était du contenu, ou le
  trouverait différemment. Il faut soit une version de format et une reconstruction, soit une
  normalisation de la seule requête qui cherche les deux formes (moins propre, mais sans toucher au
  format). C'est à Lucie de trancher.
- Les spans surlignés doivent continuer de pointer les octets du texte d'origine (`’` inclus).

## Contournement en attendant

Côté appelant (rag3weaver), envoyer les deux formes en `boolean.should` de deux `contains`.
