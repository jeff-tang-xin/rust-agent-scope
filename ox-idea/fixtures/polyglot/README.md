# Acme (polyglot QA fixture)

A small monorepo touching many languages so OpenIDEA's language detection,
highlighting, Search Everywhere and per-language servers can be checked in one
window.

| Path | Language | Notes |
| --- | --- | --- |
| `api/` | Go | HTTP API (`go run ./api`) |
| `web/` | TypeScript, HTML, CSS | Front end (`pnpm --dir web dev`) |
| `worker/` | Python | Background worker |
| `native/` | C | Hash helper (`make native`) |
| `scripts/` | Shell | Deploy script |
| `config/` | TOML, YAML, JSON | Configuration |
| `db/` | SQL | Schema |
| `proto/` | Protocol Buffers | API contract |
| `infra/` | Terraform | Infrastructure |

<!-- TODO: document the release process -->
