# Faire connaître lucivy — stratégie, canaux, séquence

28 août 2026. Écrit après la publication de 3.0.7. Ce document n'est pas une
liste de sites où poster : c'est un ordre de priorité, avec ce que chaque canal
récompense et ce qu'il punit.

**Note d'honnêteté** : les usages des communautés changent, et mes
connaissances s'arrêtent à mai 2026. Les règles de chaque endroit sont à
revérifier avant de poster — surtout les règles d'autopromotion, qui sont ce
qui fait bannir.

---

## 1. Ce que tu as vraiment, et ce que tu n'as pas

Se le dire d'abord, parce que toute la suite en découle.

**Les atouts, par ordre de force :**

1. **Une démonstration qui tourne dans l'onglet du lecteur, sans installer
   quoi que ce soit.** C'est rarissime pour un moteur de recherche, et ça
   supprime le principal frein : « je verrai plus tard ». Le playground clone
   ton propre code et l'indexe en quelques secondes.
2. **Une capacité que les autres n'ont pas** — sous-chaînes, fuzzy et regex à
   travers les frontières de tokens, classées BM25, avec les octets exacts.
   Elle se démontre en une requête (`sched` : 5 311 documents en mot entier,
   9 327 en sous-chaîne).
3. **Une histoire technique vraie et racontable** : un panel de vérité terrain
   qui a trouvé, à son premier passage, un défaut de rappel que cinq versions
   publiées avaient laissé passer. C'est ton meilleur article, et de loin.
4. **Cinq plateformes précompilées, cinq langages, MIT.** Rien à négocier.

**Ce qui manque, et qui coûte des lecteurs :**

- **Aucune preuve d'usage.** Pas d'utilisateur nommé, pas de projet tiers, pas
  d'étoile en nombre. Le premier réflexe d'un lecteur est « qui s'en sert ? ».
- **Aucune comparaison chiffrée avec une alternative connue.**
  `bench_vs_tantivy.rs` existe mais n'a jamais été publié. Or « en quoi c'est
  différent de X » est la première question, partout.
- **Le README est dense.** Il est excellent pour qui décide déjà d'évaluer ;
  il est décourageant pour qui survole. Il manque, tout en haut, trois lignes
  qui disent à qui ça s'adresse.
- **Le lien avec tantivy est un sujet à traiter, pas à éviter** (voir §5).

---

## 2. Le principe qui commande tout le reste

**Un projet technique ne se popularise pas en s'annonçant, mais en étant
utile une fois trouvé.** L'annonce apporte un pic de trois jours ; ce qui
reste, c'est ce qui répond à une recherche six mois plus tard.

Concrètement, ça veut dire que l'ordre suivant est délibéré :

1. d'abord rendre la page d'arrivée convaincante (§3),
2. ensuite écrire ce qui a de la valeur en soi (§4),
3. **et seulement après** aller vers les communautés (§6).

Poster sur Hacker News avant d'avoir §3 et §4, c'est dépenser sa seule
cartouche. On ne fait pas deux fois un « Show HN » sur le même projet.

---

## 3. Ce qu'il faut réparer avant toute annonce

Par ordre de rentabilité.

**Trois lignes en haut du README qui disent à qui ça s'adresse.** Aujourd'hui
il démarre sur ce que fait le moteur. Il devrait démarrer sur *qui* en a
besoin : « si vous cherchez du code, des messages d'erreur ou des identifiants
dans un index, et que votre moteur vous rend zéro sur `getMutexHandle` ».

**Une section « en quoi c'est différent de… »**, honnête, avec les cas où les
autres sont meilleurs. Une comparaison qui ne perd jamais n'est pas lue comme
une comparaison, elle est lue comme une publicité :

| | ce qu'ils font mieux | ce que lucivy fait qu'ils ne font pas |
|---|---|---|
| ripgrep / grep | plus simple, aucun index, mémoire nulle | classement BM25, réponse en ms sur un gros corpus |
| tantivy / Lucene | écosystème, maturité, agrégations | sous-chaîne et fuzzy **dans** les tokens |
| Meilisearch / Typesense | prêt à l'emploi, UI, tolérance aux fautes soignée | regex, spans exacts, cross-token, embarquable |
| Zoekt | conçu pour la recherche de code à grande échelle | BM25 sur sous-chaînes, spans exacts partout, navigateur |

