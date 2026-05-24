# Design : QueryTrace — graphe de debug structuré pour les briques

## Problème

Les eprintln! et les flags V3_DEBUG donnent du texte plat sans structure.
Pour diagnostiquer les FN scale-dependent, on a besoin de tracer le
cheminement complet d'une query à travers les briques : quels splits
ont été trouvés, quels siblings ont été suivis, quels ordinals ont été
résolus, pourquoi un doc a été rejeté au resolve.

## Principe

Un **arbre d'événements** stocké dans le BriquesContext. Chaque brique
pousse des événements dans le trace. Le trace est sérialisable en JSON
pour analyse.

## Structure

```rust
/// Un noeud du trace. Chaque appel de brique crée un noeud.
pub struct TraceNode {
    /// Label de l'opération ("find_literal_v3", "falling_walk_words", etc.)
    pub label: String,
    /// Données clés de l'opération (query, ordinals, counts...)
    pub data: HashMap<String, TraceValue>,
    /// Noeuds enfants (opérations appelées par celle-ci)
    pub children: Vec<TraceNode>,
}

pub enum TraceValue {
    Str(String),
    Num(i64),
    List(Vec<TraceValue>),
}
```

## Le problème du transport entre niveaux

Les briques s'appellent en cascade :
```
contains_v3 → find_literal_v3 → falling_walk_words → walk_partition
                               → sibling_chain_dfs
                               → resolve_word_chains_v3 → intermediates_are_pure_sep
```

Comment une fonction bas-niveau (intermediates_are_pure_sep) sait-elle
dans quel noeud du trace elle doit écrire ?

### Option A : TraceHandle via le context

Le BriquesContext contient un `trace: Option<QueryTrace>`. Chaque brique
fait :
```rust
let handle = ctx.trace_enter("falling_walk_words");
// ... do work ...
handle.set("num_splits", splits.len());
handle.exit(); // ferme le noeud
```

`trace_enter` pousse un nouveau noeud enfant dans l'arbre et retourne un
handle qui pointe vers ce noeud. Les fonctions enfants utilisent le même
ctx, donc elles pushent dans le bon sous-arbre.

Problème : le ctx est `&` (shared ref), pas `&mut`. On ne peut pas
muter l'arbre via une shared ref.

Solution : `RefCell<TraceTree>` dans le context. Ou mieux :

### Option B : TraceWriter thread-local

Un `thread_local!` qui stocke le trace courant. Les briques font :
```rust
trace_push!("falling_walk_words", "query" => query);
// ... do work ...
trace_set!("num_splits", splits.len());
trace_pop!();
```

Avantage : aucun changement de signature. Aucune ref mut.
Inconvénient : global, pas par-query. Mais en mode debug, on ne
trace qu'une query à la fois.

### Option C : TraceId dans le context (recommandé)

Le context a un `trace_id: Option<u64>`. Quand debug=true, un trace_id
est assigné. Les briques passent le trace_id aux fonctions bas-niveau.
Un store global `HashMap<u64, TraceTree>` stocke les traces.

```rust
// Dans BriquesContext
pub trace_id: Option<u64>,

// Macros pour les briques
macro_rules! trace {
    ($ctx:expr, $label:expr, $($key:expr => $val:expr),*) => {
        if let Some(tid) = $ctx.trace_id {
            TRACE_STORE.with(|s| {
                let mut s = s.borrow_mut();
                let tree = s.entry(tid).or_default();
                tree.push($label, &[$(($key, $val)),*]);
            });
        }
    }
}
```

Les fonctions qui n'ont pas le ctx (comme walk_partition) reçoivent
le trace_id en paramètre. C'est un seul u64, pas un gros struct.

## Usage dans les briques

```rust
pub fn find_literal_v3(ctx: &BriquesContext, query: &str, ...) {
    trace!(ctx, "find_literal_v3", "query" => query);

    let candidates = fst_candidates_v3(...);
    trace!(ctx, "fst_candidates", "count" => candidates.len());

    let splits = falling_walk_words(...);
    trace!(ctx, "falling_walk_words", "splits" => splits.len());

    for split in &splits {
        trace!(ctx, "split",
            "ord" => split.parent.raw_ordinal,
            "consumed" => split.query_consumed,
            "remainder" => &query[split.remainder_start..]);
    }

    let sib_chains = sibling_chain_dfs(&splits, ...);
    trace!(ctx, "sibling_chain_dfs", "chains" => sib_chains.len());

    // resolve
    for (doc_id, ...) in &active {
        trace!(ctx, "resolve_step",
            "doc" => doc_id,
            "prev_pos" => prev_last_pos,
            "valid" => valid);
    }
}
```

## Sortie

Quand le trace est terminé, on peut :
1. Sérialiser en JSON : `/tmp/v3_trace_{query}.json`
2. Afficher en arbre indenté (comme le diag graph montré plus haut)
3. Filtrer par doc_id pour voir pourquoi un doc spécifique a été rejeté

## Relation avec V3_DIAG

```
V3_DIAG=1 :
  Pass 1 : ground truth normal → fails.json
  Pass 2 : pour chaque fail, relance avec trace_id → trace JSON
  Analyse : le trace montre exactement où le pipeline a divergé
```

## Fichiers

| Fichier | Contenu |
|---------|---------|
| `briques/trace.rs` | TraceTree, TraceNode, macros, thread-local store |
| `briques/context.rs` | Ajout trace_id: Option<u64> |
| `briques/composite.rs` | trace!() dans find_literal_v3 |
| `briques/fst_walk.rs` | trace!() dans sibling_chain_dfs |
| `briques/resolve.rs` | trace!() dans resolve_word_chains_v3 |

## Priorité

Ce n'est pas un bloquant pour le 15/15. C'est un outil de debug pour
investiguer les FN scale-dependent. On peut :
1. L'implémenter minimal (juste les events clés dans find_literal_v3)
2. L'étendre au fur et à mesure qu'on investigue
3. Le désactiver en prod (trace_id = None → les macros sont no-op)
