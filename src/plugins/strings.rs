//! A plugin's own translations, in a JSON file beside its script.
//!
//! NanoTorrent's own strings live in `lang/*.json`, 76 files that a plugin has
//! no business editing - a plugin shipped by somebody else cannot add a key to
//! them, and would not want its strings reviewed alongside the application's.
//! So a plugin brings its own: `player.rhai` is translated by
//! `player_translations.json` in the same folder, and `t("key")` reads it.
//!
//! The suffix, rather than a bare `player.json`: a plugins folder holds scripts
//! and whatever they keep beside themselves, and a file named after the script
//! reads like something the script wrote. This one is written FOR the script,
//! by whoever translated it.
//!
//! One file per plugin rather than one folder of them, because a plugin is one
//! file and stays one file to install, copy or delete. A folder to match would
//! double what "install this plugin" means.
//!
//! ```json
//! {
//!   "en-US": { "play": "Play", "cannot_play": "Nothing here can play {0}" },
//!   "nl-NL": { "play": "Afspelen", "cannot_play": "Hier kan {0} niet mee" }
//! }
//! ```
//!
//! Locale first, key second: a translator adds a language by adding one block,
//! rather than by finding every key and adding a line to each.
//!
//! No permission. This is the plugin's own file, sitting beside the plugin,
//! saying what the plugin already says - reading it grants nothing that writing
//! the strings inline would not have.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::core::configuration::Configuration;

/// What a plugin's `.json` holds: locale, then key, then text.
pub type Catalog = HashMap<String, HashMap<String, String>>;

/// What a plugin's catalogue is called, given its script.
///
/// `player.rhai` -> `player_translations.json`.
pub fn catalogue_path(script: &Path) -> std::path::PathBuf {
    let stem = script.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    script.with_file_name(format!("{stem}_translations.json"))
}

/// The strings one plugin can reach, and the settings to learn the language
/// from.
///
/// The locale is read at CALL time, not at load: the language can change while
/// NanoTorrent is running, and a plugin that had baked its locale in at load
/// would keep answering in the old one until something reloaded it.
#[derive(Clone)]
pub struct Strings {
    catalog: Arc<Catalog>,
    cfg: Arc<Configuration>,
}

/// The language a plugin falls back to, and the one its author writes first.
const FALLBACK: &str = crate::DEFAULT_LOCALE;

impl Strings {
    /// Read `<script>_translations.json`, if there is one.
    ///
    /// A plugin with no translations is the ordinary case - most scripts are
    /// somebody's own and have one language. Its absence is silent; a file that
    /// is there and will not parse is not, because that is a typo somebody
    /// wants to hear about.
    pub fn load(script: &Path, cfg: Arc<Configuration>) -> Strings {
        let path = catalogue_path(script);
        let catalog = match std::fs::read_to_string(&path) {
            Err(_) => Catalog::new(),
            Ok(text) => match serde_json::from_str::<Catalog>(&text) {
                Ok(c) => c,
                Err(err) => {
                    tracing::error!("plugin strings {}: {err}", path.display());
                    Catalog::new()
                }
            },
        };
        Strings {
            catalog: Arc::new(catalog),
            cfg,
        }
    }

    /// The text for a key in the current language.
    ///
    /// Falls back to [`FALLBACK`] and then to the key itself. The key rather
    /// than an empty string: a button labelled `play_in_vlc` is obviously a
    /// missing translation, where a button labelled nothing is a bug in the
    /// plugin nobody can diagnose from the outside.
    pub fn get(&self, key: &str) -> String {
        let locale = self
            .cfg
            .get_string("locale_name")
            .unwrap_or_else(|| String::from(FALLBACK));

        self.catalog
            .get(&locale)
            .and_then(|m| m.get(key))
            .or_else(|| self.catalog.get(FALLBACK).and_then(|m| m.get(key)))
            .cloned()
            .unwrap_or_else(|| key.to_owned())
    }

    /// The same, with `{0}` and `{1}` replaced.
    ///
    /// Two and no more. Two is what the shipped RSS reader needs - "Checking 3
    /// feeds against 5 rules" - and a message with three moving parts is
    /// usually two messages. Concatenating instead is not an option worth
    /// leaving open: word order is exactly what differs between languages, and
    /// a string built by `+` can only come out in the order the author typed.
    pub fn get2(&self, key: &str, a: &str, b: &str) -> String {
        self.get(key).replace("{0}", a).replace("{1}", b)
    }

