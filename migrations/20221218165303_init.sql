CREATE TABLE IF NOT EXISTS file_index
(
    fd   BLOB PRIMARY KEY NOT NULL,
    upload_ip BLOB NOT NULL,
    name TEXT
);
