CREATE TABLE users (
    id         BIGSERIAL PRIMARY KEY,
    email      TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE notes (
    id      BIGSERIAL PRIMARY KEY,
    user_id BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    title   TEXT NOT NULL,
    body    TEXT NOT NULL DEFAULT ''
);

-- TODO: full-text index on notes.body
CREATE INDEX notes_user_id_idx ON notes (user_id);
