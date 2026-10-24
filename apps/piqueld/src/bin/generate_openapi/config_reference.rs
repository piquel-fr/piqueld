//! `docs/user/reference/configuration-settings.md`, rendered from the
//! daemon configuration's JSON Schema (see `DaemonConfig::json_schema`).

use anyhow::{Context, Result};
use piqueld_core::generated::Page;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// The page's introduction, after its title.
const INTRO: &str = "\
This page lists every setting of the daemon's configuration file,
`/etc/piqueld/config.toml`, with its type and default. It is generated from
the daemon's configuration types, like the file's
[JSON Schema](../../piqueld-config.schema.json), which editors that use
[Taplo](https://taplo.tamasfe.dev/) can check the file against. For how the
daemon uses the file, with examples, see
[daemon configuration](configuration.md); NixOS sets these settings through
the [NixOS module](nixos.md).

Every setting is optional unless it says otherwise. The daemon reads the
file once at startup, so restart it after a change. Settings ending in
`_file` take the path of a [credential file](#credential-file): a relative
path resolves against `$CREDENTIALS_DIRECTORY`.";

/// Renders the reference: one section per table, one heading per setting.
pub struct ConfigReference<'a> {
    page: Page,
    /// The schema's `definitions`, which `$ref`s point into.
    definitions: &'a Map<String, Value>,
    /// Value types settings link to, described at the end of the page.
    types: BTreeSet<&'a str>,
}

/// A setting of a table.
#[derive(Clone, Copy)]
struct Property<'a> {
    /// Its name in the table.
    name: &'a str,
    /// Its schema.
    schema: &'a Value,
    /// Whether the table requires it.
    required: bool,
}

/// The settings of a table: its own properties, and for a table selected by
/// a tag such as `kind`, each variant's.
#[derive(Default)]
struct Fields<'a> {
    /// The table's own settings.
    properties: Vec<Property<'a>>,
    /// One per variant of a tagged table, with its description.
    variants: Vec<(Option<&'a str>, Fields<'a>)>,
}

impl<'a> ConfigReference<'a> {
    /// Where the page is written, relative to the repository root.
    pub const PATH: &str = "docs/user/reference/configuration-settings.md";

    /// Renders the page from the configuration's JSON Schema.
    ///
    /// # Errors
    /// If the schema refers to a definition it lacks.
    pub fn render(schema: &'a Value) -> Result<String> {
        let mut reference = Self {
            page: Page::new(
                "the configuration types in `apps/piqueld/src/config`",
                "Daemon configuration settings",
                INTRO,
            ),
            definitions: schema["definitions"]
                .as_object()
                .context("configuration schema has no definitions")?,
            types: BTreeSet::new(),
        };
        reference.table("", schema)?;
        if !reference.types.is_empty() {
            reference.page.heading(2, "Value types");
            for name in std::mem::take(&mut reference.types) {
                reference.page.heading(3, &Self::title(name));
                let definition = reference.definition(name)?;
                reference
                    .page
                    .paragraphs(Self::description(definition).unwrap_or_default());
            }
        }
        Ok(reference.page.finish())
    }

    /// Adds the table at `path` (empty for the file itself, whose
    /// description the introduction replaces): its description, its
    /// settings, then its subtables. A table inside an array, such as
    /// `[[dns.providers]]`, has a `path` ending in `[]`.
    fn table(&mut self, path: &str, schema: &'a Value) -> Result<()> {
        if !path.is_empty() {
            let heading = match path.strip_suffix("[]") {
                Some(array) => format!("`[[{array}]]`"),
                None => format!("`[{path}]`"),
            };
            self.page.heading(2, &heading);
            if let Some(description) = Self::description(self.resolve(schema)?) {
                self.page.paragraphs(description);
            }
        }
        let fields = self.fields(schema)?;
        let variants = self.tag(path, &fields.variants)?;
        let mut tables = Vec::new();
        for property in Self::sorted(&fields.properties) {
            let property_path = Self::join(path, property.name);
            match self.table_kind(property.schema)? {
                Some((table, true)) => tables.push((format!("{property_path}[]"), table)),
                Some((table, false)) => tables.push((property_path, table)),
                None => self.setting(&property_path, property, None)?,
            }
        }
        for (note, properties) in variants {
            for property in properties {
                self.setting(&Self::join(path, property.name), property, Some(&note))?;
            }
        }
        for (path, table) in tables {
            self.table(&path, table)?;
        }
        Ok(())
    }

