# Page de présentation — plan de design (26 août 2026)

Vision de Lucie : on arrive, on voit un **terminal** un peu hacker, avec des
symboles glitchés ; un bouton **Play demo** lance, en direct et comme dans une
console, l'indexation du code de lucivy puis des recherches qui se déroulent
sous les yeux ; au-dessus, un pitch avec deux boutons : **Play demo** et
**Playground**.

Principe qui rend ça fort et pas kitsch : **rien n'est simulé**. Le terminal
affiche ce que le worker fait vraiment (clone GitHub, indexation, commits,
préchargement, puis chaque requête avec son chrono réel et ses vrais
résultats). Les symboles glitchés sont l'habillage, pas le contenu.

## 1. Structure de la page (une seule page statique, `playground/index.html`)

```
┌───────────────────────────────────────────────────────────────┐
│  lucivy                                    GitHub · npm · PyPI │
│                                                                │
│  Search code the way you grep it.                              │  ← pitch, 2 lignes
│  Substrings across tokens, fuzzy, regex, BM25, exact spans —   │
│  in Rust, and in your browser.                                 │
│                                                                │
│        [ ▶ Play demo ]        [ Open the playground ]          │
│                                                                │
│  ┌──────────────────────── terminal ────────────────────────┐  │
│  │ $ lucivy clone L-Defraiteur/lucivy@main                   │  │
│  │ ▸ 1101 files · 4.3 MB                                     │  │
│  │ $ lucivy index /lucivy_source          ████████░░ 82 %    │  │
│  │ ▸ committed 1000 · 2 segments · 3.1 s                     │  │
│  │ $ lucivy search "lock_init"                               │  │
│  │ ▸ 8 hits · 12 ms · spin_[lock_init](&adapter->lock)       │  │
│  │ ...                                                       │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                │
│  What you just saw (3 cartes : substring · fuzzy · regex)      │
│  Numbers (natif / navigateur, tableau du README)               │
│  Install (Rust · Python · Node · Browser, 5 lignes chacun)     │
│  How it works (schéma : tokens → suffix FST → sibling → verify)│
│  Honest limits · License · Links                               │
└───────────────────────────────────────────────────────────────┘
```

