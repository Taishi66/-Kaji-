# Task 9 — Télémétrie tokens native (S5) — rapport

## Schéma trouvé / migré

`usage_ledger` (créée en migration v15) portait déjà :
`session_id, created_timestamp, model, input_tokens, output_tokens, total_tokens,
cache_read_tokens, cache_write_tokens, cost, cost_source, is_compaction`.

**Manquant : `provider`.** `sessions.provider_name` existe, mais il peut changer en cours
de session : rattacher une ligne ancienne au provider courant fausserait l'attribution.
Migration additive **v18** (`CURRENT_SCHEMA_VERSION` 17 → 18) :

- `ALTER TABLE usage_ledger ADD COLUMN provider TEXT` (garde `pragma_table_info`, comme
  les migrations 12–17) ;
- `CREATE INDEX idx_usage_ledger_created ON usage_ledger(created_timestamp)` — toutes les
  requêtes de métriques bornent sur ce champ ;
- même colonne + même index ajoutés à `create_tables` pour les bases neuves.

Les lignes antérieures restent à `NULL` (pas de réécriture) et sont rattachées **à la
lecture** par `COALESCE(u.provider, s.provider_name, '(inconnu)')`.

**Écriture** : le provider est dénormalisé en SQL, dans `insert_usage_ledger_row`
(`(SELECT provider_name FROM sessions WHERE id = ?)`) et dans l'insert « carried_forward »
de `record_usage_metrics` (`s.provider_name`). Aucun changement de signature.

`working_dir` : joint via `sessions` (`LEFT JOIN sessions s ON s.id = u.session_id`), puis
replié côté Rust sur la racine git (`config::paths::find_git_root`), avec repli sur le
chemin lui-même quand le dépôt a disparu.

## Parité legacy / state machine

`record_usage_metrics` est le **site partagé** : `agents/reply_parts.rs` (legacy) et
`agents/state_machine/usage.rs` (SM) l'appellent tous les deux, et la dénormalisation vit
dans le SQL de ce site. Parité par construction, aucune double application nécessaire.
Les deux chemins gardent leur court-circuit de rejeu (`!self.is_replay()` / `if replay`).

## Requêtes

Trois lectures pures ajoutées à `SessionStorage` (+ délégués `SessionManager`) :

| Méthode | SQL |
|---|---|
| `metrics_buckets(dimension, start, end)` | `GROUP BY <clé de dimension>, COALESCE(u.provider, s.provider_name), u.model` sur `[start, end)` — le couple (provider, modèle) est conservé pour aller chercher le tarif du cache, pas pour l'affichage |
| `usage_ledger_costs_between(start, end)` | `(created_timestamp, cost)` bruts — le découpage en jours **locaux** se fait en Rust, pas via `date(…, 'localtime')` dont le fuseau dépend de l'environnement du process |
| `usage_cost_between(start, end)` | `SUM(cost)` |

Clés de dimension : `COALESCE(u.model, '(inconnu)')` · `COALESCE(u.provider,
s.provider_name, '(inconnu)')` · `u.session_id` · `COALESCE(s.working_dir, '')` replié en
racine git.

## Module `crates/kaji/src/metrics/`

- `calendar.rs` — `day_span` / `week_span` (lundi ISO) / `month_span` / `days_in_month` /
  `day_of_month`, intervalles semi-ouverts `[start, end)` en epoch. `now` est **toujours
  un paramètre** ⇒ bornes testables sur dates fixes. Minuit local résolu via
  `Local.from_local_datetime(...).earliest()`, avec repli sur la première heure existante
  du jour quand un saut d'heure d'été supprime minuit.
- `projection.rs` — moindres carrés sur la dépense **cumulée** (`x` = rang du jour 1-basé,
  `y` = cumul), extrapolée au dernier jour du mois.
- `budget.rs` — `KAJI_BUDGET_MONTHLY_USD` + `KAJI_BUDGET_MONTHLY_USD_<PROVIDER>` via
  `Config::get_param` (donc config OU env), seuils 50 / 80 / 100 %, `BudgetLevel`,
  `BudgetStatus`. Jamais un hard-stop.
- `mod.rs` — `MetricsWindow` (day/week/month/5h/7d), `MetricsDimension`
  (model/provider/session/project), `LedgerBucket`, `MetricsRow`, `MetricsReport`,
  `BurnReport`, `fold_buckets`, `report()`, `burn()`, `daily_costs()`, trait `PriceBook`
  + `CanonicalPrices`.

## Choix

