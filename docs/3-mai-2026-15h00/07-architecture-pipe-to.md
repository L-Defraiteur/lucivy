# Architecture : pipe_to — flow du commit sharde

## Aujourd'hui (Suspend + poll_idle)

Le ShardActor fait tout le cablage a la main, poll_idle finalise.
Le thread scheduler est capture pendant le commit DAG.

```mermaid
graph TD
    subgraph "Thread externe"
        A["commit()"] --> B["drain_pipeline()"]
        B --> C["Pool::scatter(CommitMsg)"]
        C --> D["scheduler.wait() x4"]
    end

    subgraph "ShardActor handler (scheduler thread)"
        E["handle(CommitMsg)"] --> F["flush_workers()"]
        F --> G["JoinResume(N)"]
        G --> H["set_resume() x N"]
        H --> I["return Suspend"]
    end

    subgraph "ShardActor poll_idle (scheduler thread)"
        J["poll_idle()"] --> K["collect flush results"]
        K --> L["finalize_flush_and_prepare()"]
        L --> M["writer.commit()"]
        M --> N["execute_dag() !!!"]
        N --> O["scheduler.wait() cooperative"]
        O --> P["reply.send(Ok)"]
    end

    D -.->|"wait"| E
    I -.->|"Suspend"| J
    style N fill:#ff6b6b
    style O fill:#ff6b6b
```

Probleme : `execute_dag` fait des cooperative waits sur un scheduler thread.
Si les 4 shards font ca en meme temps, starvation.

## Avec pipe_to + task_pipe_to (cible)

Aucun handler ne bloque. Le commit lourd tourne sur un thread task.
Les resultats reviennent comme des messages FIFO.

```mermaid
graph TD
    subgraph "Thread externe"
        A["commit()"] --> B["drain_pipeline()"]
        B --> C["Pool::collect_to(CommitMsg)"]
        C --> D["return — non bloquant"]
        D --> E["scheduler.wait() pour le resultat final"]
    end

    subgraph "ShardActor handler — Etape 1"
        F["handle(Commit)"] --> G["pool.collect_to(FlushMsg)"]
        G --> H["return Continue"]
    end

    subgraph "Indexer workers (N)"
        I1["handle(FlushMsg)"] --> I2["finalize_segment"]
        I2 --> I3["reply.send(bytes)"]
    end

    subgraph "ShardActor handler — Etape 2"
        J["handle(FlushDone)"] --> K["task_pipe_to(commit_work)"]
        K --> L["return Continue"]
    end

    subgraph "Task thread (pool)"
        M["finalize_flush_and_prepare()"] --> N["writer.commit()"]
        N --> O["execute_dag()"]
        O --> P["reader.reload()"]
        P --> Q["reply.send(result)"]
    end

    subgraph "ShardActor handler — Etape 3"
        R["handle(CommitDone)"] --> S["reply.send(Ok) au caller"]
        S --> T["return Continue"]
    end

    H -.->|"FlushMsg"| I1
    I3 -.->|"collect_to callback"| J
    L -.->|"task scheduled"| M
    Q -.->|"pipe callback"| R

    style H fill:#90EE90
    style L fill:#90EE90
    style T fill:#90EE90
```

Chaque handler retourne Continue immediatement (vert).
Le travail lourd (execute_dag) tourne sur un thread task, pas sur un scheduler thread qui dispatche des acteurs.

## Messages du ShardActor

```mermaid
graph LR
    subgraph "ShardMsg enum"
        Insert["Insert"]
        Search["Search"]
        Delete["Delete"]
        Commit["Commit(fast, reply)"]
        FlushDone["FlushDone(results, fast, reply)"]
        CommitDone["CommitDone(result, reply)"]
        Drain["Drain"]
    end

    subgraph "Flow commit"
        Commit -->|"collect_to"| FlushDone
        FlushDone -->|"task_pipe_to"| CommitDone
    end
```

## Comparaison des patterns

```mermaid
graph TD
    subgraph "Pattern Suspend (ancien)"
        S1["handler"] --> S2["send + JoinResume"]
        S2 --> S3["return Suspend"]
        S3 --> S4["poll_idle"]
        S4 --> S5["collect results"]
        S5 --> S6["blocking work"]
        S6 --> S7["reply"]
        style S3 fill:#ff6b6b
        style S6 fill:#ff6b6b
    end

    subgraph "Pattern pipe_to (nouveau)"
        P1["handler"] --> P2["collect_to / pipe_to"]
        P2 --> P3["return Continue"]
        P3 --> P4["handler(ResultMsg)"]
        P4 --> P5["task_pipe_to si lourd"]
        P5 --> P6["return Continue"]
        P6 --> P7["handler(DoneMsg)"]
        P7 --> P8["reply"]
        style P3 fill:#90EE90
        style P6 fill:#90EE90
    end
```

## Vision future : execute_dag async

Aujourd'hui `execute_dag` est synchrone (bloque le thread appelant).
A terme, on pourrait le rendre pipe_to-based :

```mermaid
graph TD
    subgraph "execute_dag sync (actuel)"
        DS1["run node A"] --> DS2["submit_task(B)"]
        DS2 --> DS3["scheduler.wait(B)"]
        DS3 --> DS4["run node C"]
        DS4 --> DS5["return result"]
        style DS3 fill:#ff6b6b
    end

    subgraph "execute_dag async (futur)"
        DA1["schedule node A"] --> DA2["A.pipe_to -> schedule B"]
        DA2 --> DA3["B.pipe_to -> schedule C"]
        DA3 --> DA4["C.pipe_to -> deliver result"]
        style DA2 fill:#90EE90
        style DA3 fill:#90EE90
    end
```

Chaque node completion declenche la suivante via pipe_to.
Aucun thread ne wait jamais. Mais c'est un refacto du runtime DAG
— pas necessaire maintenant car `task_pipe_to` suffit.
