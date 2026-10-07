use super::*;

#[test]
fn references_allow_whitespace_and_escapes_while_shell_syntax_stays_text() {
    let segments = |text: &str| Template::from(text).segments().unwrap();
    let domain = Segment::Reference(Reference::Variable("domain".into()));
    assert_eq!(segments("${{vars.domain}}"), std::slice::from_ref(&domain));
    assert_eq!(
        segments("${{  vars.domain \t}}"),
        std::slice::from_ref(&domain)
    );
    assert_eq!(
        segments("api.${{ vars.domain }}!"),
        [
            Segment::Text("api.".into()),
            domain,
            Segment::Text("!".into())
        ]
    );
    assert_eq!(
        segments("${HOME} $USER $${{ vars.domain }}"),
        [Segment::Text("${HOME} $USER ${{ vars.domain }}".into())]
    );
    assert_eq!(
        segments("${{ env.slug }}"),
        [Segment::Reference(Reference::System(
            SystemVariable::EnvSlug
        ))]
    );
    for (text, code) in [
        ("${{ vars.domain", codes::TEMPLATE_INVALID),
        ("${{ domain }}", codes::TEMPLATE_INVALID),
        ("${{ vars. }}", codes::TEMPLATE_INVALID),
        ("${{ secrets.token }}", codes::VARIABLE_RESERVED),
        ("${{ var.domain }}", codes::VARIABLE_NAMESPACE_UNKNOWN),
        ("${{ env.region }}", codes::VARIABLE_UNKNOWN),
    ] {
        assert_eq!(
            Template::from(text).segments().unwrap_err().code(),
            code,
            "{text}"
        );
    }
    let literal = Template::literal("${{ vars.domain }}");
    assert_eq!(literal.as_str(), "$${{ vars.domain }}");
    assert_eq!(literal.as_literal().as_deref(), Some("${{ vars.domain }}"));
}

#[test]
fn variable_text_round_trips_blank_and_numeric_looking_strings() {
    for value in [
        Variable::String("".into()),
        Variable::String("  ".into()),
        Variable::String("3".into()),
        Variable::String("true".into()),
        Variable::String("\"quoted\"".into()),
        Variable::String("piquel.fr".into()),
        Variable::Integer(3),
        Variable::Boolean(false),
    ] {
        let text = value.to_text();
        assert!(!text.trim().is_empty(), "{value:?} would read as no value");
        assert_eq!(Variable::from_text(&text), value, "{text}");
    }
}

#[test]
fn parser_preserves_dollar_edges_and_adjacent_references() {
    let reference = Segment::Reference(Reference::Variable("a".into()));
    for (source, expected) in [
        ("", vec![]),
        ("$", vec![Segment::Text("$".into())]),
        ("a$$b}}", vec![Segment::Text("a$$b}}".into())]),
        (
            "${{vars.a}}${{vars.a}}",
            vec![reference.clone(), reference.clone()],
        ),
        (
            "$${{vars.a}}${{vars.a}}",
            vec![Segment::Text("${{vars.a}}".into()), reference],
        ),
    ] {
        assert_eq!(
            Template::from(source).segments().unwrap(),
            expected,
            "{source}"
        );
    }
    assert_eq!(
        Template::from("prefix ${{vars.a").segments(),
        Err(TemplateError::Unterminated)
    );
    for source in [
        "",
        "$",
        "$$",
        "${HOME} $USER",
        "${{",
        "$${{",
        "$$${{vars.a}}",
        "é${{vars.a}}${{env.name}}}}",
    ] {
        assert_eq!(
            Template::literal(source).as_literal().as_deref(),
            Some(source)
        );
    }
    for source in ["${{vars.a}}", "${{bad}}", "${{"] {
        assert_eq!(Template::from(source).as_literal(), None);
    }
}

