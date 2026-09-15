-- Only the disposable acceptance-test database uses this schema.
CREATE TABLE hibana_extension_probe (
    id integer PRIMARY KEY,
    label text NOT NULL
);
