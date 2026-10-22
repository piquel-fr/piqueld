//! The settings reference pages, `docs/user/reference/manifest-settings.md`
//! and `docs/user/reference/configuration-settings.md`, rendered from the
//! application manifest's and the daemon configuration's JSON Schemas: a
//! section per TOML table, with an entry per setting giving its full path,
//! type, whether it is required or its default, and its description.
//!
//! Settings take their description from their doc comment and tables from
//! their type's, so every setting and table must have one. The description of
//! a table keyed by name, such as `environments`, names its key first, as in
//! ``Declares the environment `<environment>` ``. Tagged enums, such as
//! health checks, render as one table whose settings say which `type` they
//! apply to. Plain values with a type of their own, such as templates, link to
//! their description at the end of the page.

use anyhow::{Context, Result, anyhow, bail, ensure};
use piqueld_generated::Page;
use serde::{Deserialize, de::IgnoredAny};
use serde_json::Value;
use std::{collections::BTreeMap, fmt::Write as _};

/// A settings reference page: where it goes, and its hand-written parts.
pub struct SettingsPage {
    /// Where the page is written, relative to the repository root.
    pub path: &'static str,
    /// What the page is generated from, named in its do-not-edit comment.
    source: &'static str,
    title: &'static str,
    /// The introduction, after the title.
    intro: &'static str,
}

