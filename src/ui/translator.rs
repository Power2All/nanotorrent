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

fn embedded(locale: &str) -> Option<&'static str> {
    EMBEDDED_LANGS
        .iter()
        .find(|(l, _)| l.eq_ignore_ascii_case(locale))
        .map(|(_, json)| *json)
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

fn parse_lang(json: &str) -> HashMap<String, String> {
    // serde_json rejects a leading UTF-8 BOM, and some of the original
    // language files carry one (es-ES) - without this they silently parse to
    // nothing and the UI falls back to English.
    let json = json.trim_start_matches('\u{feff}');
    serde_json::from_str::<HashMap<String, serde_json::Value>>(json)
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| {
                    // The language files come from the original project;
                    // rebrand any product-name mention.
                    v.as_str()
                        .map(|s| (k, s.replace("PicoTorrent", "NanoTorrent")))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Translator {
    pub fn load(lang_dir: &Path, locale: &str) -> Translator {
        let english = parse_lang(embedded("en-US").unwrap_or("{}"));

        // Start from what is compiled in, so a missing lang/ folder is fine.
        let mut locales: Vec<String> = EMBEDDED_LANGS.iter().map(|(l, _)| l.to_string()).collect();
        let mut strings = embedded(locale).map(parse_lang).unwrap_or_default();

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
                    strings = parse_lang(&contents);
                }
            }
        }

        let mut languages: Vec<Language> = locales
            .into_iter()
            .map(|loc| Language {
                name: loc.clone(),
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

    #[allow(dead_code)]
    pub fn get_locale(&self) -> &str {
        &self.selected_locale
    }

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
    use super::*;

    fn tr() -> Translator {
        // Missing dir -> embedded en-US loads as the fallback set.
        Translator::load(Path::new("no-such-lang-dir"), "en-US")
    }

    #[test]
    fn rebrands_product_name() {
        let m = parse_lang(r#"{"a": "About PicoTorrent", "b": "no brand"}"#);
        assert_eq!(m.get("a").unwrap(), "About NanoTorrent");
        assert_eq!(m.get("b").unwrap(), "no brand");
    }

    #[test]
    fn parse_lang_ignores_non_strings() {
        let m = parse_lang(r#"{"a": "x", "n": 5, "o": {"k": "v"}}"#);
        assert_eq!(m.len(), 1);
        assert!(m.contains_key("a"));
    }

    #[test]
    fn every_language_is_embedded_and_parses() {
        // No lang/ dir needed: the whole set is compiled in.
        assert!(EMBEDDED_LANGS.len() >= 40, "only {} embedded", EMBEDDED_LANGS.len());
        assert!(embedded("en-US").is_some());
        assert_eq!(tr().languages().len(), EMBEDDED_LANGS.len());

        // af-ZA and da-DK ship upstream as empty `{}` placeholders; everything
        // else must yield strings. Catches a BOM or a truncated file turning a
        // translation into a silent English fallback.
        for (locale, json) in EMBEDDED_LANGS {
            let n = parse_lang(json).len();
            if matches!(*locale, "af-ZA" | "da-DK") {
                continue;
            }
            assert!(n > 0, "{locale} parsed to nothing");
        }
    }

    #[test]
    fn bom_prefixed_language_file_still_parses() {
        assert_eq!(parse_lang("\u{feff}{\"a\": \"x\"}").get("a").map(String::as_str), Some("x"));
        // es-ES is the one that actually carries a BOM on disk.
        assert!(parse_lang(embedded("es-ES").unwrap()).len() > 100);
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
        assert_eq!(tr().i18n("dl_rate_limit"), "Dl rate limit");
    }

    #[test]
    fn known_key_resolves_from_english() {
        assert_eq!(tr().i18n("state_downloading"), "Downloading");
    }

    #[test]
    fn positional_args_substitute() {
        let t = tr();
        assert_eq!(t.i18n1("state_error", "boom"), "Error: boom");
        assert_eq!(t.i18n2("state_error_details", "boom", "42"), "Error: boom (42)");
    }
}
