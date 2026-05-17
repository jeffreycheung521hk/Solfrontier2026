//! Tool registry — stores and looks up tools by name.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::info;

use claw_types::tool::ToolSpec;

use crate::{errors::ToolError, tool::Tool};

/// Holds all registered tools, keyed by name.
/// Clone freely — the inner map is `Arc`-wrapped.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<HashMap<String, Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a new registry from a list of tools.
    pub fn from_tools(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut map = HashMap::new();
        for tool in tools {
            let spec = tool.spec();
            info!(name = %spec.name, "registering tool");
            map.insert(spec.name.clone(), tool);
        }
        Self {
            tools: Arc::new(map),
        }
    }

    /// Returns a reference to a tool by name.
    pub fn get(&self, name: &str) -> Result<Arc<dyn Tool>, ToolError> {
        self.tools
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::NotFound { name: name.to_string() })
    }

    /// Returns the specs of all registered tools (for LLM context injection).
    pub fn all_specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    /// Returns the names of all registered tools.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Returns a new registry with an additional tool inserted.
    /// Used to inject gateway-specific tools (e.g., `submit_for_signing`)
    /// without adding gateway dependencies to this crate.
    pub fn with_tool(self, tool: Arc<dyn Tool>) -> Self {
        let spec = tool.spec();
        let mut map: HashMap<String, Arc<dyn Tool>> = (*self.tools).clone();
        info!(name = %spec.name, "registering tool");
        map.insert(spec.name.clone(), tool);
        Self {
            tools: Arc::new(map),
        }
    }
}
