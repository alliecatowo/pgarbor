CREATE TYPE user_role AS ENUM ('admin', 'member', 'guest');

CREATE TABLE users (
    id          serial PRIMARY KEY,
    email       varchar(255) NOT NULL UNIQUE,
    name        text NOT NULL,
    role        user_role NOT NULL DEFAULT 'member',
    active      boolean NOT NULL DEFAULT true,
    age         integer,
    created_at  timestamptz NOT NULL DEFAULT now()
);
