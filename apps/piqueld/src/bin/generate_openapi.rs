//! Generates or checks the `OpenAPI` specification, its Rust client bindings,
//! the standalone application manifest JSON Schema, and the manifest settings
//! reference rendered from that schema.

use anyhow::{Context, Result, anyhow, bail, ensure};
use piqueld::api::http::openapi_document;
use piqueld_core::manifest::ApplicationManifest;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

/// Client generation settings read from `tools/client-codegen/config.json`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    replacements: Vec<Replacement>,
}

/// Maps an `OpenAPI` component schema onto an existing Rust type, so the client
/// reuses `piqueld-core` types instead of generating duplicates.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Replacement {
    /// Component schema name in the `OpenAPI` document.
    schema: String,
    /// Type name Progenitor would otherwise generate for the schema.
    name: String,
    /// Fully qualified Rust type used in its place.
    rust_type: String,
}

/// Runs both stages before updating either checked-in artifact.
struct Generator {
    /// Repository root that artifact paths are relative to.
    root: PathBuf,
}

impl Generator {
    /// Renders the `OpenAPI` document and generated client, then writes both, or
    /// with `check` compares them against the checked-in files and fails listing
    /// any that are stale or missing.
    ///
    /// Every path must live under `/api/`.
    fn run(&self, check: bool) -> Result<()> {
        let document = openapi_document();
        let paths = document
            .get("paths")
            .and_then(Value::as_object)
            .context("OpenAPI document has no paths object")?;
        for path in paths.keys() {
            ensure!(
                path.starts_with("/api/"),
                "OpenAPI path is outside the API namespace: {path}"
            );
        }

        let config: Config = serde_json::from_slice(
            &fs::read(self.root.join("tools/client-codegen/config.json"))
                .context("read Progenitor configuration")?,
        )
        .context("parse Progenitor configuration")?;
        let client = self.generate_client(document.clone(), &config)?;
        let document = format!("{}\n", serde_json::to_string_pretty(&document)?);
        let manifest_schema = ApplicationManifest::json_schema();
        let settings = SettingsReference::render(&manifest_schema)
            .context("render the manifest settings reference")?;
        let manifest_schema = format!("{}\n", serde_json::to_string_pretty(&manifest_schema)?);

        let artifacts = [
            ("docs/openapi-v1.json", document),
            ("docs/application-manifest.schema.json", manifest_schema),
            ("docs/user/reference/manifest-settings.md", settings),
            ("crates/piqueld-client/src/generated.rs", client),
        ];
        let mut stale = Vec::new();
        for (relative, contents) in artifacts {
            let path = self.root.join(relative);
            if check {
                match fs::read_to_string(&path) {
                    Ok(existing) if existing == contents => println!("{relative} is up to date"),
                    Ok(_) => stale.push(relative),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        stale.push(relative);
                    }
                    Err(error) => {
                        return Err(error).with_context(|| format!("read {}", path.display()));
                    }
                }
            } else {
                fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
                println!("wrote {relative}");
            }
        }
        ensure!(
            stale.is_empty(),
            "generated artifacts are out of date: {}; run just generate",
            stale.join(", ")
        );
        Ok(())
    }

    /// Generates the Progenitor client source for `crates/piqueld-client`.
    ///
    /// The client wraps `crate::client::ClientState`, applies the configured type
    /// replacements, routes response decoding through `crate::client::decode_response`,
    /// and is formatted with the repository's `rustfmt` configuration.
    fn generate_client(&self, mut document: Value, config: &Config) -> Result<String> {
        Self::prepare_client_document(&mut document, config)?;
        let spec = serde_json::from_value(document).context("parse OpenAPI 3.0 document")?;

        let mut settings = progenitor::GenerationSettings::default();
        let inner = "crate::client::ClientState"
            .parse()
            .map_err(|error| anyhow!("parse generated client state type: {error}"))?;
        settings.with_inner_type(inner);
        for replacement in &config.replacements {
            settings.with_replacement(
                &replacement.name,
                &replacement.rust_type,
                std::iter::empty(),
            );
        }

        let tokens = progenitor::Generator::new(&settings)
            .generate_tokens(&spec)
            .context("generate Progenitor client")?;
        let syntax = syn::parse2(tokens).context("parse generated Rust")?;
        // The generated operations are private implementation details, and
        // Progenitor's indented prose is otherwise interpreted as doctest code.
        let source = prettyplease::unparse(&syntax).replace("/**", "/*");
        let decoder = "ResponseValue::from_response(response).await";
        ensure!(
            source.contains(decoder),
            "Progenitor response decoder shape changed"
        );
        let source = source.replace(decoder, "crate::client::decode_response(response).await");
        self.rustfmt(&format!(
            "// Generated by Progenitor. Run `just generate`; do not edit.\n\n// Progenitor emits mechanical patterns that intentionally trip style lints.\n#![allow(clippy::all, clippy::pedantic, dead_code, private_interfaces)]\n\n{source}"
        ))
    }

    /// Formats Rust source through `rustfmt` over stdin and stdout.
    fn rustfmt(&self, source: &str) -> Result<String> {
        let mut child = Command::new("rustfmt")
            .args(["--edition", "2024", "--emit", "stdout", "--config-path"])
            .arg(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start rustfmt for generated client")?;
        child
            .stdin
            .take()
            .context("open rustfmt stdin")?
            .write_all(source.as_bytes())
            .context("write generated client to rustfmt")?;
        let output = child.wait_with_output().context("wait for rustfmt")?;
        ensure!(
            output.status.success(),
            "rustfmt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).context("rustfmt returned non-UTF-8 output")
    }

    /// Simplifies the document into the subset Progenitor handles well.
    ///
    /// 1. Drops the readiness endpoint's 403 response, which Progenitor cannot
    ///    type alongside its readiness-body 503.
    /// 2. Keeps only the `application/json` request body when several are offered.
    /// 3. Strips parameter constraints Progenitor cannot express (`nullable`, string
    ///    length and pattern, numeric bounds), mapping non-negative `int32`/`int64`
    ///    parameters to `uint32`/`uint64`.
    /// 4. Drops every component schema except the replaced ones, which must exist,
    ///    so shared models always come from `piqueld-core`.
    fn prepare_client_document(document: &mut Value, config: &Config) -> Result<()> {
        let paths = document
            .get_mut("paths")
            .and_then(Value::as_object_mut)
            .context("OpenAPI document has no paths object")?;
        // Generated WebSocket methods return `reqwest::Upgraded`, which WASM
        // builds lack, so the exec WebSocket is written by hand in
        // `piqueld_client::exec` for native targets.
        ensure!(
            paths.remove("/api/v1/environments/{id}/exec").is_some(),
            "missing exec operation"
        );
        // Readiness uses a typed readiness body for 503. Progenitor cannot
        // generate two different error-body types for one operation; preserve
        // its existing readiness decoder and let middleware errors use the
        // unexpected-response path. The published contract retains both.
        if let Some(responses) = paths
            .get_mut("/api/v1/system/readiness")
            .and_then(|path| path.get_mut("get"))
            .and_then(|operation| operation.get_mut("responses"))
            .and_then(Value::as_object_mut)
        {
            responses.remove("403");
        }
        for item in paths.values_mut().filter_map(Value::as_object_mut) {
            for operation in item.values_mut().filter_map(Value::as_object_mut) {
                if let Some(content) = operation
                    .get_mut("requestBody")
                    .and_then(|body| body.get_mut("content"))
                    .and_then(Value::as_object_mut)
                    && let Some(json) = content.remove("application/json")
                {
                    content.clear();
                    content.insert("application/json".into(), json);
                }
                if let Some(parameters) = operation
                    .get_mut("parameters")
                    .and_then(Value::as_array_mut)
                {
                    for parameter in parameters.iter_mut().filter_map(Value::as_object_mut) {
                        let Some(schema) =
                            parameter.get_mut("schema").and_then(Value::as_object_mut)
                        else {
                            continue;
                        };
                        schema.remove("nullable");
                        if schema.get("type").and_then(Value::as_str) == Some("string") {
                            schema.remove("minLength");
                            schema.remove("maxLength");
                            schema.remove("pattern");
                        }
                        if schema.get("type").and_then(Value::as_str) == Some("integer")
                            && schema
                                .get("minimum")
                                .and_then(Value::as_f64)
                                .is_some_and(|minimum| minimum >= 0.0)
                        {
                            let format = match schema.get("format").and_then(Value::as_str) {
                                Some("int32") => Some("uint32"),
                                Some("int64") => Some("uint64"),
                                _ => None,
                            };
                            if let Some(format) = format {
                                schema.insert("format".into(), Value::String(format.into()));
                            }
                        }
                        schema.remove("minimum");
                        schema.remove("maximum");
                    }
                }
            }
        }

        let schemas = document
            .get_mut("components")
            .and_then(|components| components.get_mut("schemas"))
            .and_then(Value::as_object_mut)
            .context("OpenAPI document has no component schemas")?;
        let retained = config
            .replacements
            .iter()
            .map(|replacement| replacement.schema.as_str())
            .collect::<BTreeSet<_>>();
        for schema in &retained {
            ensure!(
                schemas.contains_key(*schema),
                "missing mapped schema {schema}"
            );
        }
        schemas.retain(|name, _| retained.contains(name.as_str()));
        Ok(())
    }
}

/// Renders `docs/user/reference/manifest-settings.md` from the manifest JSON
/// Schema: a section per TOML table, with an entry per setting giving its full
/// path, type, whether it is required or its default, and its description.
///
/// Settings take their description from their doc comment and tables from
/// their type's, so every setting and table must have one. The description of
/// a table keyed by name, such as `environments`, names its key first, as in
/// ``Declares the environment `<environment>` ``. Tagged enums, such as
/// health checks, render as one table whose settings say which `type` they
/// apply to.
struct SettingsReference<'a> {
    /// The schema's `definitions`, which its `$ref`s point into.
    definitions: &'a Map<String, Value>,
    /// The page rendered so far.
    page: String,
}

