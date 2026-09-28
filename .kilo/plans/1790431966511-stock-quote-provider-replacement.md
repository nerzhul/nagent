# Replace Stooq backend of `get_stock_quote` with Twelve Data (Finnhub soft-fallback)

## Context

`crates/stt-server/src/agents/stock_agent.rs` backs `get_stock_quote`
with Stooq's free CSV endpoint. From datacenter IPs (Kubernetes, cloud
VMs, Docker) Stooq is gated by Cloudflare and returns a JS challenge
instead of CSV — see project memory `stt_server_stooq_browser_block`.
The agent also has no first-class support for market indices.

## Provider decision

**Twelve Data** (`https://api.twelvedata.com`), free tier 800 req/day,
8 req/min. Official REST/JSON, indices (`^FCHI`, `^GSPC`, `^GDAXI`,
`^IXIC`, `^DJI`, `^FTSE`, `^N225`, `^STOXX50E`, `^SSMI`, `^IBEX`,
`^HSI`) and equities (e.g. `AAPL`, `MC.PA`) on the same `/quote`
endpoint with identical response shape.

### EU candidate survey (recorded so it is not re-litigated)

| Candidate | EU-based? | Verdict |
| --- | --- | --- |
| Stooq (Poland) | yes | Already in use, Cloudflare-gated for the user — discarded. |
| ECB SDMX (`data-api.ecb.europa.eu`) | yes, official EU institution | Free, no key, datacenter-safe — but **FX only**, no equities / no indices. Out of scope for this agent. |
| Eurostat | yes | Macro-economic stats only. |
| Euronext / Deutsche Börse / LSE / BME APIs | yes | Paid, institutional onboarding. Tier "Delayed" gratuit d'Euronext trop limité pour du chat. |
| Investing.com (Chypre) | yes | Pas d'API publique ; `investpy` scraping instable, même problème Cloudflare que Stooq. |
| SimFin (Berlin) | yes | Tier gratuit, bulk CSV, pas de REST live. Pas adapté au chat. |
| Finnhub (fondé Helsinki) | partial | Tier gratuit 60 req/min, pas de plafond journalier, indices supportés. Heritage EU mais entité US côté ToS. **Soft fallback / option d'alternative**: si Twelve Data quotas devient contraignant, swap transparent (même shape `/quote?symbol=…&token=…`). |
| Twelve Data / FMP / Alpha Vantage | US | Twelve Data retenu pour 800 req/jour, 8 req/min et la doc propre. |

"UE" est une préférence, pas une contrainte RGPD — décision confirmée par
l'utilisateur. Pas d'option EU fair-use gratuite couvrant actions +
indices + datacenter-friendly n'existe à la date d'aujourd'hui.

## Plan

1. **Nouveau provider dans `stock_agent.rs`**
   - Remplacer `fetch_with_fallback` (Stooq) par `fetch_twelvedata` →
     `https://api.twelvedata.com/quote?symbol={ticker}&apikey={key}`.
   - Ajouter `parse_twelvedata_quote` (status `ok`|`error`, champs
     `close`, `open`, `high`, `low`, `volume`, `datetime`, `exchange`)
     qui mappe vers le JSON de réponse public existant
     (`{ok, data:{ticker,exchange,price,open,high,low,close_prev,volume,as_of,as_of_time}, source, tried, fetched_at}`).
   - Ajouter `KNOWN_INDICES` (11 symboles, table plus haut).
   - Généraliser `normalise_ticker` pour accepter `^FCHI`, `cac 40`,
     `CAC40`. Conserver `KNOWN_COMPANIES` inchangé pour les actions.
   - Ajouter `INDEX_FALLBACKS = [^FCHI, ^STOXX50E, ^GDAXI, ^FTSE, ^GSPC]`
     réessayé quand le lookup principal rate.
   - Cache TTL 60 s in-memory, borné à 256 entrées, FIFO
     (`std::collections::VecDeque<(String, Instant, String)>` derrière
     un `tokio::sync::Mutex`) — nécessaire car le quota Twelve Data
     (8 req/min) est plus strict que celui de Stooq.
   - Rate limiter token-bucket 8 req/min avec
     `AgentError::RateLimited` + hint `Retry-After`.

2. **Config TOML `[agents.get_stock_quote]`** dans
   `crates/stt-server/src/config.rs` (parallel à `[agents.get_weather]`):
   ```toml
   [agents.get_stock_quote]
   api_key = "your-twelve-data-key"
   timeout_ms = 8_000
   base_url = "https://api.twelvedata.com"
   requests_per_minute = 8
   ```
   Env overrides: `STOCK_API_KEY`, `STOCK_TIMEOUT_MS`,
   `STOCK_BASE_URL`, `STOCK_REQUESTS_PER_MINUTE`.

