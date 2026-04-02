# 05 — TODO : vérifications WASM en attente

Date : 2 avril 2026

## A vérifier

### 1. Highlights fuzzy d=1 encore parfois incorrects en WASM
- Reproduire en indexant https://github.com/L-Defraiteur/rag3db dans le playground
- Query : `rak3weaver` contains fuzzy d=1
- Les highlights semblent plus corrects qu'avant mais pas encore 100%
- Comparer avec les résultats du test natif test_fuzzy_ground_truth (296/296 valid)
- Possible divergence entre le chemin WASM (pas de drain_merges) et le chemin natif

### 2. Performance indexation/commit dégradée
- Le commit semble plus lent qu'avant dans le playground WASM
- Timing ajouté dans build_derived_indexes (`[derive-timing]` dans la console)
- Hypothèses à vérifier :
  - SepMap rebuild depuis gapmap (O(num_docs × num_tokens)) potentiellement coûteux
  - FreqMap HashMap accumulation en mémoire
  - Overhead DAG pour le segment initial (thread spawn/join)
  - Nombre de fichiers écrits augmenté (freqmap ajouté)
- En attente des logs timing du playground

### 3. Opti possiblement perdue
- A investiguer une fois les timings disponibles
