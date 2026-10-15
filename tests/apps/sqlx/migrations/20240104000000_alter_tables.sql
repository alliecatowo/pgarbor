ALTER TABLE users ADD COLUMN bio text;
ALTER TABLE users ADD COLUMN settings jsonb NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE posts ALTER COLUMN title TYPE varchar(300);
ALTER TABLE posts ADD COLUMN slug text;
CREATE UNIQUE INDEX posts_slug_key ON posts (slug) WHERE slug IS NOT NULL;
ALTER TABLE comments ADD CONSTRAINT comments_body_nonempty CHECK (length(body) > 0);
