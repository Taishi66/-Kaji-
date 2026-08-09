# Architecture KAJI

> Provenance : les décisions d'architecture de ce document vivent dans un vault
> personnel du propriétaire du repo, sous forme d'ADRs datés (2026-08-07/08).
> Ce fichier en est la restitution technique côté code — pas de chemin absolu
> personnel ici, seulement les titres des décisions pour recoupement.

## Vue produit

KAJI est un harness agentique **personnel**, fork élagué de [goose](https://github.com/aaif-goose/goose),
en cours de transformation produit. Quatre exigences pilotent chaque choix :

- **Performance et légèreté** — pas de runtime lourd ajouté.
- **Un seul binaire** — un artefact `kaji` à distribuer, deux points d'entrée.
- **Mémoire et contexte optimisés au maximum** — le différenciant produit.
- **UX/UI soignée sans dépendance lourde** — pas d'Electron dans la cible.

## Architecture cible

```
                        ┌─────────────────────────────┐
                        │      binaire unique `kaji`   │
                        └───────────────┬───────────────┘
                                        │
                 ┌──────────────────────┼──────────────────────┐
                 │                                              │
        `kaji tui` (in-process)                       `kaji serve` (ACP HTTP+WS)
                 │                                              │
                 ▼                                              ▼
      ┌─────────────────────┐                       ┌───────────────────────┐
      │   TUI Ratatui        │                       │   protocole ACP        │
      │   (livrée,           │                       │   (agent-client-       │
      │   in-process)        │                       │   protocol, existant)  │
      │   zéro réseau         │                       └───────────┬───────────┘
      └──────────┬───────────┘                                    │
                 │ appels de fonction directs                     │ secret KAJI_SERVER__SECRET_KEY
                 │                                                │ + TLS fingerprint (loopback)
                 ▼                                                ▼
      ┌─────────────────────────────────────────────────────────────┐
      │                        kaji-core (lib Rust)                  │
      │  StateMachine / SessionManager · Provider · mémoire ·        │
      │  context engineering · orchestrateur SDD/AIAD (à venir)      │
      │  — jamais un daemon, jamais un processus de fond exposé —    │
      └─────────────────────────────────────────────────────────────┘
                                                                    ▲
                                                                    │ processus enfant auto-géré
                                                       ┌────────────┴────────────┐
                                                       │  Desktop Tauri v2 (cible)│
                                                       │  actuellement Electron   │
                                                       │  (`ui/desktop/`)         │
                                                       └──────────────────────────┘
```

Principe : `kaji-core` porte toute la logique (boucle agent, mémoire, contexte,
orchestrateur de modes) et n'est **jamais un daemon**. Les deux clients
l'atteignent par deux chemins distincts :

- **TUI Ratatui** — mode du binaire unique, in-process : le terminal appelle
  directement `StateMachine`/`SessionManager`, zéro réseau, zéro sérialisation
  d'état. **État actuel du code** (vérifié) : la commande `kaji tui`
  (`crates/kaji-cli/src/commands/tui.rs` → `crates/kaji-cli/src/tui/`,
  feature `tui`, dépendance `ratatui 0.30.2`) est la TUI native in-process
  livrée — chat streamé in-process sur `Agent::reply` (annulation Esc) et
  panneau SPEC pilotant une passe SDD (gate humaine, exécution, validation
  de verdict `VERDICT: VALIDE`/`DRIFT`, verrouillage anti-drift), état pur
  porté par `kaji_core::sdd`. L'ancien script JS externe
  (`ui/text/dist/tui.js` / `npx @aaif/kaji`) n'est plus invoqué ; le module
  `ui/text` reste marqué **déprécié** par son propre README.
- **Desktop** — parle au Core via le protocole **ACP** (Agent Client Protocol)
  exposé par `kaji serve` : HTTP + WebSocket (`crates/kaji/src/acp/transport/`,
  `crates/kaji/src/acp/server/`), stdio disponible en alternative
  (`kaji acp` sur stdio). Authentification par secret partagé
  `KAJI_SERVER__SECRET_KEY`, confinement loopback + confiance TLS par
  empreinte de certificat. Ce transport est **déjà implémenté et éprouvé** :
  c'est le mécanisme que le desktop Electron actuel (`ui/desktop/src/main.ts`)
  utilise en production pour parler à son backend (`checkBackendStatus`,
  `KAJI_EXTERNAL_BACKEND`, `trustBackendCertificate`). La cible porte ce même
  pattern vers un shell Tauri v2 — **aucune dépendance `tauri` n'existe encore
  dans le repo** ; le desktop Electron (`ui/desktop/`, forge/electron-builder)
  reste le client réel aujourd'hui.

Un seul binaire, deux entrées : `kaji tui` (in-process, cible) et `kaji serve`
(ACP réseau local, piloté par le parent desktop) partagent le même exécutable
`kaji` produit par `cargo build -p kaji-cli --bin kaji`.

## Sous-système mémoire (livré)

Contrat de source de vérité : le module mémoire vit dans deux endroits —
`crates/kaji-core/src/memory.rs` (moteur, dépendance-free hors `rusqlite`) et
le pont `crates/kaji/src/kaji.rs` (`SessionMemory`, câblage dans les deux
boucles agent). Vérifié dans le code, pas supposé :

- **Backend** : SQLite via `rusqlite 0.32.1` (non-bundled — lie la même
  `libsqlite3-sys` que `sqlx-sqlite`, une seule SQLite système, pas de C
  vendu). Table `memory_entries` + table virtuelle FTS5 external-content
  `memory_fts`, triggers insert/delete/update. Recall `MATCH` OR-joint sur
  tokens quotés, `ORDER BY score ASC` sur `bm25(memory_fts, 1.0, 5.0, 1.0)`
  (score brut négatif : plus négatif = meilleur ; le poids 5.0 sur la colonne
  entités reproduit le boost entités du POC BM25-lite d'origine).
- **Store partagé inter-session** : une seule DB `shared.db` (constante
  `SHARED_FILE`), colonne `session_id` par entrée. Le recall est **global**
  sur tout le corpus (BM25 sur l'ensemble des sessions) ; l'ancrage temporel
  (`anchored_view`, fenêtre + bookends) reste **scopé à la session d'origine
  du hit** — aucune fuite inter-session dans le contexte affiché.
  Migration one-shot idempotente des anciennes DB par-session (`{id}.db` →
  `{id}.db.legacy`) au premier chargement.
- **Budget de compaction AIAD** : seuil `DEFAULT_COMPACTION_THRESHOLD = 0.6`
  (`crates/kaji/src/context_mgmt/mod.rs`), remplace le seuil réactif `0.8`
  hérité de goose — compaction proactive avant dégradation, alignée sur la
  bande AIAD `[0.6, 0.7]` (`BUDGET_MIN`/`BUDGET_MAX` dans `kaji-core`).
- **Ingestion continue** : `ingest_turn` extrait les dernières instructions
  utilisateur d'un tour et les écrit en mémoire (dédup exacte via
  `contains_body`, extraction d'entités zéro-token — mots >3 caractères hors
  stopliste FR/EN).
- **Injection dans le prompt** : `splice_memory_block` (recall + rendu
  markdown 0-token, header `## KAJI memory — recalled across sessions`) est
  appelé **dans les deux chemins agent** — `agents/agent.rs` (legacy) et
  `agents/state_machine/ops_llm.rs` — avec la même query
  (`latest_user_instruction`) : parité vérifiée dans le code, pas un idéal
  déclaré.
- **Recall zéro-token** : aucun appel LLM dans le cycle mémoire ; seul le
  tour de conversation consomme des tokens.

## Modes SDD / AIAD

Toggle prévu (`mode: sdd | aiad` en config) — **statut : non implémenté**.
Aucun code de toggle, de pipeline SDD (Intent → SPEC → Gate → Exec →
Validate → Drift lock) ni d'orchestrateur AIAD n'existe dans le repo à ce
jour (vérifié par recherche de `sdd`/`SDD`/`AIAD` dans `crates/kaji`,
`crates/kaji-cli`, `crates/kaji-core` — seules des mentions en commentaire
autour du budget mémoire existent). Les deux modes partageront le même Core
et la même mémoire une fois posés.

- **SDD** — pipeline rigoureux spec-anchored : Intent → SPEC → Gate → Exec →
  Validate → Drift lock. La SPEC est un invariant vivant, verrouillé contre
  la dérive.
- **AIAD** — le cadre organisationnel complet (empirisme, orchestration,
  fluidité, excellence intentionnelle) ; AGENT-GUIDE/ARCHITECTURE comme
  mémoire collective permanente, SPEC comme mémoire de tâche — jamais
  mélangées dans le budget de contexte.

## Gardé / jeté du fork goose

| Zone | Décision | État vérifié |
|---|---|---|
| `agents/state_machine/` | **Gardé**, boucle par défaut cible | présent, activé par `KAJI_STATE_MACHINE=1` ; legacy `agents/agent.rs` toujours en parallèle (migration en cours, cf. AGENTS.md) |
| `providers/` (trait `Provider`) | **Gardé** tel quel | présent (`kaji-provider-types`, `kaji-providers`) |
| `session/` | **Gardé** | présent (`crates/kaji/src/session/`) |
| Desktop Electron (`ui/desktop/`) | **Jeté** à terme (cible Tauri v2) | **toujours le client réel aujourd'hui** — dépendances `electron-forge` en place, pas de `tauri` dans le repo |
| TUI JS (`ui/text/`) | **Jeté**, déjà mort | confirmé : README marque le projet déprécié, source retirée ; plus aucune invocation node/npx — `kaji tui` est désormais la TUI Ratatui native in-process (`crates/kaji-cli/src/tui/`) |
| `kaji-mcp` (ex `goose-mcp`) | Élagage partiel | crate toujours présente dans le workspace, pas de suppression totale constatée |

## Rationale IPC — rejet du daemon

Décision actée (ADR IPC, 2026-08-08) : pas de `kajid` persistant en écoute
loopback. Un daemon partagé par TUI et desktop introduit une surface réseau
même confinée en loopback — classe d'attaques récurrente sur les backends
locaux mal confinés (rebinding DNS, origines non validées ; motif observé sur
plusieurs CVEs de backends MCP locaux courant 2026) — pour un bénéfice nul
quand la TUI tourne dans le même utilisateur, sur la même machine, au même
instant que le Core. La TUI in-process ferme cette surface par construction ;
seul le desktop, qui a réellement besoin d'un canal réseau local (process Tauri
séparé du Core), porte la frontière ACP — et cette frontière est déjà
production-éprouvée côté Electron.

Conséquence assumée : la frontière TUI in-process est réversible
localement (`two-way`) ; la frontière ACP desktop devient une porte
`one-way` dès qu'un client externe s'y connecte — coût de retour
moyen-élevé, à ne pas franchir à la légère.