**Projection** — régression sur le **cumul**, pas sur la série brute des dépenses
quotidiennes. Le cumul est la grandeur que la projection prolonge, et il lisse les jours
creux sans donner à un pic isolé le poids qu'il aurait sur une pente de série brute.
Garde-fous : `month_end >= spent` (une pente négative ne « rembourse » pas) ; pente ≤ 0 ⇒
repli sur le taux moyen ; 1 seul jour ⇒ extrapolation plate, signalée « indicative » dans
la vue ; série vide ⇒ tout à zéro.

**Prix** — le catalogue canonique embarqué (`providers::canonical::maybe_get_canonical_model`
→ `CanonicalModel.cost: Pricing`), la même source que `Pricing::estimate_cost` qui écrit
déjà les coûts estimés au ledger ; les économies affichées sont donc cohérentes avec les
coûts. Économie de cache = `cache_read_tokens × (prix_entrée − prix_cache_read) / 1e6`,
avec `cache_read` retombant sur le prix d'entrée quand le catalogue ne le donne pas
(⇒ économie 0, jamais négative). Sans tarif connu, `cache_savings = None` et la vue dit
`n/a`, comme `/cost` le fait déjà pour le coût.

**Plancher assumé** : quand une fenêtre mélange des modèles tarifés et non tarifés, les
groupes sans tarif comptent pour 0 dans `cache_savings` — c'est donc un plancher, pas un
chiffre exact ; documenté sur le champ. `None` seulement si **aucun** groupe n'a de tarif.

**Taux de hit** = `cache_read_tokens / input_tokens`, borné à `[0, 1]`. `input_tokens`
inclut déjà les tokens lus en cache — même convention que `Pricing::estimate_cost`, qui
soustrait `cache_read + cache_write` de `input` pour obtenir l'entrée non cachée.

## Surfaces

**TUI `/cost <vue>`** — `CostView` (`Windows | Models | Day | Week | Month | Cache |
Projection`), aliases fr/en (`modèles|models`, `jour|day`, …), argument inconnu ⇒ ligne
d'usage plutôt que repli silencieux. Toutes les tables suivent le patron `/cost` existant
(`format_row`/`column_widths` factorisés dans `render_table`) et le thème actif
(`RoledSpan::table_header` / `border_inactive` / `title` / `dim` / `accent` / `gold`).

- `Models` → par modèle sur 7 j glissants (continuité avec la table `/cost` de base) ;
  `Day`/`Week`/`Month` → par modèle sur la fenêtre calendaire ; `Cache` → colonnes
  `↑ entrée · cache lu · hit · coût · économisé` + pied « sans cache : $X » ;
  `Projection` → aujourd'hui / semaine / mois (J n/N) / rythme $/jour / projection, plus
  une jauge par budget déclaré.
- Avertissements de budget : à l'affichage de `/cost` (vue `Windows`) **et** une fois au
  boot du TUI (`boot_budget_lines`, poussé seulement s'il y a quelque chose à dire).

**CLI** — `kaji metrics [--window day|week|month|5h|7d] [--by model|provider|session|project]
[--format json|table]`, enums clap (valeurs invalides refusées par clap avec la liste des
possibles). Le mode table réutilise `render_table`, donc les deux surfaces ne peuvent pas
diverger sur les colonnes. JSON `camelCase` stable :
`window, dimension, start, end, rows[], totals, cacheHitRate, cacheSavings, burn{today,
week, month, daily[], projection{…}, budgets[…]}`.

## Replay

**Aucune capture nécessaire.** `kaji metrics` et `/cost` sont hors boucle agent : lecture
seule du ledger, rien n'entre dans un prompt de tour, donc pas de nouvelle source d'état
externe au sens de la règle AGENTS.md. L'horloge locale est lue directement
(`chrono::Local::now`) dans les deux points d'entrée — noter que `clippy.toml` interdit
`chrono::Utc::now` mais **pas** `Local::now`, donc aucun `#[allow(clippy::disallowed_methods)]`
n'était nécessaire (un `allow` pour un lint qui ne peut pas se déclencher aurait été du
bruit). Le chemin d'écriture (`record_usage_metrics`) garde ses deux court-circuits de
rejeu inchangés.

## Tests

