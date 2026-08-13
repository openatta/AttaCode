//! JSON schema for `~/.atta/code/keybindings.json`. Embedded at compile time;
//! `attacode keybindings schema` (CLI subcommand) prints it for IDE autocomplete.

pub const USER_BINDINGS_JSON_SCHEMA: &str = r##"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "title": "attacode user keybindings",
  "type": "object",
  "properties": {
    "bindings": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["shortcut", "action"],
        "properties": {
          "shortcut": {
            "type": "string",
            "description": "Single shortcut (e.g. 'Ctrl+P', 'F5', 'Esc') OR a chord (whitespace-separated, e.g. 'Ctrl+X Ctrl+C').",
            "examples": ["Ctrl+P", "Esc", "F5", "Ctrl+X Ctrl+C"]
          },
          "action": {
            "type": "string",
            "description": "Action name (consumer-defined). Common namespaces: editor.* / repl.* / ask.* / slash.*",
            "examples": [
              "editor.submit",
              "editor.history.prev",
              "repl.cancel",
              "ask.confirm"
            ]
          },
          "description": {
            "type": "string"
          }
        }
      }
    }
  }
}
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_valid_json() {
        let v: serde_json::Value = serde_json::from_str(USER_BINDINGS_JSON_SCHEMA).unwrap();
        assert!(v.get("title").is_some());
        assert!(v.get("$schema").is_some());
    }
}