    /// Adds the tag of a table with variants, such as `kind`, whose values
    /// describe the variants. Returns each variant's other settings, with a
    /// note saying which value of the tag they need.
    fn tag(
        &mut self,
        path: &str,
        variants: &[(Option<&'a str>, Fields<'a>)],
    ) -> Result<Vec<(String, Vec<Property<'a>>)>> {
        let mut tag = None;
        let mut values = Vec::new();
        for (description, fields) in variants {
            let (name, value) = fields
                .properties
                .iter()
                .find_map(
                    |property| match property.schema["enum"].as_array().map(Vec::as_slice) {
                        Some([value]) => Some((property.name, value.as_str()?)),
                        _ => None,
                    },
                )
                .with_context(|| format!("a variant of {path} has no tag"))?;
            tag = Some(name);
            values.push((value, *description, fields));
        }
        let Some(tag) = tag else {
            return Ok(Vec::new());
        };
        self.page
            .heading(3, &format!("`{}`", Self::join(path, tag)));
        self.page.paragraphs("Type: string. Required. One of:");
        for (value, description, _) in &values {
            self.page.item(
                0,
                &format!(
                    "`\"{value}\"`: {}",
                    Page::prose(description.unwrap_or_default())
                ),
            );
        }
        Ok(values
            .into_iter()
            .map(|(value, _, fields)| {
                let properties = Self::sorted(&fields.properties)
                    .into_iter()
                    .filter(|property| property.name != tag)
                    .collect();
                (format!("Only with `{tag} = \"{value}\"`."), properties)
            })
            .collect())
    }

    /// Adds one setting: its type, default and description. A setting
    /// without a description of its own, taking one of several values, gets
    /// the values' description.
    fn setting(&mut self, path: &str, property: Property<'a>, note: Option<&str>) -> Result<()> {
        let Property {
            schema, required, ..
        } = property;
        self.page.heading(3, &format!("`{path}`"));
        let mut summary = format!("Type: {}.", self.type_text(schema)?);
        let default = schema
            .get("default")
            .or_else(|| Self::unwrap(schema).get("default"));
        match default {
            _ if required => summary.push_str(" Required."),
            Some(default) => {
                let _ = write!(summary, " Default: {}.", Self::value_text(path, default));
            }
            None => summary.push_str(" Default: unset."),
        }
        if let Some(note) = note {
            summary.push(' ');
            summary.push_str(note);
        }
        self.page.paragraphs(&summary);
        let definition = self.resolve(Self::unwrap(schema))?;
        let description = Self::description(schema)
            .or_else(|| Self::description(Self::unwrap(schema)))
            .or_else(|| {
                definition
                    .get("enum")
                    .and_then(|_| Self::description(definition))
            });
        if let Some(description) = description {
            self.page.paragraphs(description);
        }
        Ok(())
    }