#[test]
fn identifiers_and_reference_errors_have_bounded_precise_diagnostics() {
    use super::super::variables::valid_identifier;
    for (name, valid) in [
        ("_", true),
        ("A_b-9", true),
        ("a-", true),
        ("", false),
        ("1a", false),
        ("-a", false),
        ("é", false),
        ("a.b", false),
        ("a b", false),
    ] {
        assert_eq!(valid_identifier(name), valid, "{name}");
        let parsed = Template::from(format!("${{{{vars.{name}}}}}")).segments();
        assert_eq!(parsed.is_ok(), valid, "{name}");
    }
    assert!(valid_identifier(&"a".repeat(63)));
    assert!(!valid_identifier(&"a".repeat(64)));
    for (source, error, message) in [
        (
            "${{",
            TemplateError::Unterminated,
            "`${{` must be closed by `}}`; write `$${{` for a literal `${{`",
        ),
        (
            "${{a}}",
            TemplateError::Malformed("a".into()),
            "`a` is not a `namespace.name` reference",
        ),
        (
            "${{secrets.x}}",
            TemplateError::Reserved("secrets.x".into()),
            "`secrets.x` is reserved for secret references, which are not supported yet",
        ),
        (
            "${{wat.x}}",
            TemplateError::UnknownNamespace("wat".into()),
            "unknown namespace `wat`; references use vars, app, env, git, or deployment",
        ),
        (
            "${{app.x}}",
            TemplateError::UnknownSystemVariable("app.x".into()),
            "unknown system variable `app.x`",
        ),
    ] {
        assert_eq!(Template::from(source).segments().unwrap_err(), error);
        assert_eq!(error.to_string(), message);
    }
    let long = "é".repeat(100);
    assert_eq!(
        Template::from(format!("${{{{{long}}}}}")).segments(),
        Err(TemplateError::Malformed("é".repeat(64)))
    );
    for inner in ["vars.a.b", ".a", "1vars.a", "vars.a b", "", "vars.a + 1"] {
        assert_eq!(
            Template::from(format!("${{{{{inner}}}}}"))
                .segments()
                .unwrap_err()
                .code(),
            codes::TEMPLATE_INVALID
        );
    }
    for variable in SystemVariable::ALL {
        let reference = Reference::System(variable);
        assert_eq!(reference.to_string(), variable.as_str());
        assert_eq!(
            Template::from(format!("${{{{ {} }}}}", variable.as_str()))
                .segments()
                .unwrap(),
            [Segment::Reference(reference)]
        );
    }
    assert_eq!(
        Reference::Variable("a_b-9".into()).to_string(),
        "vars.a_b-9"
    );
}

#[test]
fn template_wire_and_comparisons_preserve_the_raw_spelling() {
    let raw = "$${{vars.a}}";
    let template = Template::from(raw.to_owned());
    assert_eq!(template.to_string(), raw);
    assert_eq!(template, raw);
    assert!(<Template as PartialEq<str>>::eq(&template, raw));
    assert_ne!(template, "${{vars.a}}");
    assert_eq!(serde_json::to_value(&template).unwrap(), json!(raw));
    assert_eq!(
        serde_json::from_value::<Template>(json!(raw)).unwrap(),
        template
    );
    assert!(serde_json::from_value::<Template>(json!(1)).is_err());
    assert_eq!(Template::default().as_literal().as_deref(), Some(""));
    for (text, mentions) in [
        (raw, true),
        ("${{", true),
        ("$VAR ${VAR}", false),
        ("", false),
    ] {
        assert_eq!(Template::mentions_reference(text), mentions);
    }
}

