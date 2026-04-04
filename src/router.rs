use crate::manifest::RouteEntry;
use matchit::{Match, Router};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct RouteMetadata {
    pub handler_id: u32,
    pub dto_class: Option<String>,
}

pub struct QuillRouter {
    routers: HashMap<String, Router<RouteMetadata>>,
}

impl QuillRouter {
    pub fn new(manifest_json: &str) -> Option<Self> {
        let entries: Vec<RouteEntry> = sonic_rs::from_str(manifest_json).ok()?;

        let mut routers: HashMap<String, Router<RouteMetadata>> = HashMap::new();

        for entry in entries {
            let router = routers.entry(entry.method.clone()).or_default();
            let metadata = RouteMetadata {
                handler_id: entry.handler_id,
                dto_class: entry.dto_class,
            };
            let _ = router.insert(entry.pattern, metadata);
        }

        Some(Self { routers })
    }

    pub fn match_route<'a>(
        &'a self,
        method: &str,
        path: &'a str,
    ) -> Result<Match<'a, 'a, &'a RouteMetadata>, i32> {
        if let Some(router) = self.routers.get(method) {
            if let Ok(matched) = router.at(path) {
                return Ok(matched);
            }
        }

        // If not found, check if path exists in other methods for 405
        for (m, router) in &self.routers {
            if m != method && router.at(path).is_ok() {
                return Err(2);
            }
        }

        Err(1)
    }
}
