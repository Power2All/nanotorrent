// Port of src/picotorrent/ui/translator.{hpp,cpp}
//
// The original embedded the lang/*.json files into a SQLite database at
// build time; this port reads the very same JSON files from the lang/
// directory at startup. en-US is compiled in as fallback.

use std::collections::HashMap;
use std::path::Path;

const EN_US: &str = include_str!("../../lang/en-US.json");

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
        let english = parse_lang(EN_US);
        let mut languages = Vec::new();
        let mut strings = HashMap::new();

        if let Ok(entries) = std::fs::read_dir(lang_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }

                let Some(loc) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };

                languages.push(Language {
                    locale: loc.to_string(),
                    name: loc.to_string(),
                });

                if loc.eq_ignore_ascii_case(locale)
                    && let Ok(contents) = std::fs::read_to_string(&path)
                {
                    strings = parse_lang(&contents);
                }
            }
        }

        languages.sort_by(|a, b| a.locale.cmp(&b.locale));

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
