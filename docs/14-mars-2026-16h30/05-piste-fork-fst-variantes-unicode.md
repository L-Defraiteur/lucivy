# Piste : fork du FST — contrôle total sur le pipeline de recherche

Date : 14 mars 2026 — 16h30 (mis à jour 14 mars 2026)

Statut : piste de recherche, pas encore décidé

## Le problème fondamental

Le FST mappe un terme (bytes) vers une valeur (u64). Un suffix comme "import"
n'existe qu'une seule fois dans le FST, avec un seul SI. Mais "import" peut
provenir de "IMPORT" (I=1 byte) ou de "İMPORT" (İ=2 bytes). Le SI en bytes
est différent selon l'original.

```
FST actuel :
  "import" → (raw_ordinal=5, SI=0)

Mais SI=0 signifie "byte 0 du token original".
Pour "IMPORT" : byte 0 = 'I' ✓
Pour "İMPORT" : byte 0 = premier byte de İ ✓ (mais byte 1 = milieu de İ ✗)

Le problème arrive pour SI > 0 :
  "mport" → SI=1
  Pour "IMPORT" : byte 1 = 'M' ✓
  Pour "İMPORT" : byte 1 = deuxième byte de İ ✗ (İ fait 2 bytes)
```

## Pistes de résolution du bug Unicode

### Idée A : FST avec output multi-valué

Stocker **plusieurs valeurs** par terme — une par variante de byte-width.
Résout le problème mais explose la taille du FST.

### Idée B : FST qui stocke SI en chars, pas en bytes

SI_chars est la même valeur quelle que soit la variante. Mais convertir
SI_chars en byte offset nécessite le texte original, qu'on n'a pas à la
recherche.

### Idée C : SI per-occurrence dans la posting list ._raw

Le FST stocke juste le raw_ordinal, le SI est per-occurrence dans le ._raw.
Résout le problème mais modifie le format de la posting list. Invasif.

### Idée D : char-to-byte mapping dans la GapMap

Per token : flag ASCII (0x00, 1 byte overhead) ou flag MULTIBYTE (0x01 +
bitmap des chars multi-byte). Fast path ASCII = zéro lookup.

### Idée E : normalisation Unicode dans le tokenizer ._raw

Normaliser les ~5 caractères problématiques (İ → I, etc.) avant le lowercase.
Les byte widths sont préservées. Minimum de code, résout BUG-1/2.

## Au-delà du bug : pourquoi forker le FST

Le bug Unicode est le déclencheur, mais un fork du FST donne le contrôle
total sur la structure de données la plus critique du pipeline. Voici ce
qu'un FST custom apporterait.

### 1. Multi-output natif

Stocker plusieurs valeurs par terme : variantes Unicode, multiple parents
inline (au lieu de la parent_list séparée actuelle). Élimine le lookup
indirect et la sérialisation/désérialisation de la table annexe.

### 2. Compression optimisée pour le code source

Le FST standard (BurntSushi/fst) compresse pour du texte naturel. Le code
source a des patterns spécifiques :
- **camelCase** : les préfixes divergent tôt (getElementById, getElement...)
  mais les suffixes sont très partagés (Id, Name, Element, Type...)
- **snake_case** : longs préfixes communs (get_element_by_id, get_element_by_name)
- **Identifiants techniques** : rag3db, lucivy, Vec, HashMap, impl...
- **Full Unicode** : noms de variables en CJK, accents, cyrillique — le fork
  doit rester **généraliste** et gérer toute la surface UTF-8, pas seulement
  l'ASCII

Un FST custom pourrait adapter la compression (taille du registre de
déduplication des nœuds, heuristiques de partage) pour ces patterns tout
en restant correct sur l'ensemble d'Unicode.

### 3. Bloom filter intégré

Rejeter les lookups négatifs en O(1) avant le walk FST. Utile pour les
queries sur des termes inexistants (fréquent en code search).

### 4. Build sans tri préalable

Le builder actuel (`fst::MapBuilder`) exige les termes en ordre
lexicographique strict. Un builder forké pourrait :
- Accepter les termes dans l'ordre d'insertion
- Trier internalement (buffer + sort)
- Ou construire incrémentalement (plus complexe)

Note : dans l'architecture à segments de lucivy, le sort externe est
déjà le pattern standard. Ce point est donc nice-to-have, pas critique.

### 5. Merge natif de FSTs

Merger deux FST sans reconstruire from scratch. Le FST standard ne supporte
que le streaming merge (union/intersection/difference) qui produit un flux
de (key, values) qu'il faut réinjecter dans un nouveau Builder = rebuild
complet.

Un merge natif (fusionner les automates directement) est algorithmiquement
possible mais complexe. Alternative pragmatique : tiered/LSM approach avec
multiple FSTs et merge on read.

### 6. WASM-first layout

- Pas de mmap (juste un `&[u8]` contiguous)
- Pas de pointeurs 64-bit inutiles (WASM = 32-bit address space)
- Alignement réduit pour minimiser le padding
- Le crate `fst` fonctionne déjà en WASM (`AsRef<[u8]>`, pas de mmap
  en core) mais le layout n'est pas optimisé pour 32-bit

## Recherche : quelle lib Rust forker ?

### Comparatif des candidats

