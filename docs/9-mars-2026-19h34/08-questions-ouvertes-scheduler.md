# Questions ouvertes — Architecture Scheduler/Actor

À traiter avant ou pendant l'implémentation. Certaines peuvent changer le design
des docs 06-07 si les réponses invalident des hypothèses.

---

## 1. Le Mutex central est un goulot d'étranglement potentiel

```rust
struct SharedState {
    ready_queue: Mutex<BinaryHeap<ReadyEntry>>,
    actors: Mutex<HashMap<ActorId, ActorSlot>>,
    ...
}
```

Deux Mutex globaux partagés entre tous les threads du pool — c'est le design naïf
d'un scheduler. En mode N=8, chaque `handle_one()` va contester ces locks. Les
schedulers de production (tokio, actix) évitent ça avec des **run queues locales par
thread + work-stealing global** seulement en cas de starvation.

Est-ce que on a mesuré quel overhead on peut accepter ? Pour lucivy (usage indexation,
pas serveur web), peut-être que N≤4 et que ça n'a pas d'importance, mais ça vaut
d'être explicite.

> **Annotation** : En pratique ld-lucivy a ~6 acteurs max. La contention sera faible
> comparée à un serveur web avec des milliers de tasks. Mais si on veut être propre,
> une run queue locale par thread avec steal global en fallback serait mieux.
> À benchmarker avant d'optimiser — ne pas tomber dans le piège du over-engineering
> prématuré. Le Mutex naïf est le bon point de départ, on optimise si les benchmarks
> montrent un problème.

---

## 2. poll_idle vs un vrai système de yield incrémental pour le MergerActor

Le design du merge coopératif via `poll_idle` est correct dans l'esprit, mais
l'interface est un peu fragile :

```rust
fn poll_idle(&mut self) -> Poll<()> {
    // Poll::Ready(()) = "encore du travail"  ← sémantique contre-intuitive
    // Poll::Pending   = "plus rien"
}
```

`Poll::Ready(())` signifiant "j'ai encore du boulot" c'est l'inverse de la convention
Rust (`Ready` = terminé). Renommer ça `HasWork` / `Idle` ou utiliser `bool` serait
plus clair. Petit problème mais source de bugs.

Plus fondamentalement : le merge incrémental `MergeState::step()` nécessite de
serialiser l'état du merge entre les steps. Si le merge actuel est une grosse fonction
synchrone itérant sur des segments, découper ça en une state machine peut être **le plus
gros refactoring du projet** — plus gros que l'infra actor elle-même. C'est bien
mentionné en risque #2 du doc 06, mais c'est probablement sous-estimé dans le plan.

> **Annotation** : Deux pistes pour éviter le refactoring massif du merge :
>
> 1. **Ne pas rendre le merge incrémental en Phase 4.** Le MergerActor traite le
>    message `Merge` en bloquant. En mode multi-thread c'est OK (il a son thread).
>    En mode single-thread, le merge bloque tout — mais il est déclenché entre les
>    commits donc l'indexation n'est pas affectée. C'est "assez bon" pour le MVP.
>
> 2. **Spawn le merge sur un thread dédié hors scheduler.** Le MergerActor envoie
>    le travail sur un `std::thread::spawn` et poll le résultat. Pas coopératif
>    mais pas bloquant pour le scheduler.
>
> Le merge incrémental (vraie state machine) reste un objectif à long terme.
> Renommer `poll_idle` en quelque chose de plus explicite : oui, à faire dès Phase 1.

---

## 3. Le deadlock potentiel dans `wait_cooperative` avec des acteurs imbriqués

La séquence commit montre un double wait imbriqué :

```
IndexerActor.handle(Flush)
  → envoie AddSegment à SegmentUpdaterActor
  → reply2.wait(scheduler)      ← appel récursif au scheduler
```

En mode single-thread, `wait_cooperative` appelle `scheduler.run_one_step()` en boucle.
Mais si ce `run_one_step()` prend le lock `SharedState.actors`, et que l'acteur courant
(IndexerActor) est encore dans ce lock... **deadlock** selon l'implémentation.