**Publier `bench_vs_tantivy`** — sur machine au repos, deux passes, avec la
vérité terrain. Y compris là où tantivy gagne. C'est le chiffre que tout le
monde demandera, et le publier soi-même vaut mieux que le voir produit par
quelqu'un d'autre dans un commentaire.

**Un GIF de la démo dans le README.** Il existe déjà en projet et n'a jamais
été fait. Sur GitHub, un GIF au-dessus du pli vaut trois paragraphes.

---

## 4. Écrire ce qui a de la valeur en soi

C'est le levier le plus fort et le plus lent. Un bon article technique amène
des lecteurs pendant des années ; une annonce, trois jours.

### 4.1 L'article à écrire en premier

**« Notre benchmark mesurait une réponse que personne n'avait vérifiée. »**

L'histoire complète est déjà écrite dans
[`02-fuzzy-perdait-des-documents.md`](02-fuzzy-perdait-des-documents.md) : un
bench qui affichait « 20 hits » sur chaque ligne parce que 20 était le plafond,
un panel de vérité terrain écrit pour le remplacer, un défaut de rappel trouvé
au premier passage, deux hypothèses fausses éliminées par la mesure, et une
cause qui tient en une phrase.

Pourquoi celui-là plutôt qu'un article sur le Suffix FST : parce qu'il parle à
**tout le monde qui écrit des benchmarks**, pas seulement à qui s'intéresse à
la recherche full-text. C'est un article sur la méthode, illustré par un moteur
de recherche — pas l'inverse. Ce genre de texte circule.

Et il fait la démonstration la plus difficile à faire en publicité : que tu
mesures honnêtement. Personne ne croit un projet qui dit « nous sommes
rigoureux » ; tout le monde croit un projet qui raconte son propre bug.

### 4.2 Les suivants, par ordre d'intérêt

- **« Chercher une sous-chaîne à travers les frontières de tokens »** — le
  Suffix FST, la table de siblings, pourquoi un index n-grammes explose. Le
  cœur technique, pour un public plus étroit mais exactement le bon.
- **« Un moteur de recherche complet dans un onglet »** — l'histoire WASM :
  25 minutes ramenées à 55 s, la limite des 4 Go, pourquoi aucun `thread::spawn`.
  Ce sujet intéresse bien au-delà de la recherche.
- **« Le BM25 correct entre machines »** — la fédération, avec le test qui
  prouve que l'union de deux nœuds égale un index unique **aux mêmes scores**.

Où les publier : sur ton propre domaine si tu en as un (le trafic te reste),
sinon GitHub Pages à côté du playground. **Pas sur une plateforme fermée** —
un article sur Medium derrière un mur de connexion perd la moitié de ses
lecteurs et tout son référencement.

---

## 5. Le sujet tantivy, à traiter de front

lucivy est un fork de tantivy v0.26.0. C'est à la fois ton **meilleur public**
(les gens qui connaissent tantivy comprennent immédiatement ce que tu apportes)
et ton **plus gros risque de dérapage** : rien ne se retourne plus vite qu'un
fork perçu comme ingrat ou opportuniste.

Les règles que je te conseille de tenir, sans exception :

- **Créditer haut et clair, pas en note de bas de page.** « Fork de tantivy
  v0.26.0 » doit être visible dans le README et dans chaque annonce.
- **Ne jamais formuler « mieux que tantivy ».** Formuler « ce que tantivy ne
  fait pas, par conception ». C'est vrai, c'est plus précis, et ça ne cherche
  pas la bagarre.
- **Dire ce que tu as retiré ou cassé** par rapport à l'original — la dette de
  compatibilité est une information que les évaluateurs cherchent.