impl SettingsPage {
    /// Every setting of an application manifest.
    pub const MANIFEST: Self = Self {
        path: "docs/user/reference/manifest-settings.md",
        source: "the doc comments of the manifest types in `crates/piqueld-core/src/manifest`",
        title: "Manifest settings",
        intro: r#"This page lists every setting of an [application manifest](manifest.md), with
its type, whether it is required or its default, and what it does. It is
generated from the manifest's
[JSON Schema](manifest.md#validation-and-editor-support), so your editor shows
the same descriptions. For how the settings work together, read the
[application manifest](manifest.md) reference and the guides, such as
[HTTP ingress](../guides/ingress.md).

Each table's section is headed by the line that starts it in the file, such
as `[[spec.services]]`. Each setting is named by its full TOML path:

- `[]` stands for each entry of an array of tables: `spec.services[].name` is
  the `name` of each `[[spec.services]]`.
- A name in angle brackets, such as `<environment>`, stands for a key you
  choose: `spec.environments.<environment>` is each environment table, such as
  `[spec.environments.staging]`.

A [template](#template) is a string that may reference
[variables](manifest.md#variables), such as `"${{ vars.domain }}"`. Only
settings whose type includes a template accept references. Linked types like
it are described under [value types](#value-types), at the end of the page.

Names, such as those of applications, environments, services, volumes, jobs,
secrets and routes, are 1 to 63 lowercase letters, digits or hyphens. They
start with a letter and end with a letter or digit."#,
    };

    /// Every setting of the daemon's configuration file.
    pub const CONFIG: Self = Self {
        path: "docs/user/reference/configuration-settings.md",
        source: "the configuration types in `apps/piqueld/src/config`",
        title: "Daemon configuration settings",
        intro: "\
This page lists every setting of the daemon's configuration file,
`/etc/piqueld/config.toml`, with its type and default. It is generated from
the daemon's configuration types, like the file's
[JSON Schema](../../piqueld-config.schema.json), which editors that use
[Taplo](https://taplo.tamasfe.dev/) can check the file against. For how the
daemon uses the file, with examples, see
[daemon configuration](configuration.md); NixOS sets these settings through
the [NixOS module](nixos.md).

Each table's section is headed by the line that starts it in the file, such
as `[[dns.providers]]`. Each setting is named by its full TOML path, where `[]`
stands for each entry of an array of tables: `dns.providers[].kind` is the
`kind` of each `[[dns.providers]]`. Settings ending in `_file` take the path of a
[credential file](#credential-file): a relative path resolves against
`$CREDENTIALS_DIRECTORY`.",
    };

    /// Renders the page from `schema`, a standalone JSON Schema (see
    /// `piqueld_core::json_schema::standalone`).
    ///
    /// # Errors
    /// If the schema has a shape the page cannot show, or lacks a
    /// description the page needs.
    pub fn render(&self, schema: &Value) -> Result<String> {
        let document = Document::deserialize(schema)
            .with_context(|| format!("read the JSON Schema of {}", self.path))?;
        Renderer {
            definitions: &document.definitions,
            page: Page::new(self.source, self.title, self.intro),
            value_types: BTreeMap::new(),
        }
        .render(&document.root)
        .with_context(|| format!("render {}", self.path))
    }
}

/// A standalone JSON Schema: the root schema and the definitions its `$ref`s
/// point into.
#[derive(Deserialize)]
struct Document {
    #[serde(flatten)]
    root: Schema,
    definitions: BTreeMap<String, Schema>,
}

/// The parts of a JSON Schema the pages show; the rest is ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Schema {
    #[serde(rename = "$ref")]
    reference: Option<String>,
    description: Option<String>,
    /// The default, absent when it is `null`.
    default: Option<Value>,
    #[serde(rename = "enum")]
    values: Option<Vec<Value>>,
    #[serde(rename = "const")]
    constant: Option<Value>,
    #[serde(rename = "type")]
    types: Option<Types>,
    items: Option<Box<Schema>>,
    properties: Option<BTreeMap<String, Schema>>,
    #[serde(default)]
    required: Vec<String>,
    additional_properties: Option<AdditionalProperties>,
    #[serde(default)]
    one_of: Vec<Schema>,
    #[serde(default)]
    all_of: Vec<Schema>,
    // i128 holds both the signed bounds and the largest unsigned ones,
    // such as `u64::MAX`.
    minimum: Option<i128>,
    maximum: Option<i128>,
}

/// A schema's `type`: one name, or several, such as `["integer", "null"]`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Types {
    One(String),
    Many(Vec<String>),
}

/// A schema's `additionalProperties`.
#[derive(Deserialize)]
#[serde(untagged)]
enum AdditionalProperties {
    /// The schema of each value of a table keyed by name.
    Values(Box<Schema>),
    /// `false`, on tables with fixed keys.
    Fixed(IgnoredAny),
}

impl Schema {
    /// The types it allows besides `null`.
    fn types(&self) -> Vec<&str> {
        match &self.types {
            Some(Types::One(name)) => vec![name.as_str()],
            Some(Types::Many(names)) => names.iter().map(String::as_str).collect(),
            None => Vec::new(),
        }
        .into_iter()
        .filter(|name| *name != "null")
        .collect()
    }

    /// Whether it only allows `null`, as the alternative utoipa adds to
    /// optional values.
    fn is_null(&self) -> bool {
        matches!(&self.types, Some(Types::One(name)) if name == "null")
    }

    /// Whether it is a plain value, such as a string, that a definition
    /// gives a meaning of its own: a value type, described once at the end
    /// of the page.
    fn is_value_type(&self) -> bool {
        self.properties.is_none()
            && self.items.is_none()
            && self.values.is_none()
            && self.constant.is_none()
            && self.one_of.is_empty()
            && self.all_of.is_empty()
            && !self.types().is_empty()
    }

    /// Its description, possibly inside a `oneOf` wrapper.
    fn description(&self) -> Option<&str> {
        self.description.as_deref().or_else(|| {
            self.one_of
                .iter()
                .find_map(|alternative| alternative.description.as_deref())
        })
    }

    /// The only value of a tag such as `type = "http"`.
    fn tag_value(&self) -> Option<&str> {
        match self.values.as_deref()? {
            [value] => value.as_str(),
            _ => None,
        }
    }
}

/// Renders one page, top to bottom.
struct Renderer<'a> {
    /// The schema's definitions, which its `$ref`s point into.
    definitions: &'a BTreeMap<String, Schema>,
    page: Page,
    /// Value types settings link to, by name, described at the end of the
    /// page.
    value_types: BTreeMap<&'a str, &'a Schema>,
}

/// A TOML table at `path`, its section on the page.
struct Section<'a> {
    /// The table's full path, empty for the top level.
    path: String,
    table: Table<'a>,
}

/// A TOML table: what it is, and its settings.
struct Table<'a> {
    /// What the table is, from its type's description.
    description: Option<&'a str>,
    tag: Option<Tag<'a>>,
    settings: Vec<Setting<'a>>,
}

/// The key selecting a variant of a tagged enum, such as `type`.
struct Tag<'a> {
    key: String,
    variants: Vec<Variant<'a>>,
}

