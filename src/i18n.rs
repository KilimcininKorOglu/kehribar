//! User interface translations. The catalogs are compiled into the binary.

use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;

text_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Lang {
        Tr => "tr",
        En => "en",
    }
}

impl Lang {
    /// Picks the first supported language of an `Accept-Language` header.
    pub fn from_accept_language(header: &str) -> Option<Self> {
        header.split(',').find_map(|part| {
            let tag = part.split(';').next().unwrap_or_default().trim();
            let primary = tag.split('-').next().unwrap_or_default();
            Self::parse(&primary.to_ascii_lowercase())
        })
    }
}

#[derive(Debug, Error)]
#[error("locale file {file} is invalid: {reason}")]
pub struct LocaleError {
    file: &'static str,
    reason: String,
}

/// All translations, keyed by `section.name`.
#[derive(Debug)]
pub struct Catalog {
    tr: HashMap<String, String>,
    en: HashMap<String, String>,
}

impl Catalog {
    pub fn load() -> Result<Self, LocaleError> {
        Ok(Self {
            tr: parse("tr.toml", include_str!("../locales/tr.toml"))?,
            en: parse("en.toml", include_str!("../locales/en.toml"))?,
        })
    }

    fn messages(&self, lang: Lang) -> &HashMap<String, String> {
        match lang {
            Lang::Tr => &self.tr,
            Lang::En => &self.en,
        }
    }
}

/// Translations of one language.
#[derive(Debug, Clone)]
pub struct Tr {
    catalog: Arc<Catalog>,
    lang: Lang,
}

impl Tr {
    pub fn new(catalog: Arc<Catalog>, lang: Lang) -> Self {
        Self { catalog, lang }
    }

    pub fn lang(&self) -> Lang {
        self.lang
    }

    /// Returns the message for `key`, or the key itself when it is missing.
    /// A test checks that every key the code uses exists in both catalogs.
    pub fn t<'a>(&'a self, key: &'a str) -> &'a str {
        self.catalog
            .messages(self.lang)
            .get(key)
            .map_or(key, String::as_str)
    }

    /// Returns the message for `key` with every `{name}` replaced by its value.
    pub fn tf(&self, key: &str, args: &[(&str, &str)]) -> String {
        args.iter()
            .fold(self.t(key).to_owned(), |text, (name, value)| {
                text.replace(&format!("{{{name}}}"), value)
            })
    }
}

fn parse(file: &'static str, source: &str) -> Result<HashMap<String, String>, LocaleError> {
    let table: toml::Table = toml::from_str(source).map_err(|err| LocaleError {
        file,
        reason: err.to_string(),
    })?;
    let mut messages = HashMap::new();
    flatten(file, "", &table, &mut messages)?;
    Ok(messages)
}

fn flatten(
    file: &'static str,
    prefix: &str,
    table: &toml::Table,
    out: &mut HashMap<String, String>,
) -> Result<(), LocaleError> {
    for (name, value) in table {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        match value {
            toml::Value::String(text) => {
                out.insert(key, text.clone());
            }
            toml::Value::Table(inner) => flatten(file, &key, inner, out)?,
            _ => {
                return Err(LocaleError {
                    file,
                    reason: format!("{key} must be a string or a table"),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    fn catalog() -> Arc<Catalog> {
        Arc::new(Catalog::load().unwrap())
    }

    #[test]
    fn both_catalogs_define_the_same_keys() {
        let catalog = catalog();
        let tr: BTreeSet<_> = catalog.tr.keys().collect();
        let en: BTreeSet<_> = catalog.en.keys().collect();
        let only_tr: Vec<_> = tr.difference(&en).collect();
        let only_en: Vec<_> = en.difference(&tr).collect();
        assert!(only_tr.is_empty(), "missing in en.toml: {only_tr:?}");
        assert!(only_en.is_empty(), "missing in tr.toml: {only_en:?}");
    }

    fn files_with_ext(dir: &Path, ext: &str, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_with_ext(&path, ext, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
                out.push(path);
            }
        }
    }

    /// A string literal is a message key when it has the form `section.name`
    /// and `section` is a section of the catalog. This also catches keys that
    /// code passes around before the lookup, such as `"login.failed"`.
    fn is_key_literal(text: &str, sections: &BTreeSet<&str>) -> bool {
        let Some((section, name)) = text.split_once('.') else {
            return false;
        };
        let word =
            |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        // File names such as "home.html" share their first part with sections.
        let file_ext = matches!(name, "html" | "css" | "js" | "toml" | "rs" | "db" | "sql");
        word(section) && word(name) && !file_ext && sections.contains(section)
    }

    fn used_keys(sections: &BTreeSet<&str>) -> BTreeSet<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        files_with_ext(&root.join("templates"), "html", &mut files);
        files_with_ext(&root.join("src"), "rs", &mut files);
        let mut keys = BTreeSet::new();
        for path in files {
            let text = std::fs::read_to_string(&path).unwrap();
            // Every second piece of a split on `"` lies inside a string literal.
            for literal in text.split('"').skip(1).step_by(2) {
                if is_key_literal(literal, sections) {
                    keys.insert(literal.to_owned());
                }
            }
        }
        keys
    }

    #[test]
    fn every_key_used_in_templates_and_code_exists() {
        let catalog = catalog();
        let sections: BTreeSet<&str> = catalog
            .tr
            .keys()
            .filter_map(|key| key.split_once('.').map(|(section, _)| section))
            .collect();
        let keys = used_keys(&sections);
        assert!(keys.contains("nav.repos"), "the key scan found nothing");
        let missing: Vec<_> = keys
            .iter()
            .filter(|key| !catalog.tr.contains_key(*key) || !catalog.en.contains_key(*key))
            .collect();
        assert!(
            missing.is_empty(),
            "keys missing from a catalog: {missing:?}"
        );
    }

    #[test]
    fn lookup_uses_the_language_and_falls_back_to_the_key() {
        let catalog = catalog();
        assert_eq!(Tr::new(catalog.clone(), Lang::Tr).t("nav.logout"), "Çıkış");
        assert_eq!(
            Tr::new(catalog.clone(), Lang::En).t("nav.logout"),
            "Sign out"
        );
        assert_eq!(Tr::new(catalog, Lang::En).t("no.such.key"), "no.such.key");
    }

    #[test]
    fn accept_language_picks_the_first_supported_tag() {
        assert_eq!(
            Lang::from_accept_language("en-US,en;q=0.9,tr;q=0.8"),
            Some(Lang::En)
        );
        assert_eq!(
            Lang::from_accept_language("de-DE, tr-TR;q=0.5"),
            Some(Lang::Tr)
        );
        assert_eq!(Lang::from_accept_language("de, fr"), None);
    }
}
