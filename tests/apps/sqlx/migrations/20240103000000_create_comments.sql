CREATE TABLE comments (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    post_id     bigint NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
    user_id     integer REFERENCES users (id) ON DELETE SET NULL,
    body        text NOT NULL,
    scores      integer[] NOT NULL DEFAULT '{}',
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX comments_post_id_idx ON comments (post_id, created_at DESC);