/// A variant of a tagged enum: its tag's value, and its description.
struct Variant<'a> {
    value: &'a str,
    description: &'a str,
}

/// One key of a table.
struct Setting<'a> {
    /// The key, or `<key>.<name>` for each value of a table keyed by name,
    /// with the placeholder its description names.
    key: String,
    /// The property's schema, holding its description and default.
    property: &'a Schema,
    /// The schema of the value, which differs from `property` for tables
    /// keyed by name.
    value: &'a Schema,
    required: bool,
    /// When the setting applies, if not always, e.g. ``with `type = "http"` ``.
    condition: Option<String>,
}

impl<'a> Renderer<'a> {
    /// Renders the page: the table `root` and those nested in it, then the
    /// value types their settings link to.
    fn render(mut self, root: &'a Schema) -> Result<String> {
        let table = self.table(root)?.context("the schema is not a table")?;
        self.section(Section {
            path: String::new(),
            table,
        })?;
        if !self.value_types.is_empty() {
            self.page.heading(2, "Value types");
            for (name, definition) in std::mem::take(&mut self.value_types) {
                self.page.heading(3, &Self::title(name));
                let types = Self::join_or(self.alternatives(definition, false)?);
                self.page.paragraphs(&format!("Type: {types}."));
                self.page.paragraphs(
                    definition
                        .description
                        .as_deref()
                        .with_context(|| format!("value type {name} has no description"))?,
                );
            }
        }
        Ok(self.page.finish())
    }

    /// Writes a table's section, then the sections of the tables nested in
    /// it.
    fn section(&mut self, Section { path, table }: Section<'a>) -> Result<()> {
        let heading = if path.is_empty() {
            "Top level".to_owned()
        } else {
            format!("`{}`", Self::header(&path))
        };
        let description = table
            .description
            .with_context(|| format!("table {heading} has no description"))?;
        self.page.heading(2, &heading);
        self.page.paragraphs(description);
        if let Some(tag) = &table.tag {
            self.page
                .heading(3, &format!("`{}`", Self::join(&path, &tag.key)));
            let values = tag
                .variants
                .iter()
                .map(|variant| format!("`\"{}\"`", variant.value));
            self.page
                .paragraphs(&format!("Type: {}. Required.", Self::join_or(values)));
            for variant in &tag.variants {
                self.page.item(
                    0,
                    &format!(
                        "`\"{}\"`: {}",
                        variant.value,
                        Page::prose(variant.description)
                    ),
                );
            }
        }
        let mut settings = table.settings;
        settings.sort_by(|left, right| left.key.cmp(&right.key));
        let mut children = Vec::new();
        for setting in &settings {
            let path = Self::join(&path, &setting.key);
            self.setting(&path, setting)?;
            children.extend(self.child(path, setting.value)?);
        }
        for child in children {
            self.section(child)?;
        }
        Ok(())
    }

    /// Writes the entry of the setting at `path`: its type, whether it is
    /// required or its default, and its description.
    fn setting(&mut self, path: &str, setting: &Setting<'a>) -> Result<()> {
        let description = setting
            .property
            .description()
            .with_context(|| format!("setting {path} has no description"))?;
        let types = self
            .alternatives(setting.value, false)
            .with_context(|| format!("the type of {path}"))?;
        let mut facts = format!("Type: {}", Self::join_or(types));
        let default = setting.property.default.as_ref();
        let shown = default.and_then(Self::toml);
        match (&setting.condition, setting.required) {
            (None, true) => facts.push_str(". Required."),
            (Some(condition), true) => write!(facts, ". Required {condition}.")?,
            (Some(condition), false) => write!(facts, ". Only {condition}.")?,
            (None, false) if shown.is_none() => facts.push_str(". Optional."),
            (None, false) => facts.push('.'),
        }
        if let Some(shown) = shown {
            write!(facts, " Default: `{shown}`")?;
            // Byte sizes, in settings ending in `_bytes`, also read in binary units.
            if let Some(size) = default
                .and_then(Value::as_u64)
                .filter(|_| path.ends_with("_bytes"))
                .and_then(Self::binary_size)
            {
                write!(facts, " ({size})")?;
            }
            facts.push('.');
        }
        self.page.heading(3, &format!("`{path}`"));
        self.page.paragraphs(&facts);
        self.page.paragraphs(description);
        Ok(())
    }