/// A TOML table: what it is, and its settings.
struct Table<'a> {
    /// The type's description; parts of an `allOf` have none.
    description: Option<&'a str>,
    /// The key selecting a variant of a tagged enum, such as `type`, with each
    /// value and its description.
    tag: Option<(String, Vec<(&'a str, &'a str)>)>,
    settings: Vec<Setting<'a>>,
}

/// One key of a table.
struct Setting<'a> {
    /// The key, or `<key>.<name>` for each value of a table keyed by name,
    /// with the placeholder its description names.
    key: String,
    /// The property's schema, holding its description and default.
    property: &'a Value,
    /// The schema of the value, which differs from `property` for tables
    /// keyed by name.
    value: &'a Value,
    required: bool,
    /// When the setting applies, if not always, e.g. ``with `type = "http"` ``.
    condition: Option<String>,
}

impl<'a> SettingsReference<'a> {
    /// Renders the page for the manifest schema `schema`.
    fn render(schema: &'a Value) -> Result<String> {
        let definitions = schema
            .get("definitions")
            .and_then(Value::as_object)
            .context("manifest schema has no definitions")?;
        let mut reference = Self {
            definitions,
            page: String::new(),
        };
        reference.page.push_str(SETTINGS_INTRODUCTION);
        let root = reference
            .table(schema)?
            .context("manifest schema is not a table")?;
        reference.section("", root)?;
        Ok(reference.page.trim_end().to_owned() + "\n")
    }

