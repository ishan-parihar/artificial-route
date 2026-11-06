//! The namespace a flat tool name belongs to, recovered from a request's own
//! declarations.
//!
//! A client that groups its tools into namespaces (Codex does, for its MCP
//! servers) declares them as containers and calls them back by
//! `(namespace, name)`. No upstream in the registry speaks a namespace
//! container, so the declarations reach an upstream flattened into one flat
//! function per tool, named `namespace__name`. The reply path has to put the
//! namespace back on the `function_call` item, and it has to know which
//! namespace a flat name belongs to.
//!
//! The split is driven by the **declared** namespaces, never by parsing the
//! flat name: a name that itself contains `__` (`_web_run` under
//! `mcp__codex_apps__search_service`) is ambiguous to a parser and exact to a
//! lookup. Building the map from what the client declared is what makes the
//! round trip lossless.

use std::collections::BTreeMap;

/// The flat names a request declared under namespaces, and the namespace each
/// belongs to.
///
/// Empty for every client that declares no namespace, which is every client
/// before Codex and most after it: an empty map puts every flat name back in
/// the client's own default namespace, which is what a request with no
/// containers has always meant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolNamespaces {
    /// Flat name -> namespace, in sorted order so the map is deterministic.
    names: BTreeMap<String, String>,
}

impl ToolNamespaces {
    /// Reads the map off a request's own tool declarations.
    ///
    /// The flat name is the join [`crate::CanonicalTool::flat_name`] builds,
    /// so the map's keys are the names the upstream sees and echoes back.
    ///
    /// ```
    /// use ar_translate::{CanonicalTool, ToolNamespaces};
    ///
    /// let tool = CanonicalTool {
    ///     name: "headroom_compress".to_owned(),
    ///     description: None,
    ///     parameters: serde_json::json!({ "type": "object" }),
    ///     namespace: Some("mcp__headroom".to_owned()),
    /// };
    /// let map = ToolNamespaces::from_declarations(std::slice::from_ref(&tool));
    ///
    /// assert_eq!(
    ///     map.namespace_of("mcp__headroom__headroom_compress"),
    ///     Some("mcp__headroom")
    /// );
    /// ```
    #[must_use]
    pub fn from_declarations(tools: &[crate::CanonicalTool]) -> Self {
        let mut names = BTreeMap::new();
        for tool in tools {
            if let Some(namespace) = tool.namespace.as_deref() {
                names.insert(tool.flat_name(), namespace.to_owned());
            }
        }
        Self { names }
    }

    /// The namespace `flat_name` was declared under, when it was declared in
    /// one.
    ///
    /// `None` names the client's own default namespace, which is what every
    /// undeclared tool already resolves to.
    #[must_use]
    pub fn namespace_of(&self, flat_name: &str) -> Option<&str> {
        self.names.get(flat_name).map(String::as_str)
    }

    /// The unqualified name and namespace a flat name splits into.
    ///
    /// `None` namespace means the client resolves it in its default namespace,
    /// which is what a request that declared no containers gets for every tool.
    ///
    /// The name is borrowed for `'a` and the namespace for the map's own
    /// lifetime, so a caller can hold both without copying either.
    #[must_use]
    pub fn split<'a>(&'a self, flat_name: &'a str) -> (&'a str, Option<&'a str>) {
        match self.namespace_of(flat_name) {
            Some(namespace) => {
                let rest = flat_name
                    .strip_prefix(namespace)
                    .and_then(|rest| rest.strip_prefix("__"))
                    .unwrap_or(flat_name);
                (rest, Some(namespace))
            }
            None => (flat_name, None),
        }
    }

    /// Whether this request declared any namespaced tool.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CanonicalTool;

    fn namespaced(name: &str, namespace: &str) -> CanonicalTool {
        CanonicalTool {
            name: name.to_owned(),
            description: None,
            parameters: serde_json::json!({ "type": "object" }),
            namespace: Some(namespace.to_owned()),
        }
    }

    #[test]
    fn should_leave_a_flat_name_undeclared_when_the_request_declared_no_namespace() {
        let map = ToolNamespaces::default();
        assert_eq!(map.split("get_weather"), ("get_weather", None));
        assert!(map.is_empty());
    }

    #[test]
    fn should_split_a_flat_name_on_the_declared_namespace() {
        let map = ToolNamespaces::from_declarations(&[namespaced("compress", "mcp__headroom")]);
        assert!(!map.is_empty());
        assert_eq!(map.namespace_of("mcp__headroom__compress"), Some("mcp__headroom"));
    }

    #[test]
    fn should_split_exactly_when_the_name_itself_carries_the_separator() {
        // The load-bearing case: `_web_run` and `mcp__codex_apps` both contain
        // `__`, so parsing the flat name is ambiguous and a lookup is not.
        let map = ToolNamespaces::from_declarations(&[namespaced(
            "_web_run",
            "mcp__codex_apps__search_service",
        )]);
        let flat = "mcp__codex_apps__search_service___web_run";
        assert_eq!(
            map.split(flat),
            ("_web_run", Some("mcp__codex_apps__search_service")),
            "the split must be on the declared namespace, not on any `__`"
        );
    }

    #[test]
    fn should_ignore_a_flat_name_the_request_never_declared() {
        let map = ToolNamespaces::from_declarations(&[namespaced("compress", "mcp__headroom")]);
        assert_eq!(map.namespace_of("invented__tool"), None);
        assert_eq!(map.split("invented__tool"), ("invented__tool", None));
    }
}
