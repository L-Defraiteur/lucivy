Ca vous tente une petite demonstration de magie noire? :)

Recherche regex sur 5 000 fichiers en 22ms. Dans le navigateur. Sans serveur.

Je travaille sur la v2 de lucivy, mon moteur de recherche full-text en Rust. Voici ce qui arrive bientot.

La plupart des moteurs de recherche traitent la regex comme un dernier recours — scanner chaque document, prier pour que ca finisse avant le timeout. On a pris une autre approche.

`rag3.*ver` — trouver tout ce qui contient "rag3" suivi de "ver", avec n'importe quoi entre les deux. Cross-token, cross-mot. 20 resultats, classes par pertinence BM25. **22 millisecondes.**

Ce qui se passe sous le capot :

**1. Zero scan exhaustif.** La regex est decomposee en fragments litteraux ("rag3", "ver"). Chaque litteral est resolu via le suffix FST en O(resultats), pas O(taille_index).

**2. Intersection multi-litterale.** Les documents contenant les deux litteraux sont intersectes par position — les offsets d'octets confirment que "rag3" apparait avant "ver" dans le texte.

**3. Validation DFA entre positions.** Un PosMap (index inverse position-vers-ordinal) parcourt l'espace entre les litteraux, en nourrissant le DFA de la regex octet par octet. Retour immediat des que le DFA accepte.

**4. Scoring BM25 reel.** Pas un score plat. Frequence du terme, frequence documentaire, normes de champ — la formule complete, avec consistance cross-shard via aggregation prescan.

**5. Tourne entierement en WebAssembly.** Le moteur compile en WASM, tourne dans l'onglet du navigateur. Zero latence reseau. Votre code ne quitte jamais votre machine.

Le meme moteur gere la recherche par sous-chaine, le fuzzy matching (automates de Levenshtein), et maintenant la regex — le tout a travers la meme infrastructure de suffix FST.

C'est lucivy — le moteur de recherche full-text que je construis, dans sa version qui va sortir bientot!

---

#lucivy #v2 #searchengine #regex #fst #suffixarray #bm25 #wasm #webassembly #fulltext #rust #opensource
