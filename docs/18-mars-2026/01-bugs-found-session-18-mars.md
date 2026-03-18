# Bugs trouvés — Session 18 mars

## Bug 1 : ShardedHandle search retourne 0 hits quand c'est le premier index

### Symptôme
- `BENCH_MODE=RR` seul → 0 hits sur toutes les queries contains/startsWith
- `BENCH_MODE=TA` seul → 20 hits (fonctionne)
- `BENCH_MODE=TA|RR` ensemble → les deux retournent 20 hits
- `BENCH_MODE=SINGLE` seul → 20 hits (LucivyHandle directe)

### Ce qui a été vérifié
- Le commit fonctionne : `reader.reload()` après commit, 25 docs visibles par shard
- Le search voit les docs : `searcher.num_docs()` = 25 par shard au search time
- Les queries sont identiques (même SuffixContainsQuery)
- Pas de crash, juste 0 résultats

### Hypothèse
Race condition dans l'initialisation du scheduler global. Quand RR est le premier
à démarrer le scheduler, quelque chose n'est pas prêt. Quand TA démarre d'abord,
les threads du scheduler sont "warm" et tout marche.

Possibles causes :
- Les threads du scheduler pas encore prêts quand les premiers messages arrivent
- Le `wait_cooperative` dans le commit ne traite pas correctement tous les actors
  sur un scheduler frais (actors pas encore dans la ready queue)
- Race entre le spawn des actors et le premier message envoyé

### À investiguer
- Ajouter un debug dans `execute_weight_on_shard` pour voir si le scorer retourne
  des résultats ou 0
- Vérifier si le `arc_swap` du reader.searcher est thread-safe entre le reload
  (thread main) et le shard actor (thread scheduler)
- Tester avec un `std::thread::sleep(100ms)` après le spawn pour voir si c'est
  du timing

## Bug 2 : blob_store.rs manquant sur feature/sfx-unified

### Cause
`lucivy_core/src/lib.rs` référence `pub mod blob_store;` mais le fichier n'avait
jamais été commité (il était untracked). Le `git add -A` dans la branche experiment
l'a capturé mais le checkout vers main l'a perdu.

### Fix
Récupéré depuis la branche experiment : `git show experiment/decouple-sfx:lucivy_core/src/blob_store.rs`
Commité sur main : `00e6b9e`

## État des branches

### feature/sfx-unified (main)
- 33 commits d'avance sur origin
- Code stable, 1318 tests green (sauf le bug search 0 hits)
- Changements non commités : reader.reload() dans commit() + debug eprintln

### experiment/decouple-sfx
- Branche expérimentale pour le deferred sfx rebuild
- Commit WIP : `cabf92b`
- merge_sfx_deferred (gapmap+sfxpost copiés, FST skippé)
- rebuild_deferred_sfx au commit
- load_sfx_files skip les FST vides
- Queries tolérantes (EmptyScorer)
- max_docs_before_merge = 50K
- NE FONCTIONNE PAS : les term queries dépendent du sfx FST

## Prochaines étapes

1. **Fixer le bug search 0 hits** — prioritaire, bloque tout le reste
2. **Revenir au plan simple** : max_docs_before_merge + merge_sfx normal (pas deferred)
3. **Si le deferred est nécessaire** : découpler le sfx FST des term queries d'abord
