// Port of src/picotorrent/ui/translator.{hpp,cpp}
//
// The original embedded the lang/*.json files into a SQLite database at
// build time; this port compiles the very same JSON files straight into the
// .exe, so it runs standalone with every translation available. A lang/
// directory next to the executable still wins per-locale, which keeps
// translations editable without a rebuild.

use std::collections::HashMap;
use std::path::Path;

// pub static EMBEDDED_LANGS: &[(&str, &str)] - locale -> file contents, built
// from lang/*.json by build.rs.
include!(concat!(env!("OUT_DIR"), "/lang_table.rs"));

/// The compiled-in JSON for a locale, if it ships with this build.
fn embedded(locale: &str) -> Option<&'static str> {
    EMBEDDED_LANGS
        .iter()
        .find(|(l, _)| l.eq_ignore_ascii_case(locale))
        .map(|(_, json)| *json)
}

/// Locale code -> what speakers of that language call it.
///
/// Endonyms, not English names: a language picker is read by someone who wants
/// their own language, and "Nederlands" is what they are looking for, not
/// "Dutch" and certainly not "nl-NL". Anything not listed falls back to its
/// locale code, so adding a lang/*.json without touching this still works.
const ENDONYMS: [(&str, &str); 41] = [
    ("af-ZA", "Afrikaans"),
    ("ar-SA", "العربية"),
    ("bg-BG", "Български"),
    ("ca-ES", "Català"),
    ("cs-CZ", "Čeština"),
    ("da-DK", "Dansk"),
    ("de-DE", "Deutsch"),
    ("el-GR", "Ελληνικά"),
    ("en-US", "English"),
    ("es-ES", "Español"),
    ("et-EE", "Eesti"),
    ("fi-FI", "Suomi"),
    ("fr-FR", "Français"),
    ("he-IL", "עברית"),
    ("hi-IN", "हिन्दी"),
    ("hr-HR", "Hrvatski"),
    ("hu-HU", "Magyar"),
    ("hy-AM", "Հայերեն"),
    ("id-ID", "Bahasa Indonesia"),
    ("it-IT", "Italiano"),
    ("ja-JP", "日本語"),
    ("ka-GE", "ქართული"),
    ("ko-KR", "한국어"),
    ("lt-LT", "Lietuvių"),
    ("lv-LV", "Latviešu"),
    ("nb-NO", "Norsk bokmål"),
    ("nl-NL", "Nederlands"),
    ("pl-PL", "Polski"),
    ("pt-BR", "Português (Brasil)"),
    ("pt-PT", "Português (Portugal)"),
    ("ro-RO", "Română"),
    ("ru-RU", "Русский"),
    ("si-LK", "සිංහල"),
    ("sk-SK", "Slovenčina"),
    ("sr-SP", "Српски"),
    ("sv-SE", "Svenska"),
    ("tr-TR", "Türkçe"),
    ("uk-UA", "Українська"),
    ("vi-VN", "Tiếng Việt"),
    ("zh-CN", "简体中文"),
    ("zh-TW", "繁體中文"),
];

/// The native name for a locale, or the code itself when it is not known.
fn endonym(locale: &str) -> &str {
    ENDONYMS
        .iter()
        .find(|(l, _)| l.eq_ignore_ascii_case(locale))
        .map_or(locale, |(_, name)| *name)
}

#[derive(Clone)]
pub struct Language {
    pub locale: String,
    pub name: String,
}

#[derive(Clone)]
pub struct Translator {
    selected_locale: String,
    strings: HashMap<String, String>,
    english: HashMap<String, String>,
    languages: Vec<Language>,
}