    /// Writes the table at `path`, then the tables nested in it.
    fn section(&mut self, path: &str, table: Table<'a>) -> Result<()> {
        let heading = if path.is_empty() {
            "Top level".to_owned()
        } else {
            format!("`{path}`")
        };
        let about = table
            .description
            .with_context(|| format!("manifest table {heading} has no description"))?;
        writeln!(self.page, "## {heading}\n\n{about}\n")?;
        let join = |key: &str| {
            if path.is_empty() {
                key.to_owned()
            } else {
                format!("{path}.{key}")
            }
        };
        if let Some((key, variants)) = &table.tag {
            let values = variants.iter().map(|(value, _)| format!("`\"{value}\"`"));
            writeln!(
                self.page,
                "### `{}`\n\nType: {}. Required.\n",
                join(key),
                join_or(values)
            )?;
            for (value, about) in variants {
                writeln!(
                    self.page,
                    "- `\"{value}\"`: {}",
                    about.replace('\n', "\n  ")
                )?;
            }
            self.page.push('\n');
        }
        let mut settings = table.settings;
        settings.sort_by(|left, right| left.key.cmp(&right.key));
        let mut children = Vec::new();
        for setting in &settings {
            let path = join(&setting.key);
            let description = description(setting.property)
                .with_context(|| format!("manifest setting {path} has no description"))?;
            let mut facts = join_or(self.alternatives(setting.value, false)?);
            let default = setting.property.get("default").and_then(toml);
            match (&setting.condition, setting.required) {
                (None, true) => facts.push_str(". Required."),
                (Some(condition), true) => write!(facts, ". Required {condition}.")?,
                (Some(condition), false) => write!(facts, ". Only {condition}.")?,
                (None, false) if default.is_none() => facts.push_str(". Optional."),
                (None, false) => facts.push('.'),
            }
            if let Some(default) = default {
                write!(facts, " Default: `{default}`.")?;
            }
            writeln!(
                self.page,
                "### `{path}`\n\nType: {facts}\n\n{description}\n"
            )?;
            if let Some((suffix, child)) = self.child(setting.value)? {
                children.push((format!("{path}{suffix}"), child));
            }
        }
        for (path, child) in children {
            self.section(&path, child)?;
        }
        Ok(())
    }

