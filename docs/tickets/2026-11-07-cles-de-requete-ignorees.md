# Les clés `boost`, `offset` et `slop` d'une requête sont ignorées en silence

*Ouvert le 7 novembre 2026, à la demande de Lucie. Relevé le 25 octobre sur lucivy 4.3.1
(`docs/25-10-2026/03-options-de-recherche.md`, § 3, branche `docs-pipeline-publication`).*

**État** : **corrigé par un avertissement** dans lucivy 4.3.2 : `QueryConfig` recueille les clés inconnues (`unknown_keys`) et `query_warnings` les nomme, avec ce qu'il faut faire pour `boost`, `offset` et `slop`, et dit `terms` et `max_expansions` acceptés sans effet ; témoin `unknown_query_keys_are_named_by_the_warnings`. **Reste ouvert** : les refuser par une erreur (incompatible, donc une 4.4.0), une décision de Lucie.

## Ce qui se passe

`QueryConfig` (`lucivy_core/src/query.rs:223-270`) se désérialise sans `deny_unknown_fields`
(alors que `SchemaConfig`, `query.rs:28`, le porte). Une clé que lucivy ne connaît pas est jetée
sans rien dire. Un appelant qui écrit `"boost": 2.0`, `"offset": 20` ou `"slop": 1` croit pondérer,
paginer ou rapprocher, et lucivy fait la recherche sans : les résultats sont faux sans erreur.

## Ce qui existe vraiment

- **Pas de pondération** par champ ni par clause : `BoostQuery` existe dans `ld_lucivy`
  (`src/query/mod.rs:49`), mais `query.rs` ne le construit jamais. `boost_factor` n'est lu que par
  `more_like_this` (`query.rs:641`).
- **Pas de pagination** : seulement `top_k` (signatures de `ShardedHandle::search*`,
  `lucivy_core/src/sharded_handle.rs:2136-2184`).
- **Pas de proximité** : `phrase` est un `contains` de toute la valeur (`query.rs:375-380`), et
  `terms` se désérialise mais n'est lu nulle part.
- Deux autres clés acceptées et jamais lues : `max_expansions` (`phrase_prefix`, `query.rs:442-447`)
  et `terms`.

## Reproduire

`{"type":"contains","field":"body","value":"kmalloc","boost":5,"offset":10,"slop":2}` rend
exactement ce que rend la même requête sans ces trois clés.

## Proposition

Deux formes, à choisir :

1. **Refuser.** `#[serde(deny_unknown_fields)]` sur `QueryConfig` et `FilterClause` : une clé
   inconnue devient une erreur qui la nomme. Les clés acceptées mais sans effet (`terms`,
   `max_expansions`) sont refusées aussi, ou retirées de la structure. C'est le plus honnête, mais
   ça casse un appelant qui envoie aujourd'hui une clé en trop. Une version mineure, annoncée au
   CHANGELOG.
2. **Dire.** Garder l'acceptation, mais rendre un avertissement par `query_warnings`
   (`sharded_handle.rs:2158`, qui avertit déjà pour `parse` sans syntaxe). Rien ne casse, mais un
   appelant qui ne lit pas les avertissements reste trompé.

Possible aussi : refuser en général, avec un message qui dit quoi faire pour les trois clés les
plus attendues (« pas de boost : deux recherches et pondérer côté appelant », « pas d'offset :
`top_k = offset + n` et couper », « pas de slop »).

## À décider

La forme (refuser ou dire), et pour quelle version. Implémenter `boost` et `offset` est un autre
chantier : ce ticket ne porte que sur le silence.
