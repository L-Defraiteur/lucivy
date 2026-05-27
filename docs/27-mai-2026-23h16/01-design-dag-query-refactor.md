# Design : Refactor find_literal_v3 en DAG luciole

> 27 mai 2026 — Session 7, branche feature/sibling-table-v3

## Motivation

`find_literal_v3` (composite.rs:31-139) est une fonction impérative de ~110 lignes avec :
- 3 pipelines (single-token, chunk-chains, word-chains) qui convergent
- ~12 `ctx.trace_msg()` tissés dans la logique
- un bloc `if ctx.trace_id.is_some()` qui re-résout les postings juste pour logger
- un double systeme de trace (QueryTrace mutex global + trace_msg manuels)

Le refactor vise a :
1. Reifier le flux en DAG luciole inspectable
2. Tuer le double systeme de trace — l'observabilite devient une propriete du DAG
3. Obtenir un "explain" gratuit (le DAG annote = l'explain)
4. Debloquer le diagnostic des 4 FN restants a 5K docs

## Pourquoi luciole et pas un DAG maison

Apres lecture du code luciole (session claude.ai du 26 mai) :

- **Node trait est synchrone** (node.rs:74) — `execute(&mut self, ctx: &mut NodeContext)`. Le threading est dans le runtime, pas dans le noeud.
- **PortValue = `Arc<dyn Any + Send + Sync>`** (port.rs:57) — zero copie, cout = 1 allocation Arc par arete. Fan-out par clone d'Arc.
- **`topological_levels()`** (dag.rs:150) est une methode publique du Dag, pas du scheduler. Separation description/execution deja en place.
- **ServiceRegistry** (node.rs:15-37) — `register<T>()` / `get<T>()` pour les ressources ambiantes.

Cout sur le chemin chaud : ~10 allocations Arc par query, ~10 downcasts TypeId. Negligeable dans un budget sub-100ms.

## Ce qu'il faut ajouter a luciole

### SequentialRunner (~30 lignes)

```rust
pub fn execute_sequential(dag: &mut Dag) -> Result<(), String> {
    let levels = dag.topological_levels()?;
    for level in &levels {
        for &node_idx in level {
            // 1. Peupler NodeContext avec les outputs des noeuds amont
            // 2. Appeler node.execute(&mut ctx)
            // 3. Recolter les outputs + metrics
        }
    }
    Ok(())
}
```

Justification : "Execute un DAG de noeuds synchrones sur le thread courant en ordre topologique — utile pour les tests, le debug, et les cibles mono-thread (WASM)." Se redige sans mentionner lucivy. Citoyen de premiere classe a cote du scheduler threade.

### Trait PortSummary (optionnel, petit)

```rust
pub trait PortSummary {
    fn summary(&self) -> String;
}
```

Permet a l'explain de rendre le contenu des aretes sans connaitre les types concrets. Exemple : `"chains: 47 produites"`, `"candidates: 12"`. Les types de donnees (Vec<ChainV3>) l'implementent cote lucivy, pas cote luciole.

## Architecture du DAG find_literal_v3

### Noeuds

| Noeud | in | out | Mode lecture in | Correspond a |
|---|---|---|---|---|
| **FstCandidates** | (trigger) | `candidates: Vec<CandidateV3>` | — | `fst_candidates_v3()` |
| **ResolveSingle** | `candidates` | `single: Vec<MatchV3>` | downcast | `resolve_single_v3()` |
| **ChunkChain** | (trigger) | `chunk_chains: Vec<ChainV3>` | — | `cross_chunk_chain_v3()` |
| **SiblingChunk** | `candidates` | `sib_chunk: Vec<ChainV3>` | downcast | falling_walk + splits_from_fst + sibling_chain_dfs |
| **ResolveChunk** | `chunk_chains` + `sib_chunk` | `chunk_matches: Vec<MatchV3>` | take + take | `resolve_chains_v3()` |
| **WordChain** | (trigger) | `word_chains: Vec<ChainV3>` | — | `cross_word_chain_v3()` |
| **SiblingWord** | `candidates` | `sib_word: Vec<ChainV3>` | downcast | falling_walk + splits_from_fst + sibling_chain_dfs (word) |
| **ResolveWord** | `word_chains` + `sib_word` | `word_matches: Vec<MatchV3>` | take + take | `resolve_word_chains_v3()` |
| **Merge** | `single` + `chunk_matches` + `word_matches`? | `results: Vec<MatchV3>` | take + take + take | sort_by_key + dedup |

### Topologie (forme statique)

```
                    FstCandidates
                   /      |       \
          (downcast) (downcast)  (downcast)
            /          |            \
    ResolveSingle  SiblingChunk   SiblingWord*
         |             |              |
         |       ChunkChain      WordChain*
         |          \  /            \  /
         |      ResolveChunk    ResolveWord*
          \          |              /
           \         |            /
                   Merge
```

`*` = conditionnel (`!strict_separators && has_word_pipeline()`)

### Invariants (a ecrire dans le code)

1. **Le port `candidates` de FstCandidates est TOUJOURS lu en downcast** (3-4 consommateurs selon la config). Jamais take. Un take panique au runtime, mais seulement en mode relaxed avec word+sibling — bug conditionnel invisible aux tests isolés.

2. **La forme du graphe varie selon 3 booleens** (`strict_separators`, `has_word_pipeline()`, `has_sibling_chains()`), jamais selon les donnees. Construction du Dag une fois par query.

3. **anchor_start est un parametre de noeud** (filtre dans ChunkChain/WordChain), pas un noeud separe. Ne pas sur-decouper.

4. **La granularite du graphe est statique et grossiere** (noeud -> noeud). Toute multiplicite vit dans les Vec sur les aretes, jamais dans la topologie. Pas de ports dynamiques / arrays de ports.

### Services vs Aretes

**Regle** : une arete c'est ce qu'un noeud *produit*, un service c'est ce qu'un noeud *consulte*.

| Donnee | Type | Justification |
|---|---|---|
| reader (SfxFileReaderV3) | **service** | Existe avant la query, lu en partage, ne change pas |
| resolver (PostingResolver) | **service** | Idem |
| sibling_v3 (SiblingTableReader) | **service** | Idem |
| termtexts (TermTextsReaderV3) | **service** | Idem |
| posmap, bytemap, word_sfxpost | **service** | Idem |
| filter_docs | **service** | Config de query |
| candidates, chains, matches | **arete** | Produit par un noeud, consomme par un autre |

Test : "si tu retires le noeud producteur, la donnee existe-t-elle encore ?" Oui = service. Non = arete.

## Observabilite : le DAG = l'explain

### Ce qui disparait

- `QueryTrace` a `LazyLock<Mutex<HashMap>>` global (trace.rs)
- Les ~12 `ctx.trace_msg()` manuels dans find_literal_v3
- Le bloc `if ctx.trace_id.is_some()` qui re-resout les postings juste pour logger
- Le double systeme trace/metrics

### Ce qui le remplace

Chaque noeud emet ses stats via `ctx.metric()` :
- `FstCandidates` : `metric("candidates_count", N)`
- `ResolveSingle` : `metric("matches", N)`
- `ChunkChain` : `metric("chains", N)`
- etc.

`tap_all()` collecte automatiquement. L'explain = rendu de `topological_levels()` + metrics par noeud :

```
FstCandidates        candidates=12       0.8ms
  |- ResolveSingle   matches=3           0.2ms
  |- SiblingChunk    sib_chains=5        1.1ms
  |- ChunkChain      chains=8            0.5ms
  |    \- ResolveChunk  matches=6        0.3ms
  |- SiblingWord     sib_chains=2        0.9ms
  |- WordChain       chains=4            0.4ms
  |    \- ResolveWord   matches=2        0.2ms
  \- Merge           results=9 (7 unique docs)  0.1ms
```

### Trace fine intra-noeud

Pas de canal special. Si un noeud a besoin de trace fine (ex: DFS des siblings), il l'emet via `ctx.info()` ou mieux, on decoupe le noeud plus finement pour que la topologie raconte l'histoire.

Philosophie : "arranger pour ne pas avoir besoin de trace intra-noeud = decouper assez finement pour que la structure soit la trace."

Si vraiment necessaire plus tard : un petit `ctx.trace_event(structured)` a ajouter a luciole. Mais pas maintenant.

## Plan d'implementation

### Phase 1 : SequentialRunner dans luciole

1. Ajouter `execute_sequential()` dans `luciole/src/runtime.rs` (ou nouveau fichier `sequential.rs`)
2. Tests : prendre un DAG existant (tests dag.rs), verifier meme resultats en sequentiel vs threade
3. ~30 lignes de code

### Phase 2 : Noeuds query pour find_literal_v3

1. Creer `src/suffix_fst/briques/dag_nodes.rs` — les 9 structs de noeuds, chacune impl Node
2. Chaque noeud : copier le corps de la fonction existante dans `execute()`
3. Les noeuds accedent au reader/resolver via `ctx.service::<BriquesServices>("briques")`

```rust
struct BriquesServices {
    reader: SfxFileReaderV3,  // ou reference via Arc
    resolver: Box<dyn PostingResolver>,
    posmap: Option<PosMapReader>,
    bytemap: Option<ByteBitmapReader>,
    word_sfxpost: Option<WordSfxPostReader>,
    sibling_v3: Option<SiblingTableReader>,
    termtexts: Option<TermTextsReaderV3>,
    filter_docs: Option<HashSet<DocId>>,
}
```

### Phase 3 : Construction du DAG

1. Creer `src/suffix_fst/briques/dag_builder.rs` — fonction qui construit le Dag selon le contexte
2. La forme depend de `strict_separators`, `has_word_pipeline()`, `has_sibling_chains()`
3. 4-5 formes possibles, memoisation optionnelle

```rust
pub fn build_literal_dag(
    query: &str,
    anchor_start: bool,
    strict_separators: bool,
    services: Arc<ServiceRegistry>,
) -> Dag {
    let mut dag = Dag::new().with_services(services);
    
    dag.add_node("fst_candidates", FstCandidatesNode::new(query, anchor_start));
    dag.add_node("resolve_single", ResolveSingleNode::new());
    dag.connect("fst_candidates", "candidates", "resolve_single", "candidates").unwrap();
    
    dag.add_node("chunk_chain", ChunkChainNode::new(query, anchor_start));
    // ... selon config, ajouter sibling, word, etc.
    
    dag.add_node("merge", MergeNode::new());
    // connecter toutes les branches au merge
    
    dag
}
```

### Phase 4 : Branchement + tests

1. `find_literal_v3` appelle `build_literal_dag()` + `execute_sequential()`
2. Verifier memes resultats sur tous les tests existants (`test_find_literal_*`)
3. Si OK, supprimer l'ancien corps imperatif
4. Supprimer QueryTrace (trace.rs) et les trace_msg manuels

### Phase 5 : Explain

1. Ajouter le trait `PortSummary` a luciole
2. Implanter sur Vec<CandidateV3>, Vec<ChainV3>, Vec<MatchV3>
3. Fonction `explain_dag()` qui rend le DAG annote apres execution
4. Brancher sur `V3_DIAG` / future commande `EXPLAIN LUCIVY_QUERY`

## Risques et garde-fous

| Risque | Garde-fou |
|---|---|
| Sur-decoupage des noeuds | Max ~10 noeuds par DAG. Si > 15, on sur-decoupe |
| Noeuds generiques / framework | Pas de trait Node custom, pas de registry dynamique. On utilise le Node de luciole tel quel |
| Regression perf | Bench avant/apres sur le ground truth 5K docs |
| Regression fonctionnelle | Les test_find_literal_* existants sont l'oracle |
| Lifetime issues (services via Arc) | BriquesServices possede ses donnees (pas de references) |

## Perimetre strict

**On fait :** find_literal_v3 seulement. Un prototype, un oracle, un verdict.

**On ne fait pas :** find_multi_token_v3, resolve_trigrams_v3, ni aucun autre orchestrateur. Si find_literal_v3 en DAG ne convainc pas (clarte, perf, explain), on a perdu une fonction, pas le moteur.

**Test de reussite :** "est-ce que la prochaine fee des bois comprend le chain building plus vite en lisant le DAG qu'en lisant les 110 lignes imperatives?"

## Liens

- Discussion archi : `docs/24-mai-2026-15h04/avis_claude.md` (lignes 580-864)
- Knowledge dump session 6 : `docs/24-mai-2026-15h04/12-knowledge-dump-session-6.md`
- Etat ground truth : 15/15 a 500 docs, 11/15 a 5000 docs (4 FN scale-dependent)
