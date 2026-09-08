# Filament — distributed tracing for Steadholme

Filament collapses Jaeger/Tempo into one DB: it stores spans, reconstructs traces, and renders a
waterfall, so request latency across the estate's services is queryable from a single
command-center dashboard.

- **Subdomain:** `traces.w33d.xyz` (Sluice `auth=sso` at the root)
- **Internal port:** `9230`
- **Store:** in-memory by default (`FILAMENT_STORE=memory`), PostgreSQL via `FILAMENT_STORE=postgres`
- **DB name:** `filament`

## Surfaces

| Method | Path                 | Auth                         | Purpose |
|--------|----------------------|------------------------------|---------|
| GET    | `/healthz`           | none                         | Liveness (container HEALTHCHECK). Returns `ok`. |
| GET    | `/`                  | SSO (gateway `X-Auth-*`)     | Recent traces; filter by `?service=` / `?min_ms=`. |
| GET    | `/trace/{trace_id}`  | SSO                          | The waterfall (spans by start, indented by parent). |
| GET    | `/api/traces`        | SSO                          | The filtered trace summaries as JSON. |
| POST   | `/ingest`            | **own** `Bearer` (internal)  | Span ingest. Returns `{ "accepted": N }`. |

`POST /ingest` is **internal-only** and **NOT** gateway-routed — services post spans to it
directly, authenticated with `Authorization: Bearer $FILAMENT_INGEST_TOKEN`. The gateway strips
inbound `X-Auth-*`; the dashboard trusts only the injected identity for display.

## Ingest formats (liberal)

Either OTLP/HTTP-ish JSON:

```json
{ "resourceSpans": [ { "resource": { "attributes": [ {"key":"service.name","value":{"stringValue":"gateway"}} ] },
  "scopeSpans": [ { "spans": [ { "traceId":"t1","spanId":"a","parentSpanId":"",
  "name":"GET /","startTimeUnixNano":"...","endTimeUnixNano":"...","status":{"code":2} } ] } ] } ] }
```

…or a flat span array (epoch **microseconds**):

```json
[ { "trace_id":"t1", "span_id":"a", "parent_id":"", "name":"GET /", "service":"gateway",
    "start_us":1000, "end_us":9000, "status":"ok", "attributes":{"http.method":"GET"} } ]
```

Spans missing `trace_id`/`span_id` are skipped; re-delivering a `span_id` overwrites (idempotent).

## Configuration (env)

| Var | Default | Purpose |
|-----|---------|---------|
| `BIND_ADDR` | `0.0.0.0:9230` | Listen address. |
| `FILAMENT_STORE` | `memory` | `memory` or `postgres`. |
| `FILAMENT_DATABASE_URL` | — | Required when `FILAMENT_STORE=postgres` (falls back to `DATABASE_URL`). |
| `FILAMENT_INGEST_TOKEN` | dev token | Bearer guarding `POST /ingest`. **Override in production.** |
| `FILAMENT_ERROR_SAMPLE_N` | `1` | Emit a `filament.trace.error` audit for 1-in-N error traces. |
| `AUDIT_ENABLED` | off | Enable the non-blocking Watchtower audit emitter. |
| `WATCHTOWER_URL` | — | e.g. `http://watchtower:8500`. |
| `AUDIT_INGEST_TOKEN` | — | Bearer for Watchtower ingest. |

## Audit

When an ingested trace contains an error span, Filament emits a sampled `filament.trace.error`
event (`source=filament`, `target=trace_id`) to Watchtower over the bounded-queue, fire-and-forget
emitter. A slow or down Watchtower never blocks, slows, or fails an ingest.

## Develop

```bash
cargo run                       # in-memory, boots zero-config on :9230
cargo check --all-targets
cargo test
```

## 前端 v2（2026-09-08）

Traces 列表与 Waterfall 按 Figma 文件 `aJo6MIddG56vwOZoCIC8fB`（Telemetry，midnight accent）
重做：套件栏、统计瓦片（traces / services / errors / p95）、服务 chip（6 个固定色槽，
由服务名哈希决定，列表与瀑布图与图例共用同一个色）、错误行着红、瀑布图标尺与
翻转的时长标签、右栏 Trace 事实表与最慢 span。样式在 `static/service.css`，
由 `/assets/filament-20260908.css` 以不可变缓存提供（测试会断言路径）。
