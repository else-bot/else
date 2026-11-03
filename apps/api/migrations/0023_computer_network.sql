-- POC-025 review: the network profile a computer was created with, so an
-- image upgrade keeps it rather than taking the current default. NULL for
-- computers created before it was recorded; their upgrade needs an explicit
-- profile.
ALTER TABLE bot_computers ADD COLUMN network text CHECK (network <> '' AND octet_length(network) <= 100);
ALTER TABLE computer_upgrades ADD COLUMN from_network text, ADD COLUMN to_network text;
