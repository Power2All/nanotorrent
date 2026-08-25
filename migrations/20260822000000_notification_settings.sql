/* Desktop notifications for finished downloads.

   On by default: this is the one event worth interrupting for, and the Win32
   build has been firing it since before the setting existed - defaulting to
   false would silently switch it off for people already using it.

   Separate from `show_in_notification_area`, which is the tray icon. A toast
   goes to the Action Center whether or not a tray icon is visible, so the two
   are unrelated despite the similar names. */

INSERT INTO setting (key, value, default_value)
VALUES ('notifications.download_complete', NULL, 'true');
