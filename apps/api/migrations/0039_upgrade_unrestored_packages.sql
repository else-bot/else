-- A JSON array of the APT packages the bot had installed that the new
-- computer could not install again; NULL when every package came back.
ALTER TABLE computer_upgrades ADD COLUMN unrestored_packages TEXT;
