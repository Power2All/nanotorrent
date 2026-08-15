/* The original api.picotorrent.org endpoint is dead. Point the update check at
   this project's GitHub releases API instead - /releases/latest already skips
   drafts and prereleases, so the newest published release is what we compare
   against. Clears `value` as well as the default: migration 20200912230012
   copied the old picotorrent URL into `value`, so setting only the default
   would leave every existing database still polling the dead host. */
UPDATE setting
SET value = NULL,
    default_value = '"https://api.github.com/repos/Power2All/nanotorrent/releases/latest"'
WHERE key = 'update_checks.url';
