//! Il modello di `options.txt`, nel formato vero del gioco.
//!
//! Non JSON: i file condivisi (globale e di modlist) si aprono con lo stesso
//! editor con cui si apre quello di un'istanza e si confrontano con `diff`
//! (D66). Il file è la verità: nessun valore di gioco finisce nel database.
//!
//! Il formato è una riga per impostazione, `chiave:valore`, dove il valore è
//! tutto quello che segue i **primi** due punti — `resourcePacks:["a","b"]` ha
//! un valore che contiene virgole e virgolette, e `key_key.hotbar.1` ha una
//! chiave che contiene punti.

use std::path::Path;

use anyhow::{Context, Result};

/// La chiave che porta la `DataVersion` del file.
///
/// Non è un'impostazione: è l'unico numero che dice al DataFixer del gioco da
/// dove partire a migrare. `Options.dataFix` la legge con default **0**, e un
/// file a DataVersion 0 si prende *tutti* i fix registrati, compreso
/// `OptionsKeyLwjgl3Fix` (DataVersion 1344), che rimappa i codici dei tasti da
/// LWJGL2 a LWJGL3. Per questo la semina non scrive mai un file senza.
pub const VERSION_KEY: &str = "version";

/// Un `options.txt` letto o costruito in memoria.
///
/// L'ordine delle righe è quello del file. Le righe vuote e quelle senza due
/// punti non vengono conservate: il gioco le salta, e noi riscriviamo solo
/// file che possediamo (globale e di modlist) o che stiamo creando.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OptionsFile {
    entries: Vec<(String, String)>,
}

impl OptionsFile {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn parse(text: &str) -> Self {
        let mut entries: Vec<(String, String)> = Vec::new();

        for raw_line in text.lines() {
            let line = raw_line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }

            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if key.is_empty() {
                continue;
            }

            entries.push((key.to_string(), value.to_string()));
        }

        Self { entries }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Ok(Self::parse(&text))
    }

    pub fn render(&self) -> String {
        let mut rendered = String::new();
        for (key, value) in &self.entries {
            rendered.push_str(key);
            rendered.push(':');
            rendered.push_str(value);
            rendered.push('\n');
        }
        rendered
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(path, self.render())
            .with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(entry_key, _)| entry_key == key)
            .map(|(_, value)| value.as_str())
    }

    /// La `DataVersion` del file, se c'è ed è un numero.
    pub fn data_version(&self) -> Option<i64> {
        self.get(VERSION_KEY)?.trim().parse::<i64>().ok()
    }

    pub fn set(&mut self, key: &str, value: &str) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(entry_key, _)| entry_key == key)
        {
            entry.1 = value.to_string();
            return;
        }
        self.entries.push((key.to_string(), value.to_string()));
    }

    /// Mette una riga in testa, dove il gioco scrive `version:`.
    pub fn set_first(&mut self, key: &str, value: &str) {
        self.entries.retain(|(entry_key, _)| entry_key != key);
        self.entries.insert(0, (key.to_string(), value.to_string()));
    }

    pub fn remove(&mut self, key: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(entry_key, _)| entry_key != key);
        before != self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_keeps_everything_after_the_first_colon() {
        let file = OptionsFile::parse("resourcePacks:[\"vanilla\",\"mod_resources\"]\n");

        assert_eq!(
            file.get("resourcePacks"),
            Some("[\"vanilla\",\"mod_resources\"]")
        );
    }

    #[test]
    fn keys_may_contain_dots_and_underscores() {
        let file = OptionsFile::parse("key_key.hotbar.1:key.keyboard.1\n");

        assert_eq!(file.get("key_key.hotbar.1"), Some("key.keyboard.1"));
    }

    #[test]
    fn blank_and_colonless_lines_are_dropped() {
        let file = OptionsFile::parse("fov:0.5\n\ngarbage\n\nguiScale:2\n");

        assert_eq!(file.len(), 2);
        assert_eq!(file.render(), "fov:0.5\nguiScale:2\n");
    }

    #[test]
    fn data_version_reads_the_version_line() {
        let file = OptionsFile::parse("version:3465\nfov:0.5\n");

        assert_eq!(file.data_version(), Some(3465));
        assert_eq!(OptionsFile::parse("fov:0.5\n").data_version(), None);
        assert_eq!(OptionsFile::parse("version:soon\n").data_version(), None);
    }

    #[test]
    fn set_first_puts_the_version_line_on_top() {
        let mut file = OptionsFile::parse("fov:0.5\nversion:1\n");
        file.set_first(VERSION_KEY, "3465");

        assert_eq!(file.render(), "version:3465\nfov:0.5\n");
    }

    #[test]
    fn round_trip_preserves_order_and_values() {
        let text = "version:3465\nfov:0.5\nkey_key.attack:key.mouse.left\n";

        assert_eq!(OptionsFile::parse(text).render(), text);
    }
}
