CREATE TABLE posts (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id   integer NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    title       varchar(200) NOT NULL,
    body        text,
    views       bigint NOT NULL DEFAULT 0,
    price       numeric(10, 2),
    tags        text[] NOT NULL DEFAULT '{}',
    meta        jsonb NOT NULL DEFAULT '{}'::jsonb,
    published   boolean NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT posts_author_title_key UNIQUE (author_id, title)
);

CREATE INDEX posts_author_id_idx ON posts (author_id);
