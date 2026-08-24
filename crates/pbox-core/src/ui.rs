use crate::ClusterResource;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    pub fn enabled(self, stdout_is_tty: bool, no_color: bool, json: bool) -> bool {
        if json || no_color {
            return false;
        }
        match self {
            Self::Auto => stdout_is_tty,
            Self::Always => true,
            Self::Never => false,
        }
    }
}

pub fn json<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(value)
}

pub fn format_resources(resources: &[ClusterResource], colour: bool) -> String {
    let mut output = String::from("ID             State      Node\n");
    for resource in resources {
        let state = resource.status.as_deref().unwrap_or("unknown");
        let node = resource.node.as_deref().unwrap_or("-");
        let name = resource.name.as_deref().unwrap_or("-");
        let label = if colour {
            format!("\x1b[1;36m{name:<14}\x1b[0m")
        } else {
            format!("{name:<14}")
        };
        output.push_str(&format!("{label} {state:<10} {node}\n"));
    }
    if resources.is_empty() {
        output.push_str("No pbox-managed containers found.\n");
    }
    output
}
