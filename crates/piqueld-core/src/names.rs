//! Validated names and shared string-newtype scaffolding.

// Validators stay with the owning type; the macro only supplies identical
// construction, wire-format, and standard-trait implementations.
macro_rules! validated_string {
    ($(#[$meta:meta])* $name:ident, $error:ident, $message:literal, $validate:expr) => {
        #[doc = concat!("Invalid `", stringify!($name), "` input.")]
        #[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
        #[error($message)]
        pub struct $error;

        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Serialize, utoipa::ToSchema)]
        $(#[$meta])*
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Parses a value while enforcing this type's invariant.
            ///
            /// # Errors
            #[doc = concat!("Returns [`", stringify!($error), "`] for invalid input: ", $message, ".")]
            pub fn parse(value: impl Into<String>) -> Result<Self, $error> {
                let value = value.into();
                if ($validate)(&value) {
                    Ok(Self(value))
                } else {
                    Err($error)
                }
            }

            /// Returns the validated wire representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = <String as serde::Deserialize>::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $error;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}
pub(crate) use validated_string;

validated_string!(
    /// User-facing logical application name.
    ApplicationName, ApplicationNameError,
    "application names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit",
    crate::resource::valid_logical_name
);
validated_string!(
    /// Logical service name, distinct from an application or volume name.
    ///
    /// ```compile_fail
    /// use piqueld_core::{ServiceName, VolumeName};
    /// let volume: VolumeName = ServiceName::parse("data").unwrap();
    /// ```
    ServiceName, ServiceNameError,
    "service names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit",
    crate::resource::valid_logical_name
);
validated_string!(
    /// Logical volume name, including references from service mounts.
    VolumeName, VolumeNameError,
    "volume names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit",
    crate::resource::valid_logical_name
);

#[cfg(test)]
mod tests {
    use super::{ApplicationName, ServiceName, VolumeName};

    #[test]
    fn logical_names_enforce_boundaries_during_parsing_and_decoding() {
        for value in ["a".into(), "a-1".into(), "a".repeat(63)] {
            let wire = serde_json::to_string(&value).unwrap();
            assert_eq!(
                serde_json::from_str::<ApplicationName>(&wire)
                    .unwrap()
                    .as_str(),
                value
            );
            assert_eq!(
                serde_json::from_str::<ServiceName>(&wire).unwrap().as_str(),
                value
            );
            assert_eq!(
                serde_json::from_str::<VolumeName>(&wire).unwrap().as_str(),
                value
            );
        }
        for value in [
            String::new(),
            "1a".into(),
            "a-".into(),
            "A".into(),
            "é".into(),
            "a".repeat(64),
        ] {
            let wire = serde_json::to_string(&value).unwrap();
            assert!(ApplicationName::parse(&value).is_err());
            assert!(ServiceName::parse(&value).is_err());
            assert!(VolumeName::parse(&value).is_err());
            assert!(serde_json::from_str::<ApplicationName>(&wire).is_err());
            assert!(serde_json::from_str::<ServiceName>(&wire).is_err());
            assert!(serde_json::from_str::<VolumeName>(&wire).is_err());
        }
    }
}