- Si tu postes sur un canal fréquenté par les mainteneurs de tantivy, **ne pas
  les mentionner pour attirer leur attention**. S'ils viennent, tant mieux.

---

## 6. Les canaux, par ordre de passage

### 6.1 Hacker News — « Show HN »

Le plus fort potentiel, et **une seule cartouche**. À ne tirer qu'une fois §3
et §4.1 faits.

- Titre : `Show HN: Lucivy – substring, fuzzy and regex search across token
  boundaries, in Rust`. Factuel, pas de superlatif, pas de point
  d'exclamation. HN sanctionne le ton promotionnel plus durement que le fond.
- Premier commentaire par toi, immédiatement : pourquoi tu l'as construit, ce
  que ça ne fait pas, et le lien du playground.
- **Reste disponible trois à quatre heures.** Un Show HN se joue dans les
  commentaires. Une question sans réponse tue un fil.
- Le jour et l'heure comptent (milieu de semaine, matinée côté États-Unis),
  mais moins que la qualité de la page d'arrivée.
- Attends-toi à : « en quoi c'est différent de X », « pourquoi pas un index
  n-grammes », « quel est le coût en taille d'index ». Prépare ces trois
  réponses **par écrit** avant de poster.

### 6.2 Reddit

Chaque sous-communauté a ses règles d'autopromotion — les lire avant, elles
sont appliquées.

| endroit | angle à prendre | remarque |
|---|---|---|
| **r/rust** | le projet lui-même, technique | le plus accueillant pour un projet Rust ; poste le mercredi si un fil hebdo existe |
| **r/programming** | l'**article**, pas le dépôt | un lien de projet y passe mal, un bon article y passe bien |
| **r/opensource** | l'annonce de version, MIT | audience plus large, moins technique |
| **r/LocalLLaMA**, **r/Rag** | le versant BM25 d'un pipeline RAG hybride | public réellement en demande, à ne pas négliger |
| **r/searchengines**, **r/elasticsearch** | la comparaison honnête | petit mais très ciblé |

**Ne pas poster partout le même jour.** Le cross-post simultané est le
comportement le plus repérable, et il est puni sur Reddit comme sur HN. Un
canal par jour, avec un angle différent à chaque fois.

### 6.3 Les canaux Rust spécifiquement

- **This Week in Rust** — accepte les propositions par pull request sur leur
  dépôt. C'est presque toujours accepté pour une sortie de crate, ça coûte dix
  minutes, et ça touche exactement le bon lectorat. **À faire en premier, avant
  même HN** : c'est sans risque et sans concurrence d'attention.
- **Lobsters** — audience de très bonne qualité, mais sur invitation. Si
  quelqu'un t'invite, garde-le pour l'article, pas pour l'annonce.
- **Le forum users.rust-lang.org**, section annonces.
- **Les listes « awesome »** : `awesome-rust`, et les listes de recherche
  full-text. Une PR par liste, avec une ligne descriptive honnête.

### 6.4 Infolettres et podcasts

Elles cherchent en permanence de la matière, et une soumission coûte cinq
minutes :

- **Console.dev** — spécialisée dans les outils pour développeurs, formulaire
  de soumission ouvert. Très bon ajustement.
- **TLDR**, **Hacker Newsletter**, **Pointer** — reprennent souvent ce qui
  monte sur HN ; l'ordre naturel est donc HN d'abord.
- **Changelog** et **Rustacean Station** (podcasts) — acceptent des
  propositions d'invités. L'histoire du bug trouvé par la vérité terrain est un
  **excellent sujet d'épisode** : c'est une histoire, pas une démonstration de
  produit.

### 6.5 Là où sont les gens qui en ont besoin aujourd'hui

Le plus rentable et le plus lent : **répondre à des questions réelles**.

Cherche, sur Stack Overflow, GitHub Issues et les forums Elasticsearch, les
gens qui demandent « comment chercher une sous-chaîne dans un token »,
« wildcard search performance », « fuzzy search across word boundaries ». Ils
existent en nombre, la question est ancienne, et les réponses actuelles sont
toutes des contournements.

