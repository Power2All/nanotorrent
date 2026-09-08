/* Whether the one-time offer to encrypt the settings database has been made.

   The encryption state itself is deliberately NOT stored here. It cannot be:
   this row lives inside the database whose protection it would be describing,
   so reading it would require already knowing the answer. The file system is
   the record instead - a key file beside the database means encrypted, and no
   key file means either plain or password-protected, which one attempted open
   tells apart. That also means the setting can never disagree with reality,
   which a stored mode absolutely would the first time someone restored an old
   profile over a new one.

   What is stored is only whether the question has been asked, because a nag
   the user has already declined must not come back on every start. */

INSERT INTO setting (key, value, default_value)
SELECT 'database.encryption_prompted', NULL, 'false'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'database.encryption_prompted');
