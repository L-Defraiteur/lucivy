# Post LinkedIn — Lucivy 3.0.6

Brouillon pour l'annonce de la v3. Le post lui-même est entre les deux
séparateurs ; ce qui suit est la note d'accompagnement (liens à vérifier,
visuel, variantes).

---

Il y a quelques mois, j'ai ouvert le code de **Lucivy**, un moteur de recherche
full-text BM25 écrit en Rust. Aujourd'hui je publie la **v3** — et c'est une
réécriture du cœur, pas une version d'entretien.

Le problème de départ n'a pas changé. La plupart des moteurs découpent le texte
en tokens et comparent des mots entiers. Cherchez `mutex` : vous trouvez le mot
`mutex`, mais pas `pthread_mutex_lock`, pas `getMutexHandle`, pas `lockmutex`.
Cherchez une phrase avec une faute de frappe, elle disparaît. Pour du texte
courant ça passe. Pour **du code, des messages d'erreur, des clés de config**,
c'est exactement ce qu'on cherche qu'on ne trouve pas.

Lucivy indexe un **Suffix FST** : chaque suffixe de chaque token, partitionné
selon l'endroit où il commence. La recherche de sous-chaîne devient aussi
précise qu'une recherche exacte, et elle traverse les frontières de mots.

**Ce que la v3 apporte :**

→ **Des spans d'octets exacts**, sur tous les modes de requête — substring,
fuzzy, regex, phrase. Pas « ce document contient votre requête » : *ces
octets-là*. Vérifiés un par un contre `grep`, sur 50 000 fichiers du noyau
Linux. C'est le test qui garde le moteur honnête.

→ **Une syntaxe booléenne** : `kmalloc AND NOT kfree`, `"phrase exacte"`,
`+obligatoire -exclu`, parenthèses — le tout abaissé en requêtes de
sous-chaînes, avec les highlights.

→ **Fuzzy au choix, Levenshtein ou Jaro-Winkler.** Une faute en fin de mot ne
compte plus autant qu'une faute au début.

→ **Apportez votre propre stockage.** Votre objet implémente `load` / `save` /
`delete` / `exists` / `list`, et le moteur tourne dessus : une base
transactionnelle devient la source de vérité, le cache mmap redevient jetable.
Disponible dans les trois bindings natifs.

→ **Ça tourne dans le navigateur.** 10 000 fichiers du noyau indexés en 55 s,
et les requêtes répondent à ~1,5× le temps natif. C'était 25 minutes et ×10.

→ **Recherche fédérée.** Chaque machine exporte ses statistiques, un
coordinateur les fusionne, et chaque nœud score sur le corpus de la fédération —
sans rien copier ni monter. Un test vérifie que l'union de deux nœuds rend
exactement ce que rendrait un index unique, **aux mêmes scores**.

**Essayez-le, il n'y a rien à installer :** le playground clone le code source
de Lucivy depuis GitHub et l'indexe dans votre onglet, en quelques secondes.
Puis vous tapez vos propres requêtes.
https://l-defraiteur.github.io/lucivy/

Je l'ai construit parce qu'il me fallait le versant BM25 de la recherche
vectorielle dans rag3db, un moteur RAG sur lequel je travaille aussi. Le
sémantique est excellent pour « trouve-moi quelque chose à propos de X ». Quand
l'utilisateur cherche un nom de fonction précis, il faut autre chose.

**Ça s'installe partout :**

- Python : `pip install lucivy` — https://pypi.org/project/lucivy/
- Node.js : `npm install lucivy` — https://www.npmjs.com/package/lucivy
- Navigateur : `npm install lucivy-wasm` — https://www.npmjs.com/package/lucivy-wasm
- Rust : `cargo add lucivy-core` — https://crates.io/crates/lucivy-core
- C++ : bibliothèque statique via un bridge CXX

Binaires précompilés pour Linux (x86_64, aarch64), macOS (Intel, Apple Silicon)
et Windows x86_64.

Code, issues et PRs : https://github.com/L-Defraiteur/lucivy
Licence MIT. Fork de tantivy v0.26.0.

Si vous intégrez de la recherche dans un produit, un pipeline RAG ou un outil de
dev — testez-le et dites-moi ce qui casse.

#opensource #search #rust #python #nodejs #wasm #bm25 #fulltext #rag

---

## Notes avant publication

**À vérifier une fois la 3.0.6 en ligne**

- Les cinq liens ci-dessus doivent afficher **3.0.6**. `lucivy-wasm` et les
  crates sont publiés à la main, donc décalés de quelques minutes par rapport à
  PyPI/npm.
- Le playground sert le WASM commité dans `playground/pkg` : s'assurer que
  GitHub Pages a bien redéployé après le commit de release, sinon la page
  annonce la v3 en servant l'ancien binaire.
- Ne pas raccourcir les liens soi-même : LinkedIn le fait (`lnkd.in`) et
  compte le lien raccourci dans la limite de caractères.

**Le visuel**

Le post porte mieux avec une image. Deux options, par ordre d'efficacité :

1. **Un GIF du terminal de la page de présentation** — il se lance seul, montre
   le clone, l'indexation avec sa vraie progression, puis les recherches avec
   leurs temps mesurés et les spans surlignés. C'est la démonstration la plus
   courte de ce que fait le moteur. À enregistrer (c'est le point 5.4 du
   rapport du 27, toujours ouvert).
2. À défaut, `docs/25-08-2026/playground_screenshot.jpg`, déjà dans le README.

**Ce que le post ne dit pas, volontairement**

Les chiffres de sélectivité du filtre sparse (540 000 ids passés de 6,0 ms à
0,22 ms) et le passage à un index segmenté sont dans le CHANGELOG. Ils
intéressent les gens qui construisent un moteur RAG, pas le lecteur LinkedIn qui
découvre le projet — et ils déclencheraient des questions sur `sparse-vector`,
qui est un crate ami et pas le sujet de l'annonce.

Pas de comparaison chiffrée avec un autre moteur non plus. `bench_vs_tantivy.rs`
existe, mais publier un ratio demande une machine au repos et deux runs, et la
leçon des deux rétractations du 27 août tient : **un bench sur machine chargée
mesure la charge**.

**Variante courte** (si tu veux tester l'accroche seule)

> Cherchez `mutex` dans votre moteur de recherche. Il trouve le mot `mutex`.
> Pas `pthread_mutex_lock`, pas `getMutexHandle`, pas `lockmutex`.
>
> Lucivy 3.0.6 les trouve — sous-chaînes, fautes de frappe et regex à travers
> les frontières de tokens, avec les octets exacts qui ont matché. En Rust,
> Python, Node, C++ et dans le navigateur. MIT.
>
> Le playground indexe le code source de Lucivy dans votre onglet, sans rien
> installer : https://l-defraiteur.github.io/lucivy/
