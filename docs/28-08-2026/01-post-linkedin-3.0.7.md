# Post LinkedIn — Lucivy 3.0.7

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
octets-là*. Et ce n'est pas une promesse : sur les **93 605 fichiers du noyau
Linux**, chaque réponse est comparée document par document **et span par span**
à une lecture brute des fichiers sur disque.

`sched` en sous-chaîne trouve **9 327 fichiers et 53 336 occurrences en 78 ms**.
Le scan naïf met **4 022 ms** à donner exactement la même réponse. En mot
entier, la même requête n'en trouve que 5 311 — la différence, ce sont les
`sched_clock`, `schedule`, `sched_domain`, que la plupart des moteurs ne
rendent jamais. Avec deux fautes de frappe, `regsiter` sort **267 348 spans
exacts en 879 ms**.


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

**À vérifier une fois la 3.0.7 en ligne**

- Les cinq liens ci-dessus doivent afficher **3.0.7**. `lucivy-wasm` et les
  crates sont publiés à la main, donc décalés de quelques minutes par rapport à
  PyPI/npm.
- Le playground sert le WASM commité dans `playground/pkg` : s'assurer que
  GitHub Pages a bien redéployé après le commit de release, sinon la page
  annonce la v3 en servant l'ancien binaire.
- Ne pas raccourcir les liens soi-même : LinkedIn le fait (`lnkd.in`) et
  compte le lien raccourci dans la limite de caractères.

**Le visuel**

Deux captures sont rangées dans `images/`, prises sur la page de présentation
le 28 août :

| fichier | ce que c'est |
|---|---|
| `images/bench-panel-verifie.png` | le panel des 93 605 fichiers, avec les colonnes lucivy (vert) et scan naïf (rouge) |
| `images/bench-navigateur-vs-natif.png` | la comparaison navigateur / natif sur 10 000 fichiers |

**La première est celle à mettre.** Elle montre en un coup d'œil ce qui fait la
différence : les deux lignes `sched` (5 311 documents en mot entier contre
9 327 en sous-chaîne), les 267 348 spans exacts, et surtout les deux colonnes
côte à côte — le lecteur voit qu'on a comparé à quelque chose, pas seulement
chronométré.

Restent possibles, si tu veux mieux : un **GIF du terminal** (il se lance seul,
montre le clone, l'indexation et les recherches avec leurs spans surlignés —
c'est la démonstration la plus courte de ce que fait le moteur), ou un **PDF
tiré du HTML** de la page pour un rendu net à l'impression. Aucun des deux
n'est fait.

**Un angle possible, à toi de trancher : dire qu'on a trouvé un bug**

Le panel des 93 605 fichiers n'a pas seulement produit des chiffres — il a
trouvé, dès son premier passage, un défaut de rappel vieux de cinq versions
publiées : en fuzzy relâché, un document dont l'unique occurrence approchée
enjambait des frontières de tokens n'était pas rendu du tout (corrigé en 3.0.7,
voir `02-fuzzy-perdait-des-documents.md`).

Le raconter est un pari. Le pour : ça prouve que la vérification n'est pas
décorative, et c'est exactement le genre d'honnêteté qui distingue un projet
sérieux d'une démo. Le contre : sur LinkedIn, « j'ai trouvé un bug chez moi »
se lit parfois comme « son moteur avait un bug » par qui ne lit qu'une ligne.

Si tu le mets, une phrase suffit, et elle doit porter sur la méthode, pas sur
la faute — par exemple : « Le premier passage de ce panel a trouvé un défaut de
rappel que cinq versions de tests avaient laissé passer. C'est pour ça qu'on ne
publie pas un temps sans la réponse qui va avec. »

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
> Lucivy 3.0.7 les trouve — sous-chaînes, fautes de frappe et regex à travers
> les frontières de tokens, avec les octets exacts qui ont matché. En Rust,
> Python, Node, C++ et dans le navigateur. MIT.
>
> Le playground indexe le code source de Lucivy dans votre onglet, sans rien
> installer : https://l-defraiteur.github.io/lucivy/
