//! Keys of a TOML file that kraal does not read: a misspelt or renamed setting, or one that only
//! another version of kraal knows. They are warned about, never refused: one file may serve
//! machines that run different versions.

use serde::Deserializer;
use serde::de::value::{Error, MapDeserializer, SeqDeserializer};
use serde::de::{DeserializeOwned, Error as _, IntoDeserializer, Visitor};

/// One step down into a document: to the value of a table's key, or to an entry of a list.
enum Step {
    Key(String),
    Index(usize),
}

/// `T` as `T::deserialize(source)` reads it, and a warning for each key of `source` that `T`
/// does not read: its path in the file (`bar.kyes`; `server[0].jmp` in the first `[[server]]`)
/// and, when one is close, the key it was probably meant to be.
pub fn deserialize<'de, T, D>(source: D) -> Result<(T, Vec<String>), D::Error>
where
    T: DeserializeOwned,
    D: Deserializer<'de>,
{
    let mut unknown = Vec::new();
    let value = serde_ignored::deserialize(source, |path| unknown.push(steps(&path)))?;
    Ok((value, unknown.iter().map(|path| warning::<T>(path)).collect()))
}

/// [`deserialize`] from the text of a TOML file.
pub fn from_toml<T: DeserializeOwned>(text: &str) -> Result<(T, Vec<String>), toml::de::Error> {
    deserialize(toml::Deserializer::parse(text)?)
}

fn steps(path: &serde_ignored::Path) -> Vec<Step> {
    use serde_ignored::Path;
    let mut steps = Vec::new();
    let mut at = path;
    loop {
        at = match at {
            Path::Root => break,
            Path::Map { parent, key } => {
                steps.push(Step::Key(key.clone()));
                parent
            }
            Path::Seq { parent, index } => {
                steps.push(Step::Index(*index));
                parent
            }
            // An optional or wrapped value is written where it stands in the file.
            Path::Some { parent } | Path::NewtypeStruct { parent } | Path::NewtypeVariant { parent } => parent,
        };
    }
    steps.reverse();
    steps
}

fn warning<T: DeserializeOwned>(path: &[Step]) -> String {
    let mut shown = String::new();
    for step in path {
        match step {
            Step::Key(key) if shown.is_empty() => shown.push_str(key),
            Step::Key(key) => shown.push_str(&format!(".{key}")),
            Step::Index(index) => shown.push_str(&format!("[{index}]")),
        }
    }
    let meant = match path.split_last() {
        Some((Step::Key(key), table)) => closest(key, keys_at::<T>(table)),
        _ => None,
    };
    match meant {
        Some(known) => format!("{shown}: unknown key (did you mean {known}?)"),
        None => format!("{shown}: unknown key"),
    }
}

/// The key of `known` that `key` most likely misspells: the nearest one, when at most a third of
/// the letters differ (two letters swapped count once).
fn closest<'a>(key: &str, known: &[&'a str]) -> Option<&'a str> {
    let letters = |s: &str| s.chars().count();
    known
        .iter()
        .map(|k| (strsim::osa_distance(key, k), *k))
        .filter(|(distance, k)| *distance <= letters(key).max(letters(k)) / 3)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, k)| k)
}

/// The keys of the table `path` leads to in a `T`, when a struct reads that table.
fn keys_at<T: DeserializeOwned>(path: &[Step]) -> &'static [&'static str] {
    let mut keys: &[&str] = &[];
    // Always an error: the probe has no data to make a `T` of.
    let _ = T::deserialize(Probe { path, keys: &mut keys });
    keys
}

/// The keys `T` reads in the table that the keys of `path` lead to, when a struct reads it.
#[cfg(test)]
pub fn keys_of<T: DeserializeOwned>(path: &[&str]) -> &'static [&'static str] {
    let path: Vec<Step> = path.iter().map(|key| Step::Key(key.to_string())).collect();
    keys_at::<T>(&path)
}

/// A deserializer without data, which leads a type down `path` (through its structs, maps, lists
/// and options) to learn the field names of the struct found there: serde's derive gives them
/// when it asks for a struct. Only hints come from it, so an approximation is fine: a type it
/// cannot follow yields no names.
struct Probe<'a> {
    path: &'a [Step],
    keys: &'a mut &'static [&'static str],
}

