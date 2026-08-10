# Plan — Timeout premier octet SSE + retry (provider Ollama)

## Contexte

Le fix pool `787989a44` (`pool_max_idle_per_host(0)`) a réduit les hangs du premier tour
(connexions poolées zombies fermées par le LB ollama.com), mais ~1 run sur 10 hang encore :
une fois le POST accepté (headers reçus), **rien ne borne l'attente de la première ligne SSE**
hors le read_timeout client de 600 s. `with_line_timeout` (ollama.rs:484-489) exempte
volontairement le premier item (« time-to-first-token governed by the request timeout »).

Objectif : borner l'attente du premier octet (~45 s, configurable) et faire rejouer la requête
par le retry transient existant — chaque essai rouvre une connexion fraîche (pool idle 0),
ce qui résout le cas zombie.

## Global Constraints

- Scope strict : `crates/kaji-providers/src/ollama.rs` + `crates/kaji/src/providers/ollama_def.rs`. Aucun autre provider.
- Le timeout premier octet produit **`ProviderError::NetworkError`** (retryable sous `transient_only`), jamais `RequestFailed`.
- Comportement per-chunk (`with_line_timeout`) inchangé pour les lignes suivantes.
- Pas de mécanisme de retry séparé : le `with_retry(...).transient_only()` existant (max 10, backoff 2 s → 15 s) porte le retry. Décision assumée : pire cas pathologique ≈ 45 s × essais, toujours < aux 600 s actuels et improbable (hangs indépendants par connexion).
- TDD (test d'abord quand faisable), style du fichier respecté (code self-documenting, commentaires rares « why »).
- `cargo fmt` obligatoire ; baseline `kaji --lib` : 8 échecs préexistants, ne pas en ajouter.
- Commit en français préfixé `kaji: `.

## Task 1 — First-byte timeout + retry dans le provider Ollama

**Fichiers** : `crates/kaji-providers/src/ollama.rs`, `crates/kaji/src/providers/ollama_def.rs`.

### 1. Option de config

- `OllamaOptions` : nouveau champ `first_byte_timeout_secs: u64`.
- Nouvelle const `OLLAMA_DEFAULT_FIRST_BYTE_TIMEOUT_SECS: u64 = 45` (à côté de `OLLAMA_DEFAULT_CHUNK_TIMEOUT_SECS`), doc comment : borne l'attente avant la première ligne SSE (résiduel des hangs de connexions zombies/LB) ; configurable via `OLLAMA_FIRST_BYTE_TIMEOUT`.
- `Default for OllamaOptions` → 45.
- `ollama_def.rs::options_from_config` : résolution via `resolve_ollama_first_byte_timeout(config)` — `OLLAMA_FIRST_BYTE_TIMEOUT` si > 0, sinon défaut. Même pattern que `resolve_ollama_chunk_timeout` (zéro/invalide ignorés). Doc comment aligné sur celui du chunk timeout.

### 2. Restructure `stream()` (ollama.rs:407-441)

- La closure passée à `with_retry` fait désormais : POST → `handle_status` → construction du line-stream (`StreamReader` + `FramedRead`/`LinesCodec`, comme aujourd'hui dans `stream_ollama`) → attente de la première ligne bornée.
- Nouveau helper testable opérant sur un stream de lignes (pas sur `Response`) :
  ```rust
  type LineStream = Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>>;
  async fn take_first_line(lines: LineStream, timeout_secs: u64)
      -> Result<(Option<String>, LineStream), ProviderError>
  ```
  - Timeout écoulé → `Err(NetworkError(...))` : message explicite « no SSE data received within {N}s » + hints (augmenter `OLLAMA_FIRST_BYTE_TIMEOUT` ; vieux builds Ollama + `stream_options` → `OLLAMA_STREAM_USAGE=false`).
  - Premier item `Err(e)` → `Err(NetworkError(...))` (connexion coupée avant le premier octet = rejouable).
  - Stream terminé sans item → `Ok((None, lines))`.
  - Premier item `Ok(line)` → `Ok((Some(line), lines))`.
- Hors closure : rechaîner `futures::stream::iter([Ok(first)])` devant le reste, puis appeler `stream_ollama`.
- `stream_ollama` change de signature : prend le line-stream réassemblé au lieu de `Response` (adapter ses tests existants si besoin ; vérifier qu'il n'a pas d'autre appelant).
- `with_line_timeout` : logique inchangée ; mettre à jour le commentaire du premier item (désormais préfetché sous le first-byte timeout, l'exemption ne couvre plus un vrai attente réseau).
- Commentaire `apply_ollama_options` (ollama.rs:239-243) : le stall des vieux builds est maintenant borné par le first-byte timeout (~45 s + retries) au lieu de 600 s — reformuler.
- Pas de double-log : `log.error` reste au niveau du `inspect_err` final, pas dans la closure.

### 3. Tests

- `ollama.rs` (mod tests) — `take_first_line` :
  - `#[tokio::test(start_paused = true)]` + `futures::stream::pending()` → timeout → `NetworkError`, message contient `OLLAMA_FIRST_BYTE_TIMEOUT`.
  - Stream `[Ok("a"), Ok("b")]` → `Some("a")` + reste dans l'ordre.
  - Stream vide → `Ok((None, _))`.
  - Premier item `Err` → `NetworkError`.
- `ollama_def.rs` : tests `resolve_ollama_first_byte_timeout` (set / unset / zéro) sur le modèle exact des tests `resolve_ollama_chunk_timeout` existants.

### 4. Vérification

```bash
cargo fmt
cargo test -p kaji-providers
cargo test -p kaji --lib ollama_def
cargo clippy -p kaji-providers --all-targets -- -D warnings
cargo clippy -p kaji --lib --bins -- -D warnings
```
(`clippy --all-targets` workspace est rouge pour cause préexistante `acp_fixtures` — hors scope.)

### 5. Commit

`kaji: providers ollama — timeout premier octet SSE (45 s, OLLAMA_FIRST_BYTE_TIMEOUT) + retry transient`