    /// The table `schema` describes, if it describes one: an object, the
    /// merged parts of an `allOf`, or the merged variants of a `oneOf`.
    fn table(&self, schema: &'a Value) -> Result<Option<Table<'a>>> {
        let schema = self.inner(schema)?;
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            let required = schema
                .get("required")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let settings = properties
                .iter()
                .map(|(key, property)| {
                    let required = required.iter().any(|name| name.as_str() == Some(key));
                    self.setting(key, property, required)
                })
                .collect::<Result<_>>()?;
            return Ok(Some(Table {
                description: description(schema),
                tag: None,
                settings,
            }));
        }
        if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
            let mut settings = Vec::new();
            for part in parts {
                let part = self
                    .table(part)?
                    .context("manifest allOf part is not a table")?;
                settings.extend(part.settings);
            }
            return Ok(Some(Table {
                description: description(schema),
                tag: None,
                settings,
            }));
        }
        let Some(variants) = schema.get("oneOf").and_then(Value::as_array) else {
            return Ok(None);
        };
        let Some(variants) = variants
            .iter()
            .map(|variant| self.table(variant))
            .collect::<Result<Option<Vec<_>>>>()?
        else {
            return Ok(None);
        };
        Ok(Some(Table {
            description: description(schema),
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
                .and_then(
                    |setting| match setting.value.get("enum")?.as_array()?.as_slice() {
                        [value] => value.as_str(),
                        _ => None,
                    },
                )
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
                        "untagged manifest variants must each be one required setting"
                    )),
                })
                .collect::<Result<Vec<_>>>()?;
            let settings = variants
                .into_iter()
                .flat_map(|variant| variant.settings)
                .map(|setting| {
                    let others = keys.iter().filter(|key| **key != setting.key);
                    let others = join_or(others.map(|key| format!("`{key}`")));
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

        let mut values = Vec::new();
        let mut merged = Vec::<(Setting<'a>, Vec<&'a str>)>::new();
        for variant in variants {
            let value = tag_value(&variant, &tag).context("manifest variant has no tag")?;
            let about = variant
                .description
                .with_context(|| format!("manifest variant {tag} = {value} has no description"))?;
            values.push((value, about));
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
                            description(other.property) == description(setting.property)
                                && other.required == setting.required,
                            "manifest setting {} differs between its variants",
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
                condition: (in_variants.len() < values.len()).then(|| {
                    let values = in_variants
                        .iter()
                        .map(|value| format!("`{tag} = \"{value}\"`"));
                    format!("with {}", join_or(values))
                }),
                ..setting
            })
            .collect();
        Ok(Table {
            description: None,
            tag: Some((tag, values)),
            settings,
        })
    }

    /// The setting `key` of a table, with the property schema `property`.
    fn setting(&self, key: &'a str, property: &'a Value, required: bool) -> Result<Setting<'a>> {
        // `additionalProperties` is `false` on tables with fixed keys.
        let (key, value) = match self.inner(property)?.get("additionalProperties") {
            Some(values) if values.is_object() => {
                let name = description(property)
                    .and_then(|text| {
                        let start = text.find("`<")? + 1;
                        let end = start + text[start..].find(">`")?;
                        Some(&text[start..=end])
                    })
                    .with_context(|| format!("manifest table {key} names no `<key>`"))?;
                (format!("{key}.{name}"), values)
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

    /// The table nested in the value `schema`, directly or as the entries of
    /// an array, with the suffix its path takes: `""` or `"[]"`.
    fn child(&self, schema: &'a Value) -> Result<Option<(&'static str, Table<'a>)>> {
        let schema = self.inner(schema)?;
        if let Some(items) = schema.get("items") {
            return Ok(self.table(items)?.map(|table| ("[]", table)));
        }
        if let Some(table) = self.table(schema)? {
            return Ok(Some(("", table)));
        }
        // A value that is a table or not, such as a Git source's repository.
        let mut tables = Vec::new();
        for alternative in schema
            .get("oneOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            tables.extend(self.table(alternative)?);
        }
        ensure!(
            tables.len() <= 1,
            "manifest value has several table alternatives"
        );
        Ok(tables.pop().map(|table| ("", table)))
    }

    /// The types a value of `schema` may have, such as `integer` and
    /// `template`, in the plural for the entries of an array.
    fn alternatives(&self, schema: &'a Value, plural: bool) -> Result<Vec<String>> {
        let word = |word: &str| {
            if plural {
                format!("{word}s")
            } else {
                word.to_owned()
            }
        };
        if let Some(name) = reference(schema) {
            if name == "Template" || name == "TypedTemplate" {
                return Ok(vec![word("template")]);
            }
            return self.alternatives(self.definition(name)?, plural);
        }
        if self.table(schema)?.is_some() {
            return Ok(vec![word("table")]);
        }
        if let Some(value) = schema.get("const") {
            return Ok(vec![format!(
                "`{}`",
                toml(value).context("manifest constant")?
            )]);
        }
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return values
                .iter()
                .map(|value| Ok(format!("`{}`", toml(value).context("manifest enum value")?)))
                .collect();
        }
        if let Some(alternatives) = schema.get("oneOf").and_then(Value::as_array) {
            let mut all = Vec::new();
            for alternative in alternatives {
                for label in self.alternatives(alternative, plural)? {
                    if !all.contains(&label) {
                        all.push(label);
                    }
                }
            }
            return Ok(all);
        }
        let types = match schema.get("type") {
            Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(name)) => vec![name.as_str()],
            _ => bail!("manifest schema has no type: {schema}"),
        };
        types
            .into_iter()
            .filter(|name| *name != "null")
            .map(|name| match name {
                "string" | "integer" | "boolean" => Ok(word(name)),
                "array" => {
                    let items = schema.get("items").context("manifest array has no items")?;
                    Ok(format!(
                        "array of {}",
                        join_or(self.alternatives(items, true)?)
                    ))
                }
                _ => bail!("unexpected manifest schema type {name}"),
            })
            .collect()
    }

    /// The schema `schema` stands for, past `$ref`s and the `oneOf`s around
    /// them that only add a description, a default, or `null`. A `oneOf` of a
    /// single inline variant, such as the only build backend, stays.
    fn inner(&self, mut schema: &'a Value) -> Result<&'a Value> {
        loop {
            if let Some(name) = reference(schema) {
                schema = self.definition(name)?;
                continue;
            }
            let alternatives = schema
                .get("oneOf")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|alternative| {
                    alternative.get("type").and_then(Value::as_str) != Some("null")
                })
                .collect::<Vec<_>>();
            match alternatives.as_slice() {
                [only] if reference(only).is_some() => schema = only,
                _ => return Ok(schema),
            }
        }
    }

    /// The definition `name` refers to.
    fn definition(&self, name: &str) -> Result<&'a Value> {
        self.definitions
            .get(name)
            .with_context(|| format!("manifest schema has no definition {name}"))
    }
}

