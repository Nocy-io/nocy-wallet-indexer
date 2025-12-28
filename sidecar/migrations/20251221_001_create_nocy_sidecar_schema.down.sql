-- Down migration: drop session_profile table and schema
DROP TABLE IF EXISTS nocy_sidecar.session_profile;
DROP SCHEMA IF EXISTS nocy_sidecar;
