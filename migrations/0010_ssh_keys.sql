-- The SSH key that fetches the repositories of a host with an ssh:// URL.
-- The private key is encrypted like the tokens; the public key is shown.
ALTER TABLE hosts ADD COLUMN ssh_key_enc BLOB;
ALTER TABLE hosts ADD COLUMN ssh_public_key TEXT;