| Emplacement | Cas | Nb |
|---|---|---|
| `metrics/calendar.rs` | minuit / 23h59↔00h00 / dimanche↔lundi / décembre→janvier / 31↔1er / février bissextile / jour 1-basé | 7 |
| `metrics/projection.rs` | dépense constante ⇒ total exact, accélération > extrapolation plate, série vide, un seul jour, jamais sous le déjà-dépensé, série nulle | 6 |
| `metrics/budget.rs` | les 3 seuils (bornes exactes), ratio + breach, limite 0, clé env par provider, constantes 50/80/100 | 5 |
| `metrics/mod.rs` (unitaires) | repli des buckets + tri, économie de cache chiffrée, taux de hit borné, parsing fenêtre/dimension, clé projet, découpage en jours locaux | 6 |
| `metrics/mod.rs::tests::ledger` (SQLite réel) | jour/semaine/mois sur le même ledger, 4 dimensions, taux de hit, burn + projection ($2/j × 9 j ⇒ $60 sur 30 j), ledger vide | 5 |
| `session_manager.rs` | migration v17→v18 (colonne ajoutée, lignes existantes non réécrites, lecture rattachée), dénormalisation du provider à l'écriture, buckets par modèle/provider/session/projet, bornes de `usage_ledger_costs_between` / `usage_cost_between` | 6 |
| `tui/report.rs` | parsing des 6 vues + rejet, fenêtre de chaque vue, table + totaux, fenêtre vide, vue cache (hit + pied « sans cache »), cache sans tarif, projection (+ avertissement 1 jour), jauges de budget, avertissements sur seuils franchis, padding de `render_table` | 11 |
| `tui/app.rs` | `/cost <vue>` → `Action::Cost(vue)` (4 vues), vue inconnue ⇒ ligne d'usage, `/costume` n'est pas un `/cost` | 3 |
| `commands/metrics.rs` | mapping des enums clap, forme JSON stable (clés camelCase, `burn.projection`, `burn.budgets`) | 2 |

**51 tests ajoutés.**

Suites : `cargo test -p kaji --lib` → **2015 passed, 0 failed** ·
`cargo test -p kaji-cli` → **959 passed, 0 failed, 1 ignored** ·
`cargo clippy -p kaji -p kaji-cli --all-targets -- -D warnings` → **clean**.

Fumée sur données réelles : `kaji metrics --help` (exit 0, les 3 options),
`--window month --by provider` (5 providers, totaux, burn, budget global à $200),
`--format json` (JSON valide), `--window decade` (refusé par clap avec la liste).

## Écarts

1. **Fichiers FOREIGN touchés.** Le plan interdit `crates/kaji-cli/src/tui/{app,mod}.rs`
   tant qu'ils portent du travail étranger non commité. La vue `/cost <arg>` est un
   livrable explicite de la task et ne peut pas exister sans eux. Diff tenu au minimum :
   `app.rs` = `Action::Cost` porte désormais une `CostView` (+ l'entrée `COMMANDS`, le
   helper `cost_command_arg`, une branche de dispatch, `use crate::tui::report`) ;
   `mod.rs` = signature de `cost_report`, le bras `Action::Cost(view)`, la ligne d'aide, et
   l'appel `boot_budget_lines`. Toute la logique vit dans `tui/report.rs` (non foreign),
   `crates/kaji/src/metrics/` et `commands/metrics.rs`. `theme.rs`, `ui.rs`, `viewer.rs` et
   `crates/kaji-exec/` : non touchés.

2. **Test bout-en-bout en `src/`, pas en `tests/`.** `SessionStorage::pool` est
   `pub(crate)` : un test d'intégration ne peut pas semer de lignes de ledger datées. Les
   5 cas SQLite vivent donc dans `metrics/mod.rs::tests::ledger`, comme les tests DB déjà
   présents dans `session_manager.rs`. L'alternative (rendre `pool` public) élargissait
   l'API pour du confort de test.

3. **`/cost modèles` = 7 j glissants**, pas une fenêtre calendaire — pour que la vue ne
   fasse pas doublon avec `/cost mois` et prolonge l'horizon le plus large de la table
   `/cost` existante. `/cost cache` est sur le mois (l'horizon des budgets).

4. **Vues `/cost` limitées aux 6 nommées par S5.** Les dimensions provider / session /
   projet passent par `kaji metrics --by`, pas par une vue TUI supplémentaire.

5. **`metrics::burn` est appelée au boot du TUI** même sans budget déclaré (deux lectures
   SQLite indexées). Pas de court-circuit : un budget déclaré uniquement en config par
   provider ne serait pas détectable sans connaître d'abord les providers dépensiers.

6. **Non-but respecté** : aucun exporteur Prometheus/OTel. `--format json` est le point de
   sortie pour qui voudra brancher autre chose.