Le playground actuel (modes, séparateurs, limite, extension, import de
fichiers, clone d'un repo) devient une **seconde vue** de la même page
(`#playground`, ou `?playground`), atteinte par le second bouton ou à la fin
de la démo (« Try your own query → »). Même worker, même index : pas de
rechargement, pas de seconde indexation.

## 2. Le terminal

**Habillage.** Fond quasi noir, police mono, curseur qui clignote, une fine
ligne de scan très discrète, et les « glitchs » : au repos (avant Play), des
caractères qui se décalent/se remplacent aléatoirement dans un bloc
d'en-tête (`▚▞ lucivy ▞▚`, `01001…`, quelques `∿ ⌁ ⟁`) — 2-3 % des
caractères, jamais sur le texte utile, désactivé si
`prefers-reduced-motion`. Pendant la démo, les glitchs se calment : le vrai
contenu prend la place.

**Contenu, dans l'ordre, tout réel :**

1. `$ lucivy clone L-Defraiteur/lucivy@main` — lignes de progression du
   proxy (fichiers listés, Mo), source : `cloneAndIndex`.
2. `$ lucivy index /lucivy_source` — barre de progression **réelle**
   (`Indexing … N/1101`), commits (`committed 1000 · 2 segments`), temps
   final, taille de l'index (`memoryStatus.index_bytes`), préchargement.
   Sur un index déjà présent dans OPFS (visite suivante) : `$ lucivy open
   /lucivy_source` → `1101 docs · 271 MB · opened in 0.4 s` puis on enchaîne
   (et un lien « re-index from GitHub »).
3. **Séquence de recherches scriptée**, une toutes les ~1,5 s, chacune
   exécutée pour de vrai via `window._playground.search`, affichée comme
   une commande + ses premiers résultats avec **les spans surlignés dans le
   texte réel** :
   - `search "lock_init"` — substring qui traverse les tokens
     (`spin_lock_init`) ;
   - `search "rag3weaver"` — séparateurs relaxés (`rag3_weaver`,
     `rag3-weaver`) ;
   - `search --fuzzy 1 "ShardedHandel"` — faute corrigée, palier de score ;
   - `search --regex "Sharded[A-Z]\w+"` — regex à travers les tokens ;
   - `search "wait_merges AND NOT compact"` — syntaxe booléenne ;
   - `search --allowed 12 ids "kmalloc"` (si on a envie de montrer le
     pré-filtre : `4 ms`).
   Chaque ligne de résultat : `N hits · T ms` avec le T mesuré par la page.
4. Fin : `$ _` avec « Try your own query → » qui bascule sur le playground
   avec l'index chaud.

**Rythme.** La démo dure 30-40 s la première fois (indexation ~15-25 s sur
PC, l'indexation *est* le spectacle), 10 s ensuite (index en OPFS). Bouton
**Skip to the searches** pendant l'indexation, **Replay** à la fin. Les
frappes de commandes sont animées (typewriter, 20 ms/caractère) ; les
sorties arrivent quand elles arrivent — pas de délai artificiel.

**Mobile.** Même page, mêmes réglages « petit appareil » (2 threads, 1
build) ; le terminal passe en pleine largeur, police 11 px, la séquence de
recherches est réduite à trois. On garde le vrai comportement : ça indexe
en ~30 s sur un tel, on l'a mesuré.

## 3. Le pitch

Deux lignes, factuelles, pas de superlatif :

> **Search code the way you grep it.** Substrings across tokens, fuzzy
> (Levenshtein or Jaro-Winkler), regex, BM25 ranking, exact byte spans —
> a Rust engine with Python, Node.js and C++ bindings, that also runs in
> your browser.

Sous les boutons, une ligne grise : « Every number on this page is
measured in your tab, right now. »

## 4. Sous le terminal

- **What you just saw** — trois cartes courtes (substring cross-token ·
  fuzzy verified on the text · regex driven by its literals), chacune avec
  l'exemple de la démo et une phrase sur *pourquoi* c'est difficile.
- **Numbers** — la table native/navigateur du README (10k kernel, 21
  requêtes) et la ligne 50k ; lien BENCHMARKS.md « reproduce it ».
- **Install** — quatre onglets, cinq lignes chacun (Rust `cargo add
  lucivy-core`, `pip install lucivy`, `npm install lucivy`,
  `npm install lucivy-wasm`), et le lien C++.
- **How it works** — un schéma SVG inline (texte → tokens avec overlap →
  suffix FST partitionné → sibling table → vérification sur le texte →
  spans), lien ARCHITECTURE.md.
- **Honest limits** — binaires précompilés Linux x86_64, 4 Go par onglet,
  ~12 000 fichiers kernel maxi en mémoire dans le navigateur, mainteneuse
  unique, héritage tantivy assumé (ce qui est dérivé, ce qui est nouveau).
- Pied : GitHub (bouton étoile), npm, PyPI, crates.io, MIT.

## 5. Contraintes techniques (pour ne rien casser)

- Statique, sans build, Pages tel quel ; `index.html` + `js/` + `pkg/` ;
  cache-buster `?v=` inchangé.
- Le terminal est un `<pre>` alimenté par les mêmes hooks que la page
  actuelle (`statusEl` → on s'abonne aux mêmes messages, pas de second
  canal), plus `window._playground.search` pour les requêtes.
- Le worker et l'index sont partagés entre la vue « landing » et la vue
  « playground » : un seul `Lucivy(...)`.
- `prefers-reduced-motion` : pas de glitch, pas de typewriter, le contenu
  s'affiche d'un bloc.
- Rien ne se lance avant **Play** (pas de clone au chargement) : la page
  d'accueil doit coûter zéro tant qu'on n'a pas cliqué — sauf si l'index
  est déjà dans OPFS, auquel cas on l'ouvre en silence pour que le
  playground soit prêt.

## 6. Ce qui est à décider par Lucie

- Le pitch exact (deux lignes) et le nom des boutons.
- Le style des glitchs (discret / assumé) et la palette (le dark actuel
  GitHub-like, ou plus « CRT » vert/ambre).
- La liste finale des 4-6 requêtes de la démo (celles ci-dessus sont des
  propositions qui marchent sur le code de lucivy).
- Une capture/GIF de la démo pour le README et les posts (on peut
  l'enregistrer une fois la page faite).

## 7. Ordre de réalisation (une journée)

1. Vue landing + terminal (habillage, hooks sur les messages existants,
   séquence de recherches) — le gros du travail.
2. Bascule landing ⇄ playground sans recharger.
3. Sections statiques (copier depuis README/BENCHMARKS).
4. Mobile + reduced-motion + test sur le tel de Lucie.
5. GIF pour le README.