    /// The table `schema` describes, if it describes one: an object, the
    /// merged parts of an `allOf`, or the merged variants of a `oneOf`.
    fn table(&self, schema: &'a Schema) -> Result<Option<Table<'a>>> {
        let schema = self.inner(schema)?;
        if let Some(properties) = &schema.properties {
            let settings = properties
                .iter()
                .map(|(key, property)| self.entry(key, property, schema.required.contains(key)))
                .collect::<Result<_>>()?;
            return Ok(Some(Table {
                description: schema.description(),
                tag: None,
                settings,
            }));
        }
        if !schema.all_of.is_empty() {
            let mut table = Table {
                description: schema.description(),
                tag: None,
                settings: Vec::new(),
            };
            for part in &schema.all_of {
                let part = self.table(part)?.context("an allOf part is not a table")?;
                if let Some(tag) = part.tag {
                    ensure!(table.tag.is_none(), "an allOf has several tagged parts");
                    table.tag = Some(tag);
                }
                table.settings.extend(part.settings);
            }
            return Ok(Some(table));
        }
        if schema.one_of.is_empty() {
            return Ok(None);
        }
        let Some(variants) = schema
            .one_of
            .iter()
            .map(|variant| self.table(variant))
            .collect::<Result<Option<Vec<_>>>>()?
        else {
            return Ok(None);
        };
        Ok(Some(Table {
            description: schema.description(),
            ..Self::merge(variants)?
        }))
    }

    /// Merges the variants of a `oneOf` into one table. Tagged variants share
    /// a key, such as `type`, with one value each; the settings of only some
    /// variants say which. Untagged variants, such as a file's `path` or
    /// `content`, must each be one required setting.
    fn merge(variants: Vec<Table<'a>>) -> Result<Table<'a>> {
        let tag_value = |table: &Table<'a>, key: &str| {
            table
                .settings
                .iter()
                .find(|setting| setting.key == key)
                .and_then(|setting| setting.value.tag_value())
        };
        let tag = variants.first().and_then(|first| {
            first
                .settings
                .iter()
                .map(|setting| setting.key.clone())
                .find(|key| {
                    variants
                        .iter()
                        .all(|variant| tag_value(variant, key).is_some())
                })
        });
        let Some(tag) = tag else {
            let keys = variants
                .iter()
                .map(|variant| match variant.settings.as_slice() {
                    [setting] if setting.required => Ok(setting.key.clone()),
                    _ => Err(anyhow!(
                        "untagged variants must each be one required setting"
                    )),
                })
                .collect::<Result<Vec<_>>>()?;
            let settings = variants
                .into_iter()
                .flat_map(|variant| variant.settings)
                .map(|setting| {
                    let others = keys.iter().filter(|key| **key != setting.key);
                    let others = Self::join_or(others.map(|key| format!("`{key}`")));
                    Setting {
                        condition: Some(format!("unless {others} is set")),
                        ..setting
                    }
                })
                .collect();
            return Ok(Table {
                description: None,
                tag: None,
                settings,
            });
        };

        let mut tag_variants = Vec::new();
        let mut merged = Vec::<(Setting<'a>, Vec<&'a str>)>::new();
        for variant in variants {
            let value = tag_value(&variant, &tag).context("a variant has no tag")?;
            let description = variant
                .description
                .with_context(|| format!("variant {tag} = {value} has no description"))?;
            tag_variants.push(Variant { value, description });
            for setting in variant
                .settings
                .into_iter()
                .filter(|setting| setting.key != tag)
            {
                match merged
                    .iter_mut()
                    .find(|(other, _)| other.key == setting.key)
                {
                    Some((other, in_variants)) => {
                        ensure!(
                            other.property.description() == setting.property.description()
                                && other.required == setting.required,
                            "setting {} differs between its variants",
                            setting.key
                        );
                        in_variants.push(value);
                    }
                    None => merged.push((setting, vec![value])),
                }
            }
        }
        let settings = merged
            .into_iter()
            .map(|(setting, in_variants)| Setting {
                condition: (in_variants.len() < tag_variants.len()).then(|| {
                    let values = in_variants
                        .iter()
                        .map(|value| format!("`{tag} = \"{value}\"`"));
                    format!("with {}", Self::join_or(values))
                }),
                ..setting
            })
            .collect();
        Ok(Table {
            description: None,
            tag: Some(Tag {
                key: tag,
                variants: tag_variants,
            }),
            settings,
        })
    }

    /// The setting `key` of a table, with the property schema `property`.
    fn entry(&self, key: &str, property: &'a Schema, required: bool) -> Result<Setting<'a>> {
        let (key, value) = match &self.inner(property)?.additional_properties {
            Some(AdditionalProperties::Values(values)) => {
                let name = property
                    .description()
                    .and_then(|text| {
                        let start = text.find("`<")? + 1;
                        let end = start + text[start..].find(">`")?;
                        Some(&text[start..=end])
                    })
                    .with_context(|| format!("table {key} names no `<key>`"))?;
                (format!("{key}.{name}"), &**values)
            }
            _ => (key.to_owned(), property),
        };
        Ok(Setting {
            key,
            property,
            value,
            required,
            condition: None,
        })
    }

    /// The table nested at `path` in the value `schema`, directly or as the
    /// entries of an array, whose path then ends in `[]`.
    fn child(&self, path: String, schema: &'a Schema) -> Result<Option<Section<'a>>> {
        let schema = self.inner(schema)?;
        if let Some(items) = &schema.items {
            return Ok(self.table(items)?.map(|table| Section {
                path: format!("{path}[]"),
                table,
            }));
        }
        let mut tables = match self.table(schema)? {
            Some(table) => vec![table],
            // A value that is a table or not, such as a Git source's repository.
            None => schema
                .one_of
                .iter()
                .filter_map(|alternative| self.table(alternative).transpose())
                .collect::<Result<Vec<_>>>()?,
        };
        ensure!(
            tables.len() <= 1,
            "the value of {path} has several table alternatives"
        );
        Ok(tables.pop().map(|table| Section { path, table }))
    }

    /// The types a value of `schema` may have, such as `integer` and a link
    /// to `template`, in the plural for the entries of an array.
    fn alternatives(&mut self, schema: &'a Schema, plural: bool) -> Result<Vec<String>> {
        let word = |word: &str| {
            if plural {
                format!("{word}s")
            } else {
                word.to_owned()
            }
        };
        if let Some(reference) = &schema.reference {
            let (name, definition) = self.resolve(reference)?;
            if !definition.is_value_type() {
                return self.alternatives(definition, plural);
            }
            self.value_types.insert(name, definition);
            // Links to the type's heading, whose anchor is its title in lower case.
            let title = Self::title(name).to_lowercase();
            return Ok(vec![format!(
                "[{}](#{})",
                word(&title),
                title.replace(' ', "-")
            )]);
        }
        if self.table(schema)?.is_some() {
            return Ok(vec![word("table")]);
        }
        if let Some(value) = &schema.constant {
            return Ok(vec![format!(
                "`{}`",
                Self::toml(value).context("constant")?
            )]);
        }
        if let Some(values) = &schema.values {
            return values
                .iter()
                .map(|value| Ok(format!("`{}`", Self::toml(value).context("enum value")?)))
                .collect();
        }
        if !schema.one_of.is_empty() {
            let mut all = Vec::new();
            for alternative in schema
                .one_of
                .iter()
                .filter(|alternative| !alternative.is_null())
            {
                for label in self.alternatives(alternative, plural)? {
                    if !all.contains(&label) {
                        all.push(label);
                    }
                }
            }
            return Ok(all);
        }
        let types = schema.types();
        ensure!(!types.is_empty(), "schema has no type");
        types
            .into_iter()
            .map(|name| match name {
                "string" | "boolean" => Ok(word(name)),
                "integer" => Ok(
                    match (
                        schema.minimum.filter(|minimum| *minimum > 0),
                        schema.maximum,
                    ) {
                        (Some(minimum), Some(maximum)) => {
                            format!("{} from {minimum} to {maximum}", word(name))
                        }
                        (Some(minimum), None) => format!("{} of at least {minimum}", word(name)),
                        (None, Some(maximum)) => format!("{} of at most {maximum}", word(name)),
                        (None, None) => word(name),
                    },
                ),
                "array" => {
                    let items = schema.items.as_deref().context("array has no items")?;
                    Ok(format!(
                        "array of {}",
                        Self::join_or(self.alternatives(items, true)?)
                    ))
                }
                _ => bail!("unexpected schema type {name}"),
            })
            .collect()
    }

    /// The schema `schema` stands for, past `$ref`s and the `oneOf`s around
    /// them that only add a description, a default, or `null`. A `oneOf` of a
    /// single inline variant, such as the only build backend, stays.
    fn inner(&self, mut schema: &'a Schema) -> Result<&'a Schema> {
        loop {
            if let Some(reference) = &schema.reference {
                schema = self.resolve(reference)?.1;
                continue;
            }
            let mut alternatives = schema
                .one_of
                .iter()
                .filter(|alternative| !alternative.is_null());
            match (alternatives.next(), alternatives.next()) {
                (Some(only), None) if only.reference.is_some() => schema = only,
                _ => return Ok(schema),
            }
        }
    }

    /// The name and definition a `$ref` points to.
    fn resolve(&self, reference: &str) -> Result<(&'a str, &'a Schema)> {
        let name = reference
            .strip_prefix("#/definitions/")
            .with_context(|| format!("unexpected reference {reference}"))?;
        let (name, definition) = self
            .definitions
            .get_key_value(name)
            .with_context(|| format!("the schema has no definition {name}"))?;
        Ok((name, definition))
    }

    /// `value` written as TOML, or `None` for empty arrays and tables, which
    /// defaults do not need to show.
    fn toml(value: &Value) -> Option<String> {
        match value {
            Value::Null => None,
            Value::String(_) | Value::Bool(_) | Value::Number(_) => Some(value.to_string()),
            Value::Array(values) if values.is_empty() => None,
            Value::Array(values) => {
                let values = values.iter().map(Self::toml).collect::<Option<Vec<_>>>()?;
                Some(format!("[{}]", values.join(", ")))
            }
            Value::Object(entries) if entries.is_empty() => None,
            Value::Object(entries) => {
                let entries = entries
                    .iter()
                    .filter_map(|(key, value)| Some(format!("{key} = {}", Self::toml(value)?)))
                    .collect::<Vec<_>>();
                Some(format!("{{ {} }}", entries.join(", ")))
            }
        }
    }

    /// `bytes` in the largest binary unit that divides it, such as `4 MiB`.
    fn binary_size(bytes: u64) -> Option<String> {
        [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")]
            .into_iter()
            .find(|(size, _)| bytes >= *size && bytes.is_multiple_of(*size))
            .map(|(size, unit)| format!("{} {unit}", bytes / size))
    }

    /// `path.key`, or `key` at the top level.
    fn join(path: &str, key: &str) -> String {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        }
    }

    /// The TOML header of the table at `path`: `[[spec.services]]` for
    /// `spec.services[]`, `[spec.services.healthcheck]` for
    /// `spec.services[].healthcheck`.
    fn header(path: &str) -> String {
        let table = path.replace("[]", "");
        if path.ends_with("[]") {
            format!("[[{table}]]")
        } else {
            format!("[{table}]")
        }
    }

    /// Joins `items` as `a`, `a or b`, or `a, b or c`.
    fn join_or(items: impl IntoIterator<Item = String>) -> String {
        let mut items = items.into_iter().collect::<Vec<_>>();
        let last = items.pop().unwrap_or_default();
        if items.is_empty() {
            last
        } else {
            format!("{} or {last}", items.join(", "))
        }
    }

    /// A type's name as a title: `CredentialFile` reads `Credential file`.
    fn title(name: &str) -> String {
        let mut title = String::new();
        for (index, character) in name.char_indices() {
            if index > 0 && character.is_uppercase() {
                title.push(' ');
                title.extend(character.to_lowercase());
            } else {
                title.push(character);
            }
        }
        title
    }
}