/// Flatten one language file into key -> text.
///
/// Malformed entries are skipped rather than failing the load: a translation
/// with one bad line should lose that line, not the language.
///
/// `rebrand` renames the product in the inherited translations, which are
/// PicoTorrent's originals and say so in a dozen strings. It must be false for
/// en-US: that file is ours and already says what it means, including the one
/// place the old name is deliberate - the About box credits the project this
/// is a port OF, and blanket-renaming turned it into "a Rust port of
/// NanoTorrent".
fn parse_lang(json: &str, rebrand: bool) -> HashMap<String, String> {
    // Never renamed, in any file. Today only en-US defines it, so the flag
    // above is enough - but the day someone translates the credit, renaming it
    // would silently restore "a Rust port of NanoTorrent" in that language
    // alone, which is the hardest kind of bug to notice.
    const ABOUT_UPSTREAM: [&str; 1] = ["app_credit"];

    // serde_json rejects a leading UTF-8 BOM, and some of the original
    // language files carry one (es-ES) - without this they silently parse to
    // nothing and the UI falls back to English.
    let json = json.trim_start_matches('\u{feff}');
    serde_json::from_str::<HashMap<String, serde_json::Value>>(json)
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| {
                    v.as_str().map(|s| {
                        let s = if rebrand && !ABOUT_UPSTREAM.contains(&k.as_str()) {
                            s.replace("PicoTorrent", "NanoTorrent")
                        } else {
                            s.to_string()
                        };
                        (k, s)
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Translator {
    /// Load a locale, preferring a file in `lang_dir` over the compiled-in
    /// copy so a translation can be edited without a rebuild.
    ///
    /// Always succeeds: an unknown or unreadable locale falls back to the
    /// embedded en-US, because a missing translation must not stop startup.
    pub fn load(lang_dir: &Path, locale: &str) -> Translator {
        // Never rebranded: en-US is ours, not an inherited translation.
        let english = parse_lang(embedded("en-US").unwrap_or("{}"), false);

        // Start from what is compiled in, so a missing lang/ folder is fine.
        let mut locales: Vec<String> = EMBEDDED_LANGS.iter().map(|(l, _)| l.to_string()).collect();
        let is_english = locale.eq_ignore_ascii_case(crate::DEFAULT_LOCALE);
        let mut strings = embedded(locale)
            .map(|json| parse_lang(json, !is_english))
            .unwrap_or_default();

        // A lang/ directory on disk overrides the embedded copy per locale and
        // may add locales that did not ship with this build.
        if let Ok(entries) = std::fs::read_dir(lang_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }

                let Some(loc) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };

                if !locales.iter().any(|l| l.eq_ignore_ascii_case(loc)) {
                    locales.push(loc.to_string());
                }

                if loc.eq_ignore_ascii_case(locale)
                    && let Ok(contents) = std::fs::read_to_string(&path)
                {
                    // Same rule for a lang/ override as for the embedded
                    // copy: an en-US.json on disk is a replacement for ours.
                    strings = parse_lang(&contents, !is_english);
                }
            }
        }

        let mut languages: Vec<Language> = locales
            .into_iter()
            .map(|loc| Language {
                name: endonym(&loc).to_string(),
                locale: loc,
            })
            .collect();
        // English first (it is the default and the fallback every other locale
        // resolves missing keys against), then the rest by locale code.
        languages.sort_by_key(|l| {
            (
                !l.locale.eq_ignore_ascii_case(crate::DEFAULT_LOCALE),
                l.locale.clone(),
            )
        });

        Translator {
            selected_locale: locale.to_string(),
            strings,
            english,
            languages,
        }
    }

    /// The locale actually loaded, which is not necessarily the one asked for.
    pub fn get_locale(&self) -> &str {
        &self.selected_locale
    }

    /// Every language this build can switch to, English first and the rest
    /// sorted - the order the Preferences list shows them in.
    pub fn languages(&self) -> &[Language] {
        &self.languages
    }

    /// Port of the i18n() macro. Keys missing from every language file are
    /// humanized ("dl_rate_limit" -> "Dl rate limit") instead of shown raw.
    pub fn i18n(&self, key: &str) -> String {
        self.strings
            .get(key)
            .or_else(|| self.english.get(key))
            .cloned()
            .unwrap_or_else(|| {
                let mut s = key.replace('_', " ");
                if let Some(first) = s.get_mut(0..1) {
                    first.make_ascii_uppercase();
                }
                s
            })
    }

    /// i18n with a single positional argument ({0} or {}).
    pub fn i18n1(&self, key: &str, arg: &str) -> String {
        let s = self.i18n(key);
        if s.contains("{0}") {
            s.replace("{0}", arg)
        } else {
            s.replacen("{}", arg, 1)
        }
    }

    /// i18n with two positional arguments.
    pub fn i18n2(&self, key: &str, arg0: &str, arg1: &str) -> String {
        let s = self.i18n(key);
        if s.contains("{0}") || s.contains("{1}") {
            s.replace("{0}", arg0).replace("{1}", arg1)
        } else {
            s.replacen("{}", arg0, 1).replacen("{}", arg1, 1)
        }
    }
}

#[cfg(test)]
mod tests {
    /// The About box credits the project this is a port OF, so that one string
    /// must keep saying PicoTorrent - a blanket rename turned it into "A Rust
    /// port of NanoTorrent", which is nonsense. Every other en-US string must
    /// already say NanoTorrent in the file itself rather than relying on that
    /// rename, and the inherited translations must still get renamed.
    #[test]
    fn only_the_credit_still_names_picotorrent() {
        let english = super::parse_lang(super::embedded("en-US").expect("en-US is embedded"), false);

        let credit = english.get("app_credit").expect("app_credit exists");
        assert!(credit.contains("PicoTorrent"), "the credit lost its subject: {credit}");
        assert!(!credit.contains("NanoTorrent"), "the credit was rebranded: {credit}");

        let stray: Vec<&String> = english
            .iter()
            .filter(|(k, v)| k.as_str() != "app_credit" && v.contains("PicoTorrent"))
            .map(|(k, _)| k)
            .collect();
        assert!(stray.is_empty(), "en-US should say NanoTorrent itself: {stray:?}");

        // A translated credit keeps its subject too. No locale defines one
        // today, so this is the guard rather than the current behaviour.
        let translated = super::parse_lang(
            r#"{"app_credit": "Een Rust-port van PicoTorrent.", "about_picotorrent": "Over PicoTorrent"}"#,
            true,
        );
        assert!(translated["app_credit"].contains("PicoTorrent"), "credit renamed");
        assert!(translated["about_picotorrent"].contains("NanoTorrent"), "title not renamed");

        // An inherited translation is still rebranded - that is what the flag
        // is for, and 40 files depend on it.
        let dutch = super::parse_lang(super::embedded("nl-NL").expect("nl-NL is embedded"), true);
        assert!(
            dutch.values().all(|v| !v.contains("PicoTorrent")),
            "inherited translations must be rebranded"
        );
    }

    use super::*;

    fn tr() -> Translator {
        // Missing dir -> embedded en-US loads as the fallback set.
        Translator::load(Path::new("no-such-lang-dir"), "en-US")
    }

    #[test]
    fn rebrands_product_name() {
        let m = parse_lang(r#"{"a": "About PicoTorrent", "b": "no brand"}"#, true);
        assert_eq!(m.get("a").unwrap(), "About NanoTorrent");
        assert_eq!(m.get("b").unwrap(), "no brand");
    }

    #[test]
    fn parse_lang_ignores_non_strings() {
        let m = parse_lang(r#"{"a": "x", "n": 5, "o": {"k": "v"}}"#, true);
        assert_eq!(m.len(), 1);
        assert!(m.contains_key("a"));
    }

    #[test]
    fn every_language_is_embedded_and_parses() {
        // No lang/ dir needed: the whole set is compiled in.
        assert!(
            EMBEDDED_LANGS.len() >= 40,
            "only {} embedded",
            EMBEDDED_LANGS.len()
        );
        assert!(embedded("en-US").is_some());
        assert_eq!(tr().languages().len(), EMBEDDED_LANGS.len());

        // af-ZA and da-DK ship upstream as empty `{}` placeholders; everything
        // else must yield strings. Catches a BOM or a truncated file turning a
        // translation into a silent English fallback.
        for (locale, json) in EMBEDDED_LANGS {
            let n = parse_lang(json, true).len();
            if matches!(*locale, "af-ZA" | "da-DK") {
                continue;
            }
            assert!(n > 0, "{locale} parsed to nothing");
        }
    }

    #[test]
    fn bom_prefixed_language_file_still_parses() {
        assert_eq!(
            parse_lang("\u{feff}{\"a\": \"x\"}", true)
                .get("a")
                .map(String::as_str),
            Some("x")
        );
        // es-ES is the one that actually carries a BOM on disk.
        assert!(parse_lang(embedded("es-ES").unwrap(), true).len() > 100);
    }

    #[test]
    fn english_is_first_in_the_language_list() {
        let t = tr();
        let langs = t.languages();
        assert_eq!(langs[0].locale, "en-US");
        // Everything after it stays sorted, and nothing is lost or duplicated.
        let rest: Vec<&str> = langs[1..].iter().map(|l| l.locale.as_str()).collect();
        let mut sorted = rest.clone();
        sorted.sort_unstable();
        assert_eq!(rest, sorted);
        assert_eq!(langs.len(), EMBEDDED_LANGS.len());
    }

    #[test]
    fn default_locale_starts_in_english() {
        // A fresh install must come up in English regardless of the OS locale.
        let t = Translator::load(Path::new("no-such-lang-dir"), crate::DEFAULT_LOCALE);
        assert_eq!(t.get_locale(), "en-US");
        assert_eq!(t.i18n("state_downloading"), "Downloading");
    }

    #[test]
    fn embedded_locale_resolves_without_a_lang_dir() {
        let t = Translator::load(Path::new("no-such-lang-dir"), "nl-NL");
        assert_eq!(t.get_locale(), "nl-NL");
        // A real Dutch string, so this fails if the table were English-only.
        assert_eq!(t.i18n("state_downloading"), "Downloaden");
    }

    #[test]
    fn missing_key_is_humanized() {
        // Deliberately not a real key: this used to use `dl_rate_limit`, which
        // then got added to en-US.json and quietly turned the test into an
        // assertion about a key that resolves.
        assert_eq!(tr().i18n("no_such_key_anywhere"), "No such key anywhere");
    }

    #[test]
    fn known_key_resolves_from_english() {
        assert_eq!(tr().i18n("state_downloading"), "Downloading");
    }

    #[test]
    fn positional_args_substitute() {
        let t = tr();
        assert_eq!(t.i18n1("state_error", "boom"), "Error: boom");
        assert_eq!(
            t.i18n2("state_error_details", "boom", "42"),
            "Error: boom (42)"
        );
    }
}

#[cfg(test)]
mod endonym_tests {
    use super::{ENDONYMS, endonym};

    /// Every shipped language file needs a native name, or the picker falls
    /// back to showing a locale code among real names - which is exactly the
    /// state this table was added to fix.
    #[test]
    fn every_embedded_locale_has_an_endonym() {
        let missing: Vec<&str> = super::EMBEDDED_LANGS
            .iter()
            .map(|(locale, _)| *locale)
            .filter(|locale| endonym(locale) == *locale)
            .collect();
        assert!(missing.is_empty(), "no native name for: {missing:?}");
    }

    #[test]
    fn unknown_locales_fall_back_to_the_code() {
        assert_eq!(endonym("xx-XX"), "xx-XX");
        // Case-insensitive, like every other locale comparison here.
        assert_eq!(endonym("NL-nl"), "Nederlands");
    }

    #[test]
    fn no_duplicate_locales_in_the_table() {
        let mut seen: Vec<&str> = ENDONYMS.iter().map(|(l, _)| *l).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate locale in ENDONYMS");
    }
}

#[cfg(test)]
mod slint_key_tests {
    /// Every `L.s("key")` in the .slint markup must name a real key.
    ///
    /// A typo does not fail the build - `i18n` humanizes anything it cannot
    /// find, so `L.s("cancle")` would quietly render "Cancle". This is the
    /// only thing that catches that.
    #[test]
    fn every_key_used_in_markup_exists() {
        let english = super::parse_lang(super::embedded("en-US").expect("en-US is embedded"), false);

        let mut missing: Vec<String> = Vec::new();
        for file in [
            include_str!("../ui_slint/app.slint"),
            include_str!("../ui_slint/preferences.slint"),
            include_str!("../ui_slint/about.slint"),
            include_str!("../ui_slint/create.slint"),
            include_str!("../ui_slint/closeprompt.slint"),
            include_str!("../ui_slint/tray.slint"),
        ] {
            for (_, rest) in file
                .match_indices("L.s(\"")
                .map(|(i, m)| (i, &file[i + m.len()..]))
            {
                let Some(key) = rest.split('"').next() else {
                    continue;
                };
                if !english.contains_key(key) && !missing.contains(&key.to_string()) {
                    missing.push(key.to_string());
                }
            }
        }
        assert!(
            missing.is_empty(),
            "keys not in lang/en-US.json: {missing:?}"
        );
    }
}
