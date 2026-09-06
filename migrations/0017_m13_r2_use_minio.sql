-- M13 改: R2 は MinIO/S3 でちゃんと実装した（worker→CP 内部→MinIO）。0016 で作った
-- Postgres の暫定テーブル r2_objects は不要になったので撤去する（未使用・空）。
-- 以後 R2 の本体は object storage（`r2/{tenant}/{bucket}/{key}`）に保持する。
DROP TABLE IF EXISTS r2_objects;
