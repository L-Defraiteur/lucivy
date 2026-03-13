# Playground : Git clone, startsWith UI, fixes

## Clone GitHub dans le playground

Ajout d'un champ URL GitHub dans la section import du playground. L'utilisateur colle une URL `https://github.com/owner/repo`, choisit optionnellement une branche, et clique "Clone repo".

### Architecture

1. Le playground fetch le tarball via un **Cloudflare Worker CORS proxy** (`lucivy-proxy.luciedefraiteur.workers.dev`) — une seule requête pour tout le repo
2. Le proxy ne sert que les URLs `api.github.com` (whitelist), forward le token si présent, suit les redirects côté serveur
3. Le tarball est extrait côté client avec `extractTarGz()` (code existant réutilisé)
4. Les fichiers texte sont indexés par batch de 200 avec commit intermédiaire (évite OOM WASM sur gros repos)
5. Les fichiers > 100KB sont ignorés (datasets CSV/parquet de rag3db = 46MB)

### Token GitHub

Champ optionnel (type password, sauvé en localStorage). Un fine-grained PAT avec zéro permission sur public repos suffit — augmente juste le rate limit. Message dans l'UI : "Si limite dépassée ou repos privés, utilisez votre token. Tout tourne dans votre navigateur, rien n'est sauvegardé."

### UX

- Animation `Downloading owner/repo.` → `..` → `...` pendant le download + extraction
- Progression `Indexing owner/repo... 200/1547` pendant l'indexation
- Résultats stale : compteur de génération pour ignorer les réponses async tardives (race condition search-as-you-type)

## startsWith exposé dans le playground

### Rust (bindings/emscripten/src/lib.rs)

- Nouveau type `startsWith_split` dans `parse_query` : split les mots, crée un `boolean { should: [startsWith, startsWith, ...] }`
- `startsWith` simple passé tel quel au core (déjà supporté)

### UI (playground/index.html)

Nouveau select de mode de recherche :
1. **startsWith (split)** — défaut, le plus rapide (FST prefix range)
2. startsWith
3. contains (split)
4. contains
5. contains + regex

### `buildQuery` simplifié

Le mode est passé directement comme `type` au query JSON (sauf regex qui reste un cas spécial).

## Suppression des logs scheduler en WASM

`bindings/emscripten/src/lib.rs` : supprimé `LUCIVY_SCHEDULER_DEBUG=1` et `set_scheduler_log_hook`. C'était un résidu de debug qui spammait la console via `_fd_write` → proxy cross-thread pour chaque event scheduler. Sans subscriber, l'EventBus est zero-cost (`has_subscribers()` court-circuite).

## Cloudflare Worker

Déployé sur `lucivy-proxy.luciedefraiteur.workers.dev`. Code dans `playground/lucivy-proxy-worker.js`.

- CORS preflight (OPTIONS) → `Access-Control-Allow-Origin: *`
- Whitelist `https://api.github.com/` uniquement
- Forward `Authorization` header si présent
- `redirect: 'follow'` côté serveur (contourne le CORS block de `codeload.github.com`)
- Free tier : 100k req/jour

## WASM rebuild

Build OK. `lucivy.wasm` = 6.6 MB, `lucivy.js` = 87 KB. Copié dans `playground/pkg/`.

## Fichiers modifiés/créés

```
bindings/emscripten/src/lib.rs       # startsWith_split, suppression debug logs
playground/index.html                # git clone UI, startsWith modes, race condition fix, animation
playground/lucivy-proxy-worker.js    # nouveau — code du Cloudflare Worker
playground/serve.mjs                 # inchangé
bindings/emscripten/pkg/*            # rebuild WASM
playground/pkg/*                     # rebuild WASM
```

## Prochaines étapes

1. Tester startsWith dans le playground avec le nouveau build
2. Commit + push
3. Publication Emscripten npm
4. Post LinkedIn avec les chiffres de bench