#[test]
fn typed_parsing_and_slots_preserve_types_and_failed_assignments() {
    use super::super::variables::{Slot, TypedSlot};
    let mut number = Typed::from(7_u16);
    assert_eq!(number.literal(), Some(&7));
    assert_eq!(number.to_string(), "7");
    assert_eq!("7".parse::<Typed<u16>>().unwrap(), number);
    assert!("bad".parse::<Typed<u16>>().is_err());
    assert!(Slot::Typed(&mut number).template().is_none());
    for raw in ["${{ vars.n }}", "$${{vars.n}}", "${{"] {
        let mut typed: Typed<u16> = raw.parse().unwrap();
        assert_eq!(typed.literal(), None);
        assert_eq!(typed.to_string(), raw);
        assert_eq!(
            serde_json::from_value::<Typed<u16>>(json!(raw)).unwrap(),
            typed
        );
        assert_eq!(serde_json::to_value(&typed).unwrap(), json!(raw));
        assert_eq!(Slot::Typed(&mut typed).template().unwrap().as_str(), raw);
        assert!(typed.set(json!("7")).is_err());
        assert_eq!(typed.to_string(), raw);
        typed.set(json!(9)).unwrap();
        assert_eq!(typed.literal(), Some(&9));
    }
    assert_eq!(
        serde_json::from_value::<Typed<u16>>(json!(7)).unwrap(),
        number
    );
    for value in [
        json!("7"),
        json!(true),
        json!(-1),
        json!(70000),
        Value::Null,
        json!([]),
        json!({}),
    ] {
        assert!(serde_json::from_value::<Typed<u16>>(value).is_err());
    }
    let mut text = Template::from("hello");
    assert_eq!(Slot::Text(&mut text).template().unwrap().as_str(), "hello");
}

#[test]
fn variable_values_round_trip_native_types_and_interpolate_as_text() {
    for (value, wire, display) in [
        (VariableValue::Boolean(false), json!(false), "false"),
        (VariableValue::Integer(-42), json!(-42), "-42"),
        (VariableValue::String("42".into()), json!("42"), "42"),
    ] {
        assert_eq!(serde_json::to_value(&value).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<VariableValue>(wire).unwrap(),
            value
        );
        assert_eq!(value.to_string(), display);
    }
    for value in [Value::Null, json!(1.2), json!([]), json!({})] {
        assert!(serde_json::from_value::<VariableValue>(value).is_err());
    }
}

#[test]
fn typed_template_schema_describes_reference_text() {
    use utoipa::PartialSchema;
    let schema = serde_json::to_value(Typed::<u16>::schema()).unwrap();
    assert!(schema.to_string().contains("TypedTemplate"));
    // OpenAPI follows the private schema-only type through its registered schema.
    let mut schemas = Vec::new();
    <Typed<u16> as utoipa::ToSchema>::schemas(&mut schemas);
    let schema = schemas
        .into_iter()
        .find(|(name, _)| name == "TypedTemplate")
        .unwrap()
        .1;
    assert_eq!(serde_json::to_value(schema).unwrap()["pattern"], r"\$\{\{");
}

#[test]
fn syntax_values_work_as_distinct_ordered_and_hashed_keys() {
    use std::collections::{BTreeSet, HashSet};
    // Raw spelling remains significant when using templates as cache keys.
    let templates = [
        Template::from("${{vars.a}}"),
        Template::from("${{ vars.a }}"),
        Template::from("$${{vars.a}}"),
    ];
    assert_eq!(
        templates.clone().into_iter().collect::<HashSet<_>>().len(),
        3
    );
    assert_eq!(templates.into_iter().collect::<BTreeSet<_>>().len(), 3);
    let references = SystemVariable::ALL.map(Reference::System);
    assert_eq!(
        references.clone().into_iter().collect::<HashSet<_>>().len(),
        6
    );
    assert_eq!(references.into_iter().collect::<BTreeSet<_>>().len(), 6);
    assert_eq!(
        SystemVariable::ALL
            .into_iter()
            .collect::<HashSet<_>>()
            .len(),
        6
    );
    let typed = [Typed::from(1_u16), Typed::Template("${{vars.a}}".into())];
    assert_eq!(typed.clone().into_iter().collect::<HashSet<_>>().len(), 2);
    assert_eq!(typed.into_iter().collect::<BTreeSet<_>>().len(), 2);
}