    /// `properties` sorted by name, as the schema lists them.
    fn sorted(properties: &[Property<'a>]) -> Vec<Property<'a>> {
        let mut properties = properties.to_vec();
        properties.sort_by_key(|property| property.name);
        properties
    }

    /// The settings of a table, gathered through `allOf` (flattened
    /// structures) and `oneOf` (tagged variants).
    fn fields(&self, schema: &'a Value) -> Result<Fields<'a>> {
        let schema = self.resolve(schema)?;
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut fields = Fields::default();
        if let Some(properties) = schema["properties"].as_object() {
            fields
                .properties
                .extend(properties.iter().map(|(name, schema)| Property {
                    name,
                    schema,
                    required: required.contains(&name.as_str()),
                }));
        }
        for member in schema["allOf"].as_array().into_iter().flatten() {
            let member = self.fields(member)?;
            fields.properties.extend(member.properties);
            fields.variants.extend(member.variants);
        }
        for variant in schema["oneOf"].as_array().into_iter().flatten() {
            let description = variant["allOf"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|member| Self::description(self.resolve(member).ok()?));
            fields.variants.push((description, self.fields(variant)?));
        }
        Ok(fields)
    }

    /// The table `schema` holds, if it is one: an object with settings
    /// rather than a value, and whether it is in an array (`[[...]]` in TOML).
    fn table_kind(&self, schema: &'a Value) -> Result<Option<(&'a Value, bool)>> {
        let schema = Self::unwrap(schema);
        let array = schema["type"] == "array";
        let schema = if array { &schema["items"] } else { schema };
        if schema.get("$ref").is_none() {
            return Ok(None);
        }
        let resolved = self.resolve(schema)?;
        let table = resolved.get("properties").is_some() || resolved.get("allOf").is_some();
        Ok(table.then_some((schema, array)))
    }

    /// How a setting's type reads, such as `integer from 1 to 86400`, with
    /// a link for value types described at the end of the page.
    fn type_text(&mut self, schema: &'a Value) -> Result<String> {
        let schema = Self::unwrap(schema);
        if let Some(name) = schema["$ref"]
            .as_str()
            .and_then(|reference| reference.strip_prefix("#/definitions/"))
        {
            let definition = self.definition(name)?;
            if definition.get("enum").is_some() {
                return self.type_text(definition);
            }
            self.types.insert(name);
            // Links to the type's heading, whose anchor is its title in lower case.
            let words = Self::title(name).to_lowercase();
            return Ok(format!(
                "{}, a [{words}](#{})",
                self.type_text(definition)?,
                words.replace(' ', "-")
            ));
        }
        if let Some(values) = schema["enum"].as_array() {
            let values: Vec<String> = values.iter().map(|value| format!("`{value}`")).collect();
            return Ok(format!("string, one of {}", values.join(", ")));
        }
        let kind = match &schema["type"] {
            Value::Array(kinds) => kinds
                .iter()
                .filter_map(Value::as_str)
                .find(|kind| *kind != "null"),
            kind => kind.as_str(),
        }
        .with_context(|| format!("schema without a type: {schema}"))?;
        Ok(match kind {
            "array" => format!("array of {}s", self.type_text(&schema["items"])?),
            "integer" => {
                let minimum = schema["minimum"].as_u64().filter(|minimum| *minimum > 0);
                match (minimum, schema["maximum"].as_u64()) {
                    (Some(minimum), Some(maximum)) => {
                        format!("integer from {minimum} to {maximum}")
                    }
                    (Some(minimum), None) => format!("integer, at least {minimum}"),
                    (None, Some(maximum)) => format!("integer up to {maximum}"),
                    (None, None) => "integer".to_owned(),
                }
            }
            kind => kind.to_owned(),
        })
    }

    /// A default value as written in TOML, `unset` for `null`. Byte sizes,
    /// in settings ending in `_bytes`, also read in binary units.
    fn value_text(path: &str, value: &Value) -> String {
        if value.is_null() {
            return "unset".to_owned();
        }
        let mut text = format!("`{value}`");
        if let Some(bytes) = value.as_u64().filter(|_| path.ends_with("_bytes")) {
            let units = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
            if let Some((size, unit)) = units
                .into_iter()
                .find(|(size, _)| bytes >= *size && bytes % size == 0)
            {
                let _ = write!(text, " ({} {unit})", bytes / size);
            }
        }
        text
    }

    /// The schema an optional value or a value with a default wraps:
    /// utoipa writes them as a `oneOf` of the value and `null`.
    fn unwrap(schema: &'a Value) -> &'a Value {
        match schema["oneOf"].as_array().map(Vec::as_slice) {
            Some([value]) => value,
            Some([null, value] | [value, null]) if null["type"] == "null" => value,
            _ => schema,
        }
    }

    /// The definition a `$ref` schema points to, or the schema itself.
    fn resolve(&self, schema: &'a Value) -> Result<&'a Value> {
        match schema["$ref"].as_str() {
            Some(reference) => self.definition(
                reference
                    .strip_prefix("#/definitions/")
                    .with_context(|| format!("unexpected reference {reference}"))?,
            ),
            None => Ok(schema),
        }
    }

    /// The definition called `name`.
    fn definition(&self, name: &str) -> Result<&'a Value> {
        self.definitions
            .get(name)
            .with_context(|| format!("configuration schema lacks the definition {name}"))
    }

    /// A schema's description, if it has one.
    fn description(schema: &Value) -> Option<&str> {
        schema["description"].as_str()
    }

    /// `path.name`, or `name` at the top level.
    fn join(path: &str, name: &str) -> String {
        if path.is_empty() {
            name.to_owned()
        } else {
            format!("{path}.{name}")
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
