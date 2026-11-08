# `ShardedHandle::close` sort sur l'erreur sans arrêter ses pools

*Ouvert le 8 novembre 2026, à la demande de c7 : lu par la session « recherche » de rag3db en
cartographiant un SIGSEGV à la sortie des e2e de rag3weaver.*

## Ce qui se passe

`close()` (`lucivy_core/src/sharded_handle.rs`) enchaîne trois écritures — le commit de chaque
shard, l'état du routeur (`write_root_file`), la fermeture de chaque shard — et sort par `?` sur la
première qui échoue. L'arrêt des pools (`reader_pool`, l'acteur routeur, `shard_pool`) vient après :
il n'a pas lieu, et le drapeau `closed` n'est pas posé.

Les acteurs restent alors inscrits sur le planificateur global (`GLOBAL_SCHEDULER`, statique) jusqu'à
la fin du processus, chacun tenant un `Arc` de son shard et, par lui, le stockage. Quand le dossier de
l'index a déjà disparu (`save_metas : No such file`), la fermeture échoue et laisse ces acteurs vivants
sur un chemin mort ; à la sortie, le processus meurt (le SIGSEGV de rag3weaver).

Le `Drop` de `ShardedHandle` appelle `close()` ; il en hérite.

## Reproduire

Un index de deux shards, un document ajouté sans commit, le dossier de l'index effacé, puis
`close()` : erreur, et chaque `Arc<LucivyHandle>` de shard est encore tenu par son acteur
(`Arc::strong_count > 1`).

## Ce qu'il faut

`close()` arrête et joint ses pools **quoi qu'il arrive** : chaque étape d'écriture est tentée, la
première erreur est gardée, les pools sont arrêtés, `closed` est posé, puis l'erreur est rendue. Un
handle fermé en échec est inerte comme un handle fermé avec succès.

## Pour le fermer

Rouge d'abord : le témoin ci-dessus, qui vérifie aussi qu'après le `close()` en échec aucun acteur
ne tient plus son shard et que l'erreur nomme ce qui a échoué.
