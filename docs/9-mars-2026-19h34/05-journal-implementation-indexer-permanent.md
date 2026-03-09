# Journal d'implémentation : Indexer thread permanent

## Fichiers modifiés

### `src/indexer/mod.rs`
- Ajout de l'enum `WorkerMessage<D>` : `Docs(AddBatch<D>)`, `Shutdown`
- Types `WorkerSender<D>`, `WorkerReceiver<D>` (remplacent `AddBatchSender`/`AddBatchReceiver`)
- Types `FlushSender`, `FlushReceiver` : canaux dédiés par worker pour le flush
- `AddBatch<D>` toujours utilisé pour construire les batches avant envoi

### `src/indexer/index_writer_status.rs`
- `IndexWriterStatus` utilise `WorkerReceiver<D>` au lieu de `AddBatchReceiver<D>`
- Tests mis à jour avec `WorkerMessage<LucivyDocument>`

### `src/indexer/index_writer.rs`
- `IndexWriter` : `operation_sender: WorkerSender<D>`, ajout `worker_flush_senders: Vec<FlushSender>`
- `send_add_documents_batch` : wrappe dans `WorkerMessage::Docs(...)`
- `add_indexing_worker` : crée un canal flush dédié par worker, passe `flush_rx` au worker
- `prepare_commit` : envoie `Flush(oneshot_tx)` à chaque worker via son canal flush dédié, attend les réponses
- `rollback` : flush workers d'abord, puis kill segment_updater, puis `*self = new`
- `Drop` : envoie `Shutdown` à chaque worker via doc channel, puis join
- `wait_merging_threads` : envoie `Shutdown` + join au lieu de `drop_sender`
- Supprimé `recreate_document_channel` (plus nécessaire)
- Supprimé `drop_sender` (plus utilisé)
- Remplacé `index_documents` par `finalize_segment` (helper) + `worker_loop` (boucle permanente)

## Architecture du worker_loop

```
worker_loop(doc_receiver, flush_receiver, segment_updater, index, mem_budget, delete_cursor, bomb)

Boucle externe: select! {
    recv(doc_receiver) => match {
        Docs(batch) => {
            créer segment + segment_writer
            indexer le batch
            boucle interne: select! {
                recv(doc_receiver) => Docs → indexer | Shutdown → finalize + return
                recv(flush_receiver) => break Some(done_tx)
            }
            si mem_budget atteint → break None
            finalize_segment()
            si flush_done → done_tx.send(Ok(()))
        }
        Shutdown => defuse bomb, return
    }
    recv(flush_receiver) => {
        // pas de segment en cours → répondre immédiatement
        done_tx.send(Ok(()))
    }
}
```

## Bug #1 résolu : MPMC flush race

**Problème** : Premier design envoyait N `Flush` messages dans le channel MPMC partagé.
Un worker idle pouvait consommer plusieurs Flush avant qu'un worker occupé n'en reçoive un.
Résultat : le worker occupé ne finalisait jamais son segment.

**Fix** : Canaux flush dédiés par worker (`FlushSender`/`FlushReceiver`).
Chaque worker a son propre canal flush. `prepare_commit` envoie sur chaque canal.
Le worker fait `crossbeam::select!` entre le doc channel (partagé) et son flush channel (dédié).

## Bug #2 en cours : docs perdus dans les proptests

**Symptôme** : `test_delete_proptest_adding` échoue avec `left: 19, right: 2073`.
Seulement 19 docs indexés au lieu de 2073. Énorme perte de données.

**Contexte** : Le test utilise `writer_for_tests()` = 1 worker thread, budget 15MB.
Les docs sont petits, le budget n'est jamais atteint.

**Hypothèse principale** : Le problème est dans le flux quand le budget mémoire n'est
PAS atteint (cas le plus fréquent). Le worker reste dans la boucle interne `select!`
en attendant soit plus de docs, soit un Flush. Les docs sont envoyés un par un.

**Flow normal attendu** :
1. Worker idle dans outer select! → reçoit Docs → crée segment → entre inner select!
2. Inner select! reçoit plus de Docs → ajoute au segment
3. Inner select! reçoit Flush → break avec done_tx
4. Finalize segment → schedule_add_segment.wait()
5. Envoie done_tx → retour à outer select!

**Ce qui pourrait aller mal** :
- Le worker pourrait rester bloqué dans `finalize_segment` (bloqué sur `schedule_add_segment.wait()`)
  pendant que d'autres docs arrivent. Ces docs s'accumulent dans le channel mais ne sont pas perdus.
- Après finalize, le worker retourne à la boucle EXTERNE. Les docs restants dans le channel
  seront pris par la prochaine itération.

**Différence fondamentale avec l'ancien code** :
L'ancien `index_documents` recevait un iterator sur le channel entier :
```rust
// ANCIEN CODE
loop {
    let mut document_iterator = document_receiver_clone.clone().into_iter()
        .filter(|batch| !batch.is_empty()).peekable();
    if document_iterator.peek().is_none() { return Ok(()); }
    index_documents(mem_budget, index.new_segment(), &mut document_iterator, ...);
}
```
`into_iter()` sur un crossbeam receiver crée un iterator bloquant qui yield tous les
messages tant que le channel est ouvert. `index_documents` consomme cet iterator
jusqu'au budget mémoire. Si le budget n'est pas atteint, l'iterator continue de bloquer
en attendant le prochain message.