impl<'de> Deserializer<'de> for Probe<'_> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Error> {
        Err(Error::custom("the path leads no further"))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        if self.path.is_empty() {
            *self.keys = fields;
        }
        self.deserialize_map(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.path.split_first() {
            Some((Step::Key(key), rest)) => {
                let value = Probe { path: rest, keys: self.keys };
                visitor.visit_map(MapDeserializer::new(std::iter::once((key.as_str(), value))))
            }
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.path.split_first() {
            Some((Step::Index(_), rest)) => {
                let entry = Probe { path: rest, keys: self.keys };
                visitor.visit_seq(SeqDeserializer::new(std::iter::once(entry)))
            }
            _ => self.deserialize_any(visitor),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _: &'static str, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf unit
        unit_struct tuple tuple_struct enum identifier ignored_any
    }
}

impl<'de> IntoDeserializer<'de> for Probe<'_> {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct File {
        scrollback: usize,
        #[serde(default)]
        bar: Bar,
        #[serde(default)]
        proxy: Option<Bar>,
        #[serde(default, rename = "server")]
        servers: Vec<Server>,
        #[serde(default)]
        tools: BTreeMap<String, Server>,
        #[serde(default)]
        hooks: toml::Table,
    }

    #[derive(Debug, Default, Deserialize)]
    struct Bar {
        #[serde(default, alias = "shortcuts")]
        keys: String,
        #[serde(default)]
        width: u16,
    }

    #[derive(Debug, Deserialize)]
    struct Server {
        name: String,
        #[serde(default)]
        jump: Option<String>,
    }

    fn read(text: &str) -> (File, Vec<String>) {
        super::from_toml(text).unwrap()
    }

    #[test]
    fn what_is_read_is_not_warned_about() {
        let (file, warnings) = read(
            "scrollback = 10\n[bar]\nshortcuts = \"context\"\nwidth = 3\n[proxy]\nkeys = \"off\"\n\
             [[server]]\nname = \"web-1\"\njump = \"bastion\"\n[tools.lazygit]\nname = \"git\"\n\
             [hooks]\n\"agent.blocked\" = \"true\"\nanything = 1\n",
        );
        assert_eq!(warnings, [] as [&str; 0]);
        // A key read under another name is read.
        assert_eq!((file.scrollback, file.bar.keys.as_str(), file.bar.width), (10, "context", 3));
        assert_eq!(file.proxy.unwrap().keys, "off");
        assert_eq!((file.servers[0].name.as_str(), file.servers[0].jump.as_deref()), ("web-1", Some("bastion")));
        assert_eq!((file.tools["lazygit"].name.as_str(), file.hooks.len()), ("git", 2));
    }

    #[test]
    fn unknown_keys_are_named_by_their_path_with_the_key_they_resemble() {
        let (file, warnings) = read(
            "scrolback = 1\nscrollback = 10\n[bar]\nkyes = \"off\"\ncolour = \"red\"\n[sidebar]\nwidth = 3\n\
             [proxy]\nwidht = 4\n[[server]]\nname = \"web-1\"\n[[server]]\nname = \"web-2\"\njmp = \"bastion\"\n\
             [tools.lazygit]\nname = \"git\"\nnmae = \"x\"\n",
        );
        assert_eq!(
            warnings,
            [
                // Nothing close: no guess.
                "bar.colour: unknown key",
                "bar.kyes: unknown key (did you mean keys?)",
                "proxy.widht: unknown key (did you mean width?)",
                "scrolback: unknown key (did you mean scrollback?)",
                "server[1].jmp: unknown key (did you mean jump?)",
                // A whole table that nothing reads is one unknown key.
                "sidebar: unknown key",
                "tools.lazygit.nmae: unknown key (did you mean name?)",
            ]
        );
        // The rest of the file is read as if they were not there.
        assert_eq!((file.scrollback, file.bar.keys.as_str(), file.servers.len()), (10, "", 2));
    }

    #[test]
    fn a_file_that_cannot_be_read_is_still_an_error() {
        assert!(super::from_toml::<File>("scrollback = \"many\"\nscrolback = 1\n").is_err());
        assert!(super::from_toml::<File>("scrollback = ").is_err());
    }

    #[test]
    fn only_a_close_key_is_suggested() {
        let known = ["keys", "prefix", "preset", "on"];
        assert_eq!(super::closest("kyes", &known), Some("keys"));
        assert_eq!(super::closest("prefx", &known), Some("prefix"));
        assert_eq!(super::closest("Preset", &known), Some("preset"));
        assert_eq!(super::closest("no", &known), None);
        assert_eq!(super::closest("leader", &known), None);
        // Letters count, not bytes: four letters that are all different are not close.
        assert_eq!(super::closest("клюѕ", &known), None);
    }
}