3. **Startup guard**: `lib.rs` / `main.rs` — si la feature `stock-agent`
   est activée et aucune clé n'est fournie (ni TOML ni env), log
   warning + l'agent n'est pas enregistré. Erreur claire pointant vers
   `examples/config.toml.example`.

4. **Feature flags Cargo** (`crates/stt-server/Cargo.toml`):
   - `stock-agent` (default-on pour `make run*`) → Twelve Data.
   - `stock-agent-stooq` (non-default) → chemin Stooq préservé pour
     ceux qui y tiennent, aucun nouveau dev.

5. **Mise à jour des tests**
   - Inline (dans `stock_agent.rs::tests`): remplacer les
     `parse_stooq_csv_*` par 10 tests `parse_twelvedata_quote_*`
     (voir détail dans la section "Validation").
   - `tests/agents.rs::stock_agent_parses_csv_with_loopback_fixture`
     → renommé `stock_agent_parses_quote_with_loopback_fixture`,
     stub loopback Twelve Data via `base_url` override.
   - Ajouter
     `stock_agent_missing_api_key_errors_at_startup` — assert que
     `get_stock_quote` n'apparaît pas dans `GET /v1/agents` sans clé.

6. **Documentation** (`README.md`)
   - Section `get_stock_quote` (lignes 381-416) : réécrire pour
     Twelve Data, lister `KNOWN_INDICES`, documenter le bloc TOML +
     env, mentionner le cache 60 s.
   - Table de config maître (ligne 215) : ajouter l'entrée
     `[agents.get_stock_quote]`.
   - `examples/config.toml.example` : ajouter le bloc d'exemple.

7. **`Makefile`**: aucun changement — `stock-agent` reste activé sur
   les 6 targets `make run*` (par défaut = Twelve Data maintenant).

## Finnhub soft-fallback (note pour plus tard)

Si Twelve Data devient contraignant (quota, latence, changement de
ToS), le code est structuré pour que le swap soit localisé :
- `fetch_twelvedata` → `fetch_finnhub`, param `token` au lieu de
  `apikey`, endpoint `/quote?symbol=…&token=…`.
- Mêmes `KNOWN_INDICES`, même parser (mapping trivial).
- Aucun changement au contrat public de `get_stock_quote`.

Ce n'est **pas** dans le scope de cette itération, mais la séparation
provider / agent est pensée pour ça.

## Validation

1. `make fmt && make clippy && make test` clean.
2. `cargo build -p stt-server --no-default-features --features
   stt-server/stock-agent,stt-server/stock-agent-stooq` — compile
   des deux côtés.
3. Tests unitaires `stock_agent::tests`:
   - `parse_twelvedata_quote_happy_path_equity` (`AAPL`),
   - `parse_twelvedata_quote_happy_path_index` (`^FCHI`),
   - `parse_twelvedata_quote_errors_on_missing_symbol` → `NoData`,
   - `parse_twelvedata_quote_errors_on_auth_failure` → `Auth`,
   - `parse_twelvedata_quote_errors_on_rate_limit` → `RateLimited`,
   - `normalise_ticker_handles_caret_prefix` (`^FCHI`, `cac 40`,
     `CAC40`),
   - `resolve_known_index_handles_french_names`,
   - `tried_symbols_for_index_lists_index_fallbacks`,
   - `rate_limiter_blocks_after_burst`,
   - `cache_returns_cached_payload_within_ttl`.
4. Smoke manuel avec une vraie clé:
   ```sh
   STOCK_API_KEY=… make run-mock
   curl -s -X POST localhost:8080/v1/agents/get_stock_quote/invoke \
     -H 'content-type: application/json' \
     -d '{"arguments":{"ticker":"^FCHI"}}'
   curl -s -X POST localhost:8080/v1/agents/get_stock_quote/invoke \
     -H 'content-type: application/json' \
     -d '{"arguments":{"ticker":"Atos"}}'
   ```
5. `GET /v1/agents` liste `get_stock_quote`; `POST …/invoke` avec
   `{"ticker":"AA;DROP"}` → 400 (régression, inchangé).

## Out of scope

- Time-series / historique (autre endpoint Twelve Data, autre schéma).
- Forex / crypto / commodities (ajout = nouvel agent).
- Fallback scraping Investing.com ou Stooq-via-proxy-résidentiel.
- Activation du backend Twelve Data dans la CI (les tests loopback
  suffisent, comme aujourd'hui pour Stooq).
