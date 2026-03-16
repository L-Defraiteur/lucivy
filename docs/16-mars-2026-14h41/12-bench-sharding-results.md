# Bench Sharding — Résultats 16 mars 2026

## Setup

- Corpus : rag3db clone (~5365 fichiers texte, mêmes excludes que build_dataset.py)
- Build : **debug** (release sera ~10x plus rapide en absolu)
- Machine : 12 threads
- 4 shards, balance_weight=0.2 (token-aware) vs 1.0 (round-robin)

## Résultats 1K docs

```
Index time:  1-shard 2.77s  |  TA-4sh 6.14s  |  RR-4sh 5.51s
TA distribution: [237, 250, 243, 270]  CV=0.050
RR distribution: [250, 250, 250, 250]  CV=0.000

Query                                 Hits    1-shard     TA-4sh     RR-4sh
---------------------------------------------------------------------------
contains 'rag3db'                       20    205.9ms     77.3ms     90.1ms
contains 'kuzu'                         20    208.4ms     60.1ms     88.5ms
contains 'function'                     20    202.4ms     73.2ms     88.0ms
contains 'create index'                 20    463.5ms    166.7ms    153.9ms
startsWith 'segment'                    11    246.3ms     72.7ms     78.3ms
contains 'cmake'                        20      4.9ms      2.6ms      2.6ms
```

## Résultats 5K docs

```
Index time:  1-shard 16.27s  |  TA-4sh 20.24s  |  RR-4sh 19.21s
TA distribution: [1256, 1254, 1253, 1237]  CV=0.006
RR distribution: [1250, 1250, 1250, 1250]  CV=0.000

Query                                 Hits    1-shard     TA-4sh     RR-4sh
---------------------------------------------------------------------------
contains 'rag3db'                       20    937.6ms    314.7ms    333.4ms
contains 'kuzu'                         20    972.1ms    333.7ms    372.7ms
contains 'function'                     20    950.0ms    323.7ms    313.4ms
contains 'create index'                 20   2119.4ms    643.7ms    789.8ms
startsWith 'segment'                    20   2248.2ms    720.8ms    781.4ms
contains 'cmake'                        20     21.4ms     10.4ms      9.3ms
```

## Analyse

### Search
- **Speedup 4-shard vs 1-shard : ~3x** (quasi-linéaire)
- **TA bat RR de 10-19%** sur tokens rares/mid-frequency (kuzu, "create index")
- RR gagne marginalement (~3%) sur tokens ultra-fréquents ("function") — normal
- TA distribution quasi-parfaite à 5K docs : CV=0.006

### Indexation — le bottleneck
- 1-shard : 16.27s pour 5K docs
- TA-4sh : 20.24s (+24% overhead routing + 4× writers)
- RR-4sh : 19.21s (+18%)
- **L'indexation est 10-20x plus lente que la recherche** — c'est le prochain axe d'optimisation
- En debug build, l'overhead est amplifié (bounds checks, no inlining)

### Scatter-gather BM25
- Weight compilé 1 fois avec stats globales → IDF exact cross-shard
- Aucune différence de scoring entre TA et RR (même IDF global)

## Prochaine étape : optimisation indexation

L'indexation à 5K docs/16s = ~307 docs/s en debug. En release ~3000 docs/s.
Pour 213K docs ça ferait ~71s en release, acceptable mais améliorable.

Axes :
1. Commit batch (actuellement 1 commit/1000 docs pour single, 1 commit final pour sharded)
2. Paralléliser l'insertion dans les shards (actuellement séquentiel)
3. Réduire l'overhead du tokenize+hash dans add_document
4. Release build pour le vrai bench