/// The page's introduction, before the generated sections.
const SETTINGS_INTRODUCTION: &str = r#"<!-- Generated by `just generate` from the doc comments of the manifest types in
crates/piqueld-core/src/manifest. Do not edit it by hand: edit the doc comments
and run `just generate` again. -->

# Manifest settings

This page lists every setting of an [application manifest](manifest.md), with
its type, whether it is required or its default, and what it does. It is
generated from the manifest's
[JSON Schema](manifest.md#validation-and-editor-support), so your editor shows
the same descriptions. For how the settings work together, read the
[application manifest](manifest.md) reference and the guides, such as
[HTTP ingress](../guides/ingress.md).

Each setting is named by its full TOML path:

- `[]` stands for each entry of an array of tables: `spec.services[].name` is
  the `name` of each `[[spec.services]]`.
- A name in angle brackets, such as `<environment>`, stands for a key you
  choose: `spec.environments.<environment>` is each environment table, such as
  `[spec.environments.staging]`.

A *template* is a string that may reference [variables](manifest.md#variables),
such as `"${{ vars.domain }}"`. Only settings whose type includes template
accept references.

Names, such as those of applications, environments, services, volumes, jobs,
secrets and routes, are 1 to 63 lowercase letters, digits or hyphens. They
start with a letter and end with a letter or digit.

"#;

/// The definition name a `$ref` schema points to.
fn reference(schema: &Value) -> Option<&str> {
    schema.get("$ref")?.as_str()?.strip_prefix("#/definitions/")
}

/// The description of a property, possibly inside a `oneOf` wrapper.
fn description(schema: &Value) -> Option<&str> {
    schema
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| {
            schema
                .get("oneOf")?
                .as_array()?
                .iter()
                .find_map(|alternative| alternative.get("description")?.as_str())
        })
}

/// `value` written as TOML, or `None` for `null` and empty arrays and tables,
/// which defaults do not need to show.
fn toml(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(Value::String(text.clone()).to_string()),
        Value::Bool(_) | Value::Number(_) => Some(value.to_string()),
        Value::Array(values) if values.is_empty() => None,
        Value::Array(values) => {
            let values = values.iter().map(toml).collect::<Option<Vec<_>>>()?;
            Some(format!("[{}]", values.join(", ")))
        }
        Value::Object(entries) if entries.is_empty() => None,
        Value::Object(entries) => {
            let entries = entries
                .iter()
                .filter_map(|(key, value)| Some(format!("{key} = {}", toml(value)?)))
                .collect::<Vec<_>>();
            Some(format!("{{ {} }}", entries.join(", ")))
        }
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

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    ensure!(
        args.is_empty() || args == ["--check"],
        "usage: generate_openapi [--check]"
    );
    Generator {
        root: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .context("locate repository root")?,
    }
    .run(!args.is_empty())
}
