# TODO : highlights contains sans lecture stored text

Date : 14 mars 2026

## Probleme

Le changement `if !self.needs_validation() && self.highlight_sink.is_none()` dans `contains_scorer.rs` forçait le slow path (lecture stored text) pour TOUTES les queries contains quand les highlights étaient demandés. Ça causait un ralentissement visible sur les gros index (4000+ docs).

Reverté car le coût est trop élevé. Les highlights multi-token startsWith passent maintenant par le path séparateurs (qui lit le stored text), mais les contains simples reprennent le fast path.

## Impact

- startsWith multi-token : highlights fonctionnent (via separators path)
- startsWith single-token : highlights fonctionnent (via FuzzyTermQuery/AutomatonWeight)
- contains multi-token SANS séparateurs : **pas de highlights** (fast path skip)
- contains multi-token AVEC séparateurs : highlights OK (validation path lit stored text)

## Piste d'optimisation

Collecter les byte offsets directement depuis les postings (WithFreqsAndPositionsAndOffsets) dans le fast path, sans lire le stored text. Les offsets sont déjà dans le posting list — on pourrait les extraire pendant le position intersection au lieu de relire le document.

Fichier : `src/query/phrase_query/contains_scorer.rs`, méthode `phrase_match()`.