**Répondre en résolvant leur problème d'abord**, en mentionnant lucivy ensuite
et seulement si c'est réellement la réponse. Une réponse utile qui cite ton
projet vaut cent liens postés ; une réponse qui ne sert qu'à citer ton projet
te fait bannir et laisse une trace.

---

## 7. Séquence proposée

Rien n'oblige à tout faire, mais l'ordre compte.

**Semaine 1 — réparer la page d'arrivée**
Les trois lignes en tête du README, la section comparative, le GIF.
Publier `bench_vs_tantivy` sur machine au repos.

**Semaine 2 — LinkedIn et This Week in Rust**
Le post est prêt ([03](03-post-linkedin-court.md)). TWiR par PR. Les deux sont
sans risque : ils touchent ton réseau et le lectorat Rust, sans consommer la
cartouche HN.

**Semaine 3 — l'article sur le benchmark**
Écrit, relu, publié sur ton domaine. Puis r/programming et r/rust, à deux
jours d'écart.

**Semaine 4 — Show HN**
Seulement si les semaines précédentes ont tenu : la page convainc, l'article
existe, la comparaison est publiée. Bloque une demi-journée pour répondre.

**Ensuite, en continu** — les réponses aux questions réelles (§6.5), les listes
awesome, les infolettres, et un article de temps en temps.

---

## 8. Ce qu'il ne faut pas faire

- **Acheter de l'attention** (posts sponsorisés, services d'étoiles). Sur un
  public technique, ça se voit et ça détruit la crédibilité pour longtemps.
- **Annoncer partout le même jour.** Repérable, puni, et ça gaspille les
  canaux.
- **Publier un ratio de performance non reproduit.** Tu as déjà retiré deux
  chiffres faux cette semaine ; le troisième serait retenu contre toi. Chaque
  chiffre publié doit avoir sa commande de reproduction et ses conditions.
- **Comparer sans dire où tu perds.** Le lecteur technique cherche la faille de
  la comparaison ; s'il n'en trouve aucune, il conclut que la comparaison est
  malhonnête, pas que le produit est parfait.
- **Répondre sèchement à une critique.** Un fil de commentaires est lu par cent
  fois plus de gens qu'il n'en participe. Le ton de tes réponses **est** le
  produit, pour eux.

---

## 9. Ce qu'il faut regarder pour savoir si ça marche

Pas les étoiles GitHub : elles mesurent l'attention, pas l'usage.

| indicateur | où | ce qu'il dit |
|---|---|---|
| **Téléchargements PyPI / npm / crates.io** hors semaine d'annonce | les registres | l'usage réel, seul chiffre qui compte |
| **Issues ouvertes par des tiers** | GitHub | quelqu'un l'a assez utilisé pour buter dessus — le meilleur signal qui existe |
| **Visiteurs uniques du playground**, et combien lancent une requête à eux | à instrumenter, si tu le fais sans traqueur | la démo convertit-elle |
| **Mentions spontanées** | recherche du nom | le bouche-à-oreille a-t-il démarré |
| Étoiles | GitHub | vanité, mais sert de preuve sociale aux suivants |

Le seuil qui change tout : **la première issue ouverte par quelqu'un que tu ne
connais pas.** À partir de là, le projet existe en dehors de toi.

---

## 10. Le point le plus important

Ta meilleure carte n'est pas le moteur — c'est **la manière dont tu le
vérifies**.

Tout le monde publie des benchmarks. Presque personne ne publie un panel qui
compare chaque réponse, span par span, à une lecture brute du disque, qui
affiche le temps de cette référence à côté du sien, qui marque `NOT VERIFIED`
les lignes qu'il ne sait pas prouver, et qui a trouvé un bug de rappel chez son
auteur dès le premier passage.

C'est ça qui te distingue durablement d'un projet de plus, et c'est ça qu'il
faut mettre en avant partout : dans le README, dans l'article, dans les
commentaires, dans les réponses. Pas « c'est rapide » — **« voici le chiffre,
voici la référence, voici la commande pour le refaire »**.