Il faut vérifier que le lock est libéré avant d'appeler `wait_cooperative` — ou
structurer le scheduler pour qu'un acteur puisse re-entrer.

> **Annotation** : C'est le point le plus dangereux du design. Trois solutions :
>
> 1. **L'acteur ne fait PAS de wait dans handle().** Au lieu de ça, il envoie le
>    message et retourne `Yield`. Le scheduler rappelle `poll_idle()` qui check si
>    la Reply est revenue. Pas de réentrance, mais ça complexifie les acteurs
>    (state machine explicite pour chaque séquence request/reply).
>
> 2. **Le lock actors est libéré pendant handle().** Le scheduler prend l'acteur
>    OUT du HashMap, appelle handle, puis le remet. Pendant handle, le lock est
>    libre. `run_one_step()` peut prendre le lock pour un AUTRE acteur sans deadlock.
>
> 3. **Pas de Mutex, des slots avec accès par index.** `actors: Vec<Option<ActorSlot>>`,
>    accès par ActorId = index. Pas de lock pour accéder à un slot spécifique.
>    Le scheduler "emprunte" un slot (Option::take), le traite, le remet.
>
> L'option 2 est probablement la plus pragmatique. À valider avec un test de
> réentrance dès Phase 1.

---

## 4. Backpressure et question ouverte non résolue

Le doc 06 mentionne "bounded vs unbounded ?" comme question ouverte mais ne tranche pas.
Pour un moteur d'indexation qui peut recevoir des millions de docs en burst, c'est
critique.

`PIPELINE_MAX_SIZE_IN_DOCS = 10_000` comme capacité par défaut de la mailbox
IndexerActor — ça signifie que `add_document()` va bloquer le caller si les indexers
sont en retard. C'est le comportement actuel aussi, mais avec l'actor model on pourrait
faire mieux : retourner un `Result<(), Backpressure>` et laisser le caller décider
d'attendre ou de batcher différemment.

> **Annotation** : Le comportement bloquant actuel (crossbeam bounded channel) est
> en fait correct pour l'indexation — ça empêche l'OOM en limitant le buffer.
> L'API `add_document` est synchrone et bloquante dans tantivy, changer ça casserait
> la compat. Garder bounded(10_000) pour le MVP, explorer les alternatives plus tard
> si le profiling montre un problème.

---

## 5. Scheduler custom vs executor existant

On construit un scheduler custom en Rust pour un use case très spécifique (N acteurs
connus à l'avance, pas de spawn dynamique chaud). Est-ce qu'on a considéré d'utiliser
un executor existant léger plutôt que d'écrire le nôtre ?

- **smol** — executor async-std léger, sans tokio, tourne sur 1 à N threads, compile
  pour WASM
- **async-channel** pour les mailbox, `smol::spawn` pour les acteurs

L'avantage : on garde notre model actor (traits, messages typés, priorités), mais on
délègue le scheduling réel à une lib battle-testée. L'inconvénient : on perd du
contrôle sur les priorités (smol est work-stealing LIFO, pas priority queue). Mais
peut-être qu'on n'a pas besoin des priorités dynamiques en pratique — les benchmarks
le diraient.

> **Annotation** : Arguments pour le scheduler custom :
> - Contrôle total sur les priorités (critique pour le mode single-thread)
> - Pas de dépendance async runtime (smol tire async-io, polling, etc.)
> - Le scheduler ld-lucivy est simple (~300 lignes) vs un runtime généraliste
> - L'observabilité (events) est triviale à intégrer quand on contrôle le scheduler
>
> Arguments pour un executor existant :
> - Battle-testé, edge cases déjà couverts
> - Work-stealing optimisé sans avoir à le coder
> - Moins de code à maintenir
>
> **Recommandation** : commencer custom (Phase 1), benchmarker. Si le scheduling
> devient un goulot, évaluer smol comme remplacement du run_loop interne sans
> changer le model actor (les traits restent les mêmes, seul le dispatcher change).
> C'est une décision réversible — ne pas sur-investir dans le choix avant d'avoir
> des données.
