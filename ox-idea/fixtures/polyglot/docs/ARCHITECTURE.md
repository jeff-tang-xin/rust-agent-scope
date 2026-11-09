# Architecture

```text
web (TypeScript) ──HTTP──▶ api (Go) ──SQL──▶ postgres
                              │
                              └──queue──▶ worker (Python)
```

- The API contract lives in `proto/notes.proto`.
- Configuration is read from `config/app.toml`, overridable by `.env`.