    /// One argument, for the strings that take one.
    pub fn get1(&self, key: &str, arg: &str) -> String {
        self.get(key).replace("{0}", arg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_locale(locale: &str) -> Arc<Configuration> {
        let db = Arc::new(crate::core::database::Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db);
        cfg.set("locale_name", &locale);
        Arc::new(cfg)
    }

    fn strings(json: &str, locale: &str) -> Strings {
        Strings {
            catalog: Arc::new(serde_json::from_str(json).expect("test catalog parses")),
            cfg: cfg_with_locale(locale),
        }
    }

    const CATALOG: &str = r#"{
        "en-US": { "play": "Play", "cannot": "Cannot play {0}" },
        "nl-NL": { "play": "Afspelen" }
    }"#;

    #[test]
    fn a_string_comes_back_in_the_current_language() {
        assert_eq!(strings(CATALOG, "nl-NL").get("play"), "Afspelen");
        assert_eq!(strings(CATALOG, "en-US").get("play"), "Play");
    }

    /// A half-translated plugin is the normal state of a translated plugin, and
    /// it must not leave holes in its own window.
    #[test]
    fn a_missing_translation_falls_back_to_english_then_to_the_key() {
        let nl = strings(CATALOG, "nl-NL");
        assert_eq!(nl.get("cannot"), "Cannot play {0}", "no Dutch for this one");
        assert_eq!(
            nl.get("never_written"),
            "never_written",
            "a key nobody translated reads as itself, not as nothing"
        );

        // A language the plugin has never heard of behaves like a missing key
        // rather than like an error.
        assert_eq!(strings(CATALOG, "ja-JP").get("play"), "Play");
    }

    #[test]
    fn the_argument_is_substituted_wherever_the_language_puts_it() {
        let en = strings(
            r#"{"en-US": {"x": "Cannot play {0}"}, "de-DE": {"x": "{0} kann nicht"}}"#,
            "de-DE",
        );
        assert_eq!(en.get1("x", "ep01.mkv"), "ep01.mkv kann nicht");
        // And a string with no placeholder is not broken by passing one.
        assert_eq!(strings(CATALOG, "en-US").get1("play", "ignored"), "Play");
    }

    /// Two arguments, and the reason they are numbered rather than positional:
    /// the order they appear in is the language's business, not the author's.
    #[test]
    fn two_arguments_may_swap_places_between_languages() {
        let s = strings(
            r#"{
                "en-US": {"c": "Checking {0} feeds against {1} rules"},
                "ja-JP": {"c": "{1} 個のルールに対して {0} 個のフィードを確認中"}
            }"#,
            "ja-JP",
        );
        assert_eq!(s.get2("c", "3", "5"), "5 個のルールに対して 3 個のフィードを確認中");

        let s = strings(
            r#"{"en-US": {"c": "Checking {0} feeds against {1} rules"}}"#,
            "en-US",
        );
        assert_eq!(s.get2("c", "3", "5"), "Checking 3 feeds against 5 rules");
    }

    /// The claim the design rests on: the locale is read when a string is
    /// ASKED for, not when the plugin loaded. Somebody changing the language in
    /// Preferences must not have to restart, or reload their plugins, to see a
    /// plugin's own window follow.
    #[test]
    fn a_language_change_is_picked_up_without_reloading() {
        let cfg = cfg_with_locale("en-US");
        let s = Strings {
            catalog: Arc::new(serde_json::from_str(CATALOG).unwrap()),
            cfg: cfg.clone(),
        };
        assert_eq!(s.get("play"), "Play");

        // The same Strings, no reload, no new engine.
        cfg.set("locale_name", &"nl-NL");
        assert_eq!(s.get("play"), "Afspelen");

        // And back, because a setting that only moves one way is not a setting.
        cfg.set("locale_name", &"en-US");
        assert_eq!(s.get("play"), "Play");
    }

    /// No file is the ordinary case: most plugins are somebody's own and have
    /// one language. It must not be an error, and every key must still answer.
    #[test]
    fn a_plugin_with_no_json_answers_with_its_keys() {
        let dir = std::env::temp_dir().join(format!("nt-strings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("lonely.rhai");
        std::fs::write(&script, "// no strings here").unwrap();

        let s = Strings::load(&script, cfg_with_locale("fr-FR"));
        assert_eq!(s.get("anything"), "anything");

        // And one that will not parse is the same to the plugin - it says so in
        // the log rather than taking the plugin down.
        std::fs::write(dir.join("lonely_translations.json"), "{ not json").unwrap();
        let s = Strings::load(&script, cfg_with_locale("fr-FR"));
        assert_eq!(s.get("anything"), "anything");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_is_the_script_name_plus_translations() {
        assert_eq!(
            catalogue_path(Path::new("/plugins/player.rhai")),
            Path::new("/plugins/player_translations.json"),
        );

        let dir = std::env::temp_dir().join(format!("nt-strings2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("player.rhai"), "// x").unwrap();
        // Deliberately also writing the name it is NOT, so a reader that fell
        // back to `player.json` would pick up the wrong one and fail here.
        std::fs::write(dir.join("player.json"), r#"{"nl-NL":{"play":"WRONG"}}"#).unwrap();
        std::fs::write(dir.join("player_translations.json"), CATALOG).unwrap();

        let s = Strings::load(&dir.join("player.rhai"), cfg_with_locale("nl-NL"));
        assert_eq!(s.get("play"), "Afspelen");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