| Crate | Version | Downloads | LOC | Output | Builder | Merge | WASM | License |
|-------|---------|-----------|-----|--------|---------|-------|------|---------|
| **`fst` (BurntSushi)** | 0.4.7 | 16.5M | ~4-5K | 1x u64 additif | sorted-only | streaming (rebuild) | oui | MIT/Unlicense |
| `tantivy-fst` | 0.5.0 | 12.7M | ~5.4K | 1x u64 additif | sorted-only | streaming (rebuild) | oui | MIT/Unlicense |
| `rustfst` (Garvys) | 1.2.6 | 750K | ~30K | weighted arcs (semirings) | state-by-state | composition, union | non testé | MIT/Apache-2.0 |
| `yada` | 0.5.1 | 12.1M | petit | 31-bit uint | sorted | non | probable | MIT/Apache-2.0 |
| `cedarwood` | 0.4.6 | 1.65M | moyen | 1x i32 | updatable | non | non testé | BSD-2 |
| `furze` | 0.1.1 | 12.9K | petit | u64 | sorted | non | non testé | MIT |

### Analyse détaillée

**`fst` (BurntSushi) — LE candidat**

Le choix évident. 4-5K LOC de code propre et bien documenté, par l'auteur
de ripgrep. Architecture :
- `Fst<D: AsRef<[u8]>>` : storage-agnostic (mmap, Vec, &[u8])
- Nodes compacts avec transitions varint-packed
- Registre de déduplication (10K entrées par défaut) pour le partage de suffixes
- Automaton trait : Levenshtein DFA, Regex, Subsequence, StartsWith...
- Footer : version, type, root address, key count

C'est la base de `tantivy-fst` (quasi-clone, juste edition 2021 + ajustements
API mineurs). Forker `fst` directement donne une base plus propre.

**`tantivy-fst` — pas d'avantage**

Fork minimal de BurntSushi/fst. Le README dit explicitement "Please do not
depend on this crate directly". Les différences sont cosmétiques.
C'est ce que lucivy utilise actuellement (`tantivy-fst = "0.5"`).

**`rustfst` — mauvais domaine**

30K LOC conçu pour le NLP (speech recognition, traduction). Weighted FSTs
avec semirings, composition, epsilon-removal. Modèle complètement différent
du terme→postings d'un moteur de recherche. Trop gros, trop complexe, pas
le bon outil.

**`yada` — limité**

Double-array trie, pas un vrai FST. Limite à 31-bit outputs et ~2GB.
Lookup exact rapide mais moins bonne compression que le FST pour les grands
vocabulaires. Pas de merge, pas d'automaton trait.

**`cedarwood` — intéressant mais immature**

Seul candidat updatable après construction. Mais beta, BSD-2, i32 outputs,
pas de merge. Trop risqué comme base.

### Verdict : fork de `fst` (BurntSushi)

**Pourquoi :**
- Code le plus propre et le plus petit (~4-5K LOC)
- License la plus permissive (MIT/Unlicense)
- Architecture prouvée (16.5M downloads, base de ripgrep et tantivy)
- `AsRef<[u8]>` = déjà WASM-friendly
- Automaton trait = on garde la compatibilité DFA Levenshtein
- Communauté qui comprend le code (2.1K stars, 140 forks)

**Ce qu'il faudrait modifier :**

| Feature | État actuel dans `fst` | Travail estimé |
|---------|----------------------|----------------|
| Multi-output | 1x u64, additif le long du chemin | Remplacer `Output(u64)` par un type richer. Option simple : u64 = offset dans une table externe (pattern TermInfoStore de tantivy). Option avancée : varint-encoded list inline dans le nœud. |
| Registre adapté code source | 10K entrées, heuristique texte naturel | Tuner la taille du registre et les heuristiques de partage. Le partage de suffixes (DAFSA) profite déjà au camelCase. |
| Build unsorted | Rejet strict si non-trié | Buffer + sort externe (pragmatique). Le pattern segments fait que c'est un nice-to-have. |
| Merge natif | Streaming only, rebuild requis | Complexe. Alternative LSM/tiered plus pragmatique. |
| WASM layout | Fonctionne mais pas optimisé 32-bit | Varint 32-bit, suppression code mmap, alignement compact. `fst-no-std` (fork existant) comme référence. |
| Unicode correct | Bytes natifs, UTF-8 ok | Déjà ok. Le Levenshtein automaton gère les codepoints Unicode. |

**Stratégie d'adoption :**

Le fork ne serait utilisé QUE pour le .sfx (suffix FST). Le ._raw et le
TermDictionary principal restent sur `tantivy-fst` standard. Cela limite
le blast radius : si le fork a un bug, seul le suffix search est affecté,
le reste du pipeline fonctionne normalement.

À terme, si le fork est stable et benchmarké, il pourrait remplacer
`tantivy-fst` partout dans lucivy.

## Approche recommandée

### Court terme (Phase 7)

Normalisation Unicode dans le tokenizer ._raw (`ByteWidthPreservingFilter`).
Résout BUG-1/2 sans toucher au FST. Minimum de code.

### Moyen terme

Char-to-byte mapping dans la GapMap (1 byte overhead per token, fast path
ASCII). Résout le problème à la racine sans toucher au FST ni au tokenizer.

### Long terme

Fork de `fst` (BurntSushi) en crate `lucivy-fst`. Modifications ciblées :
multi-output, compression tuned, WASM layout. Déploiement progressif,
d'abord sur le .sfx seul, puis (si validé) sur tout le pipeline.

## Conclusion

Le fork de `fst` (BurntSushi) est le meilleur candidat. Code propre (~4-5K
LOC), licence permissive, architecture prouvée. Les modifications nécessaires
sont ciblées et incrémentales — pas besoin de tout réécrire.

L'approche généraliste est non-négociable : le FST doit gérer toute la
surface UTF-8 (CJK, cyrillique, accents, emojis dans les strings...).
Les optimisations pour le code source (camelCase, snake_case, préfixes
longs) sont des bonus de compression, pas des restrictions de scope.

Pas prioritaire pour le MVP suffix search. À planifier comme projet séparé
quand le pipeline .sfx est validé et benchmarké sur corpus réel (Phase 10).