**Le point clé** : dans l'ancien code, un commit fermait le channel (via `recreate_document_channel`).
L'iterator retournait `None`, `peek()` voyait `None`, le worker sortait de la boucle.
Le worker avait consommé TOUS les docs du channel dans ses segments (potentiellement
plusieurs segments si le budget était atteint plusieurs fois).

Dans notre nouveau code : le worker ne consomme PAS l'iterator channel en continu.
Il fait `select!` message par message. Quand un Flush arrive, il break.
**MAIS** : entre deux messages `Docs`, quand le worker est dans le `select!` interne,
il attend soit un doc soit un flush. C'est équivalent.

**Autre hypothèse** : Le `select!` pourrait avoir un biais. Si les deux channels ont
des messages (un doc ET un flush), `crossbeam::select!` choisit **aléatoirement**.
Le Flush pourrait être choisi avant que tous les docs soient consommés !

C'est ça le bug ! Quand `commit()` envoie un Flush et que le channel a encore des docs
en attente, le `select!` peut choisir le Flush en premier. Le worker finalise le segment
avec seulement quelques docs, répond au commit, et les docs restants dans le channel
ne sont indexés que dans le prochain segment... MAIS `prepare_commit` attend les réponses
flush et PUIS fait `schedule_commit`. Les docs restants dans le channel ne seront pas
inclus dans ce commit — ils seront dans le prochain commit.

**C'est exactement le bug.** Le commit doit s'assurer que TOUS les docs envoyés avant
le commit sont indexés. Avec `select!` aléatoire, le Flush peut arriver avant que tous
les docs soient drainés du channel.

## Fix proposé pour Bug #2

**Option A** : Drainer le doc channel avant de traiter le Flush.
Quand le worker reçoit un Flush dans le `select!` interne, il doit d'abord
`try_recv` tous les docs restants dans le doc channel, les indexer, PUIS finaliser.

```rust
recv(flush_receiver) -> msg => {
    // Drainer tous les docs restants avant de flusher
    while let Ok(WorkerMessage::Docs(batch)) = doc_receiver.try_recv() {
        for doc in batch { segment_writer.add_document(doc)?; }
    }
    break Some(done_tx);
}
```

**Option B** : Pas de select! dans la boucle interne.
N'écouter que le doc channel dans la boucle interne. Utiliser `try_recv` sur le flush
channel après chaque batch de docs. Le flush n'est traité que quand le doc channel est
vide.

**Option C** : Envoyer le Flush via le doc channel (un seul channel), pas de select!.
Chaque worker a un ID, le Flush porte l'ID du worker cible. Seul le worker avec le bon
ID traite le Flush. Problème : race MPMC de nouveau.

**Recommandation** : Option A — simple, correct, minimal de changements.
Il faut aussi appliquer le même drain dans la boucle EXTERNE (quand le Flush arrive
et qu'il n'y a pas de segment en cours, il faut quand même drainer les docs du channel
qui pourraient être arrivés en même temps).

## Bug #2 résolu : docs perdus dans les proptests (select! + drain)

**Fix appliqué** : Option A — drain avec `try_recv` avant de traiter le Flush.

Quand le worker reçoit un Flush (boucle interne ou externe), il draine tous les docs
restants dans le doc channel avec `try_recv` avant de finaliser le segment.

**Bug #2b découvert pendant le fix** : après avoir ajouté le drain, les tests passaient
de "docs perdus" à "docs en trop" (`left: 2316, right: 2073`). Toujours 243 docs en trop.

**Cause** : Les appels `delete_cursor.skip_to(batch[0].opstamp)` étaient faits pour
CHAQUE batch dans la boucle interne et le drain. Cela avançait le curseur de suppression
au-delà de delete operations qui devaient être appliquées au segment courant.

Dans l'ancien code, `skip_to` n'était appelé qu'**une seule fois** par segment (avant le
clonage du curseur passé à `index_documents`). Le code `index_documents` ne faisait PAS
de `skip_to` pour chaque batch — il ajoutait simplement les docs au segment.

**Fix** : Ne garder `skip_to(batch[0].opstamp)` que dans la boucle EXTERNE, au moment
de la création du segment (premier batch). Retirer tous les `skip_to` de la boucle interne
et du drain.

## Bug #3 résolu : propagation d'erreur worker (tokenizer non enregistré)

**Symptôme** : `test_show_error_when_tokenizer_not_registered` échouait avec un message
d'erreur générique au lieu de l'erreur spécifique du schema.

**Cause** : Dans l'ancien code, `prepare_commit` faisait `join()` sur les workers,
récupérant directement l'erreur. Dans le nouveau code, `prepare_commit` envoie un Flush
et attend la réponse via un oneshot channel. Si le worker meurt pendant l'indexation
(avant de recevoir le Flush), le oneshot sender est droppé → erreur générique.

**Fix** : Ajout de `harvest_worker_error()` qui envoie Shutdown à tous les workers puis
fait `join()` pour récupérer l'erreur réelle. Appelé quand `flush_sender.send()` ou
`rx.recv()` échoue dans `prepare_commit`.

## État final de la compilation et des tests

- **Compile** : ✓
- **Tests passants** : 1118/1118 (1066 lib + 50 doctests + 2 compile-fail)
- **Tests ignorés** : 8
- **Tests échouants** : 0

## Résumé des changements dans `worker_loop`

1. `skip_to` appelé UNE SEULE FOIS par segment (outer loop, premier batch)
2. `try_recv` drain dans le inner flush handler (pas de `skip_to`)
3. `try_recv` drain dans le outer flush handler (un seul `skip_to` pour le premier batch drainé)
4. `harvest_worker_error` dans `prepare_commit` pour propager les erreurs worker
