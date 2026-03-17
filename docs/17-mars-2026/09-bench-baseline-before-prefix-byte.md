# Bench baseline — avant prefix byte partitioning

Date : 17 mars 2026

## Release build, 5K docs, 4 shards, rag3db clone

### Dernier bench avec SI=0 filter (sans prefix byte partitioning)

```
Index time:  1-shard 2.44s  |  TA-4sh 2.40s  |  RR-4sh 2.25s

Query                                 Hits    1-shard     TA-4sh     RR-4sh
---------------------------------------------------------------------------
contains 'function'                     20     71.5ms     38.9ms     36.7ms
contains_split 'create index'           20    224.8ms    116.9ms    112.6ms
contains 'segment'                      20     86.8ms     54.4ms     50.8ms
startsWith 'segment'                    20    115.6ms     61.6ms     58.6ms
contains 'rag3db'                       20     84.4ms     39.4ms     50.2ms
startsWith 'rag3db'                     20    100.5ms     54.7ms     48.4ms
contains 'kuzu'                         20     93.6ms     51.5ms     49.2ms
startsWith 'kuzu'                       20     78.0ms     43.4ms     43.8ms
contains 'cmake' (path)                 20      1.6ms      1.4ms      1.0ms

Balance CV:  TA 0.006  |  RR 0.000
```

### Ce qu'on attend du prefix byte partitioning

- **startsWith** : le FST skip nativement les entrées SI>0 → devrait être plus rapide que contains (moins d'entrées à traverser)
- **contains** : même travail (walk deux partitions + merge) → devrait être pareil ou légèrement plus lent (overhead du merge HashMap)
- **Indexation** : FST ~5% plus gros (entries dupliquées pour "core" SI=0 + "core" SI>0) → marginal

### Bench précédent avant SI=0 filter (RegexContinuationQuery)

```
startsWith 'segment' TA-4sh: 196ms  (avant SI=0 filter)
startsWith 'rag3db'  TA-4sh: 123ms
startsWith 'kuzu'    TA-4sh:  66ms
```

### Historique des gains

| Optimisation | startsWith 'segment' TA-4sh |
|---|---|
| RegexContinuationQuery (baseline) | 196ms |
| SI=0 runtime filter | 62ms (3.2x) |
| Prefix byte partitioning (attendu) | <50ms (FST skip natif) |
